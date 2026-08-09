use serde::Deserialize;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use worker::D1Database;

use crate::auth::{new_id, sha256_hex};
use crate::db;
use crate::error::{ApiError, ApiResult};
use crate::models::{CommandEnvelope, CommandRow};
use crate::pagination;

#[derive(Debug, Deserialize)]
struct IdOnly {
    #[serde(rename = "id")]
    _id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandValidationError {
    UnsupportedSchemaVersion,
    MissingIntent,
    MissingIdempotencyKey,
    InvalidRisk,
    InvalidConfidence,
    MissingLocale,
    MissingTimezone,
    InvalidCommandId,
    InvalidFieldLength,
    SensitiveArgument,
}

impl std::fmt::Display for CommandValidationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::UnsupportedSchemaVersion => "unsupported command schema version",
            Self::MissingIntent => "command intent is required",
            Self::MissingIdempotencyKey => "command idempotency_key is required",
            Self::InvalidRisk => "command risk_level is invalid",
            Self::InvalidConfidence => "command confidence must be between 0 and 1",
            Self::MissingLocale => "command locale is required",
            Self::MissingTimezone => "command timezone is required",
            Self::InvalidCommandId => "command_id is required",
            Self::InvalidFieldLength => "command field exceeds the maximum length",
            Self::SensitiveArgument => "command arguments cannot contain credentials or secrets",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for CommandValidationError {}

const RISKS: [&str; 4] = ["low", "medium", "high", "destructive"];
#[allow(dead_code)]
const STATES: [&str; 10] = [
    "pending",
    "validated",
    "awaiting_confirmation",
    "queued",
    "running",
    "succeeded",
    "failed",
    "expired",
    "cancelled",
    "unknown",
];

/// The backend action registry is intentionally small until each action has a
/// real executor and an explicit permission policy. Model output cannot invent
/// an executable intent.
pub fn registry_requires_confirmation(intent: &str) -> Option<bool> {
    match intent {
        "search_history" | "create_reminder" | "create_draft" => Some(false),
        "send_message" => Some(true),
        _ => None,
    }
}

/// Validate the transport-level portion of CommandEnvelope v1.
///
/// Ownership and action-registry authorization are intentionally separate
/// checks. The caller supplies the authenticated user and resolves the intent
/// against the backend registry before mutating any state.
pub fn validate_envelope(envelope: &CommandEnvelope) -> Result<(), CommandValidationError> {
    if envelope.schema_version != 1 {
        return Err(CommandValidationError::UnsupportedSchemaVersion);
    }
    if envelope.command_id.trim().is_empty() {
        return Err(CommandValidationError::InvalidCommandId);
    }
    if envelope.command_id.len() > 128
        || envelope.intent.len() > 128
        || envelope.idempotency_key.len() > 200
        || envelope.locale.len() > 32
        || envelope.timezone.len() > 64
    {
        return Err(CommandValidationError::InvalidFieldLength);
    }
    if envelope.intent.trim().is_empty() {
        return Err(CommandValidationError::MissingIntent);
    }
    if envelope.idempotency_key.trim().is_empty() {
        return Err(CommandValidationError::MissingIdempotencyKey);
    }
    if !RISKS.contains(&envelope.risk_level.as_str()) {
        return Err(CommandValidationError::InvalidRisk);
    }
    if !envelope.confidence.is_finite() || !(0.0..=1.0).contains(&envelope.confidence) {
        return Err(CommandValidationError::InvalidConfidence);
    }
    if envelope.locale.trim().is_empty() {
        return Err(CommandValidationError::MissingLocale);
    }
    if envelope.timezone.trim().is_empty() {
        return Err(CommandValidationError::MissingTimezone);
    }
    if contains_sensitive_argument(&Value::Object(envelope.args.clone())) {
        return Err(CommandValidationError::SensitiveArgument);
    }
    Ok(())
}

fn contains_sensitive_argument(value: &Value) -> bool {
    match value {
        Value::Object(object) => object.iter().any(|(key, value)| {
            let key = key.to_ascii_lowercase().replace(['-', '.'], "_");
            let sensitive = matches!(
                key.as_str(),
                "api_key"
                    | "authorization"
                    | "credential"
                    | "credentials"
                    | "password"
                    | "private_key"
                    | "refresh_token"
                    | "secret"
                    | "token"
            ) || key.ends_with("_api_key")
                || key.ends_with("_password")
                || key.ends_with("_secret")
                || key.ends_with("_token");
            sensitive || contains_sensitive_argument(value)
        }),
        Value::Array(values) => values.iter().any(contains_sensitive_argument),
        _ => false,
    }
}

/// Backend policy always wins over the model's needs_confirmation value.
pub fn requires_confirmation(envelope: &CommandEnvelope, registry_requires: bool) -> bool {
    registry_requires
        || envelope.needs_confirmation
        || matches!(envelope.risk_level.as_str(), "high" | "destructive")
}

#[allow(dead_code)]
pub fn valid_state(state: &str) -> bool {
    STATES.contains(&state)
}

#[allow(dead_code)]
pub fn valid_transition(from: Option<&str>, to: &str) -> bool {
    if !valid_state(to) {
        return false;
    }
    let Some(from) = from else {
        return to == "pending";
    };
    match from {
        "pending" => matches!(to, "validated" | "failed" | "expired" | "cancelled"),
        "validated" => matches!(
            to,
            "awaiting_confirmation" | "queued" | "failed" | "expired" | "cancelled"
        ),
        "awaiting_confirmation" => matches!(to, "queued" | "cancelled" | "expired"),
        "queued" => matches!(to, "running" | "cancelled" | "expired" | "unknown"),
        "running" => matches!(to, "succeeded" | "failed" | "unknown"),
        "unknown" => matches!(to, "running" | "succeeded" | "failed" | "expired"),
        "succeeded" | "failed" | "expired" | "cancelled" => false,
        _ => false,
    }
}

/// Hash the canonical wire representation used by confirmation tokens.
/// serde_json::Map is ordered in this build, so equivalent object keys produce
/// a stable hash across retries.
pub fn canonical_hash(envelope: &CommandEnvelope) -> Result<String, serde_json::Error> {
    let value = json!({
        "schema_version": envelope.schema_version,
        "command_id": envelope.command_id,
        "intent": envelope.intent,
        "args": envelope.args,
        "risk_level": envelope.risk_level,
        "needs_confirmation": envelope.needs_confirmation,
        "idempotency_key": envelope.idempotency_key,
        "confidence": envelope.confidence,
        "locale": envelope.locale,
        "timezone": envelope.timezone,
        "device_id": envelope.device_id,
        "session_id": envelope.session_id,
        "model_version": envelope.model_version,
    });
    let bytes = serde_json::to_vec(&value)?;
    Ok(hex_encode(Sha256::digest(bytes)))
}

fn hex_encode(bytes: impl AsRef<[u8]>) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    bytes
        .as_ref()
        .iter()
        .flat_map(|byte| {
            [
                HEX[(byte >> 4) as usize] as char,
                HEX[(byte & 0x0f) as usize] as char,
            ]
        })
        .collect()
}

/// Keep this helper next to the validator so route handlers cannot accidentally
/// trust a client-supplied user ID when a future envelope adds one.
#[allow(dead_code)]
pub fn require_owner(authenticated_user_id: &str, resource_user_id: &str) -> bool {
    !authenticated_user_id.is_empty() && authenticated_user_id == resource_user_id
}

pub fn normalized_args(args: &Map<String, Value>) -> Map<String, Value> {
    args.clone()
}

fn command_select() -> &'static str {
    "SELECT id, user_id, device_id, session_id, schema_version, intent, args_json, risk_level, needs_confirmation, idempotency_key, confidence, locale, timezone, state, command_hash, result_json, error_code, expires_at, model_version, version, created_at, updated_at FROM commands"
}

pub async fn get_for_user(
    db: &D1Database,
    user_id: &str,
    command_id: &str,
) -> ApiResult<Option<CommandRow>> {
    db::first(
        db,
        &format!("{} WHERE id = ? AND user_id = ?", command_select()),
        vec![db::text(command_id), db::text(user_id)],
    )
    .await
}

fn result_value(row: &CommandRow) -> Value {
    row.result_json
        .as_deref()
        .and_then(|value| serde_json::from_str::<Value>(value).ok())
        .unwrap_or(Value::Null)
}

fn error_value(row: &CommandRow) -> Value {
    row.error_code
        .as_ref()
        .map(|code| {
            json!({
                "code": code,
                "message": code,
                "retryable": row.state == "unknown",
            })
        })
        .unwrap_or(Value::Null)
}

/// A command list is intentionally a summary. In particular, it never emits
/// the one-time confirmation token and does not repeat the full argument
/// object for every row.
pub fn summary(row: &CommandRow) -> Value {
    json!({
        "command_id": row.id,
        "session_id": row.session_id,
        "intent": row.intent,
        "risk_level": row.risk_level,
        "needs_confirmation": row.needs_confirmation != 0,
        "state": row.state,
        "result": result_value(row),
        "error": error_value(row),
        "expires_at": row.expires_at,
        "version": row.version,
        "created_at": row.created_at,
        "updated_at": row.updated_at,
    })
}

pub async fn list_for_user(
    db: &D1Database,
    user_id: &str,
    before: Option<&str>,
    state: Option<&str>,
    session_id: Option<&str>,
    limit: i32,
) -> ApiResult<Value> {
    if let Some(state) = state {
        if !valid_state(state) {
            return Err(ApiError::validation("Invalid command state"));
        }
    }
    if let Some(session_id) = session_id {
        if session_id.trim().is_empty() || session_id.len() > 128 {
            return Err(ApiError::validation("Invalid session_id"));
        }
    }

    let cursor = pagination::decode(before)?;
    let safe_limit = limit.clamp(1, 50) as i64;
    let mut sql = format!("{} WHERE user_id = ?", command_select());
    let mut values = vec![db::text(user_id)];
    if let Some(cursor) = cursor {
        sql.push_str(" AND (updated_at < ? OR (updated_at = ? AND id < ?))");
        values.push(db::text(&cursor.sort_key));
        values.push(db::text(&cursor.sort_key));
        values.push(db::text(&cursor.id));
    }
    if let Some(state) = state {
        sql.push_str(" AND state = ?");
        values.push(db::text(state));
    }
    if let Some(session_id) = session_id {
        sql.push_str(" AND session_id = ?");
        values.push(db::text(session_id));
    }
    sql.push_str(" ORDER BY updated_at DESC, id DESC LIMIT ?");
    values.push(db::number(safe_limit + 1));

    let rows: Vec<CommandRow> = db::all(db, &sql, values).await?;
    let has_more = rows.len() as i64 > safe_limit;
    let rows = rows
        .into_iter()
        .take(safe_limit as usize)
        .collect::<Vec<_>>();
    let next_cursor = has_more
        .then(|| {
            rows.last()
                .map(|row| pagination::encode(&row.updated_at, &row.id))
        })
        .flatten();
    Ok(json!({
        "commands": rows.iter().map(summary).collect::<Vec<_>>(),
        "next_cursor": next_cursor,
        "has_more": has_more,
    }))
}

fn envelope_from_row(row: &CommandRow) -> CommandEnvelope {
    let args = serde_json::from_str::<Value>(&row.args_json)
        .ok()
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    CommandEnvelope {
        schema_version: row.schema_version,
        command_id: row.id.clone(),
        intent: row.intent.clone(),
        args,
        risk_level: row.risk_level.clone(),
        needs_confirmation: row.needs_confirmation != 0,
        idempotency_key: row.idempotency_key.clone(),
        confidence: row.confidence.unwrap_or(0.0),
        locale: row.locale.clone(),
        timezone: row.timezone.clone(),
        device_id: row.device_id.clone(),
        session_id: row.session_id.clone(),
        model_version: row.model_version.clone(),
    }
}

pub fn response(row: &CommandRow, confirmation_token: Option<&str>) -> Value {
    json!({
        "command_id": row.id,
        "state": row.state,
        "command": envelope_from_row(row),
        "confirmation_token": confirmation_token,
        "result": result_value(row),
        "error": error_value(row),
        "undo_command_id": Value::Null,
        "version": row.version,
        "created_at": row.created_at,
        "updated_at": row.updated_at,
    })
}

fn validation_error(error: CommandValidationError) -> ApiError {
    ApiError::validation(error.to_string())
}

fn registry_error(intent: &str) -> ApiError {
    ApiError::new(
        422,
        "unsupported_intent",
        format!("Intent is not registered: {intent}"),
    )
}

async fn validate_scope(
    db: &D1Database,
    user_id: &str,
    envelope: &CommandEnvelope,
) -> ApiResult<()> {
    if let Some(session_id) = envelope.session_id.as_deref() {
        ensure_session_live(db, user_id, session_id).await?;
    }
    if let Some(device_id) = envelope.device_id.as_deref() {
        if db::first::<IdOnly>(
            db,
            "SELECT id FROM devices WHERE id = ? AND user_id = ?",
            vec![db::text(device_id), db::text(user_id)],
        )
        .await?
        .is_none()
        {
            return Err(ApiError::not_found("Device not found"));
        }
    }
    Ok(())
}

pub async fn ensure_session_live(
    db: &D1Database,
    user_id: &str,
    session_id: &str,
) -> ApiResult<()> {
    if db::first::<IdOnly>(
        db,
        "SELECT id FROM sessions WHERE id = ? AND user_id = ? AND deleted_at IS NULL",
        vec![db::text(session_id), db::text(user_id)],
    )
    .await?
    .is_none()
    {
        return Err(ApiError::not_found("Session not found"));
    }
    Ok(())
}

pub async fn create(db: &D1Database, user_id: &str, envelope: CommandEnvelope) -> ApiResult<Value> {
    validate_envelope(&envelope).map_err(validation_error)?;
    let registry_requires = registry_requires_confirmation(&envelope.intent)
        .ok_or_else(|| registry_error(&envelope.intent))?;
    validate_scope(db, user_id, &envelope).await?;
    let command_hash = canonical_hash(&envelope)?;

    if let Some(existing) = db::first::<CommandRow>(
        db,
        &format!(
            "{} WHERE user_id = ? AND idempotency_key = ?",
            command_select()
        ),
        vec![db::text(user_id), db::text(&envelope.idempotency_key)],
    )
    .await?
    {
        if existing.command_hash != command_hash {
            return Err(ApiError::conflict(
                "idempotency_key was already used for a different command",
            ));
        }
        return Ok(response(&existing, None));
    }

    let command_id = envelope.command_id.clone();
    let now = db::now_iso();
    let expires_at = db::add_seconds_iso(900);
    let confirmation_required = requires_confirmation(&envelope, registry_requires);
    let final_version = 2_i64;
    let mut statements = vec![
        db::prepare(
            db,
            "INSERT INTO commands (id, user_id, device_id, session_id, schema_version, intent, args_json, risk_level, needs_confirmation, idempotency_key, confidence, locale, timezone, state, command_hash, expires_at, model_version, version, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'pending', ?, ?, ?, 0, ?, ?)",
            vec![
                db::text(&command_id),
                db::text(user_id),
                db::optional_text(envelope.device_id.as_deref()),
                db::optional_text(envelope.session_id.as_deref()),
                db::number(envelope.schema_version as i64),
                db::text(&envelope.intent),
                db::text(&Value::Object(normalized_args(&envelope.args)).to_string()),
                db::text(&envelope.risk_level),
                db::bool_number(envelope.needs_confirmation || confirmation_required),
                db::text(&envelope.idempotency_key),
                db::decimal(envelope.confidence),
                db::text(&envelope.locale),
                db::text(&envelope.timezone),
                db::text(&command_hash),
                db::text(&expires_at),
                db::optional_text(envelope.model_version.as_deref()),
                db::text(&now),
                db::text(&now),
            ],
        )?,
        db::prepare(
            db,
            "UPDATE commands SET state = 'validated', version = 1, updated_at = ? WHERE id = ? AND user_id = ? AND state = 'pending'",
            vec![db::text(&now), db::text(&command_id), db::text(user_id)],
        )?,
    ];

    let token = if confirmation_required {
        let token = new_id("ctok")?;
        let token_hash = sha256_hex(&token);
        let token_id = new_id("cont")?;
        let token_expires_at = db::add_seconds_iso(600);
        statements.push(db::prepare(
            db,
            "UPDATE commands SET state = 'awaiting_confirmation', version = 2, updated_at = ? WHERE id = ? AND user_id = ? AND state = 'validated'",
            vec![db::text(&now), db::text(&command_id), db::text(user_id)],
        )?);
        statements.push(db::prepare(
            db,
            "INSERT INTO confirmation_tokens (id, command_id, user_id, token_hash, command_hash, expires_at, created_at) VALUES (?, ?, ?, ?, ?, ?, ?)",
            vec![
                db::text(&token_id),
                db::text(&command_id),
                db::text(user_id),
                db::text(&token_hash),
                db::text(&command_hash),
                db::text(&token_expires_at),
                db::text(&now),
            ],
        )?);
        Some(token)
    } else {
        statements.push(db::prepare(
            db,
            "UPDATE commands SET state = 'queued', version = 2, updated_at = ? WHERE id = ? AND user_id = ? AND state = 'validated'",
            vec![db::text(&now), db::text(&command_id), db::text(user_id)],
        )?);
        statements.push(db::prepare(
            db,
            "INSERT INTO outbox_events (id, user_id, topic, aggregate_id, payload_json, idempotency_key, state, created_at, updated_at) VALUES (?, ?, 'command.execute', ?, ?, ?, 'queued', ?, ?)",
            vec![
                db::text(&new_id("out")?),
                db::text(user_id),
                db::text(&command_id),
                db::text(&json!({"command_id": command_id}).to_string()),
                db::text(&envelope.idempotency_key),
                db::text(&now),
                db::text(&now),
            ],
        )?);
        None
    };

    statements.push(db::prepare(
        db,
        "INSERT INTO audit_logs (id, user_id, session_id, action, metadata_json, created_at) VALUES (?, ?, ?, 'command.create', ?, ?)",
        vec![
            db::text(&new_id("aud")?),
            db::text(user_id),
            db::optional_text(envelope.session_id.as_deref()),
            db::text(&json!({
                "command_id": command_id,
                "intent": envelope.intent,
                "state": if confirmation_required { "awaiting_confirmation" } else { "queued" },
            }).to_string()),
            db::text(&now),
        ],
    )?);

    statements.push(db::prepare(
        db,
        "INSERT INTO phone_changes (user_id, entity_type, entity_id, session_id, version, created_at) VALUES (?, 'command', ?, ?, ?, ?)",
        vec![
            db::text(user_id),
            db::text(&command_id),
            db::optional_text(envelope.session_id.as_deref()),
            db::number(final_version),
            db::text(&now),
        ],
    )?);

    if let Err(error) = db.batch(statements).await {
        if let Some(existing) = db::first::<CommandRow>(
            db,
            &format!(
                "{} WHERE user_id = ? AND idempotency_key = ?",
                command_select()
            ),
            vec![db::text(user_id), db::text(&envelope.idempotency_key)],
        )
        .await?
        {
            if existing.command_hash == command_hash {
                return Ok(response(&existing, None));
            }
        }
        return Err(error.into());
    }

    let created = get_for_user(db, user_id, &command_id)
        .await?
        .ok_or_else(|| ApiError::new(500, "command_error", "Command insert failed"))?;
    Ok(response(&created, token.as_deref()))
}

pub async fn confirm(
    db: &D1Database,
    user_id: &str,
    command_id: &str,
    token: &str,
) -> ApiResult<Value> {
    if token.trim().is_empty() {
        return Err(ApiError::validation("confirmation_token is required"));
    }
    let command = get_for_user(db, user_id, command_id)
        .await?
        .ok_or_else(|| ApiError::not_found("Command not found"))?;
    if let Some(session_id) = command.session_id.as_deref() {
        ensure_session_live(db, user_id, session_id).await?;
    }
    if command.state != "awaiting_confirmation" {
        return Err(ApiError::conflict("Command is not awaiting confirmation"));
    }
    let token_hash = sha256_hex(token);
    let token_row = db::first::<crate::models::ConfirmationTokenRow>(
        db,
        "SELECT id, command_id, user_id, token_hash, command_hash, expires_at, used_at, created_at FROM confirmation_tokens WHERE command_id = ? AND user_id = ? AND token_hash = ?",
        vec![db::text(command_id), db::text(user_id), db::text(&token_hash)],
    )
    .await?
    .ok_or_else(|| ApiError::unauthorized("Invalid confirmation token"))?;
    if token_row.used_at.is_some() {
        return Err(ApiError::conflict("Confirmation token was already used"));
    }
    if db::is_expired(&token_row.expires_at)
        || db::is_expired(command.expires_at.as_deref().unwrap_or_default())
    {
        return Err(ApiError::gone("Confirmation token expired"));
    }
    if token_row.command_hash != command.command_hash {
        return Err(ApiError::unauthorized(
            "Confirmation token does not match command",
        ));
    }

    let now = db::now_iso();
    let next_version = command.version + 1;
    let outbox_id = new_id("out")?;
    let mut statements = vec![
        db::prepare(
            db,
            "UPDATE confirmation_tokens SET used_at = ? WHERE id = ? AND user_id = ? AND command_hash = ? AND used_at IS NULL AND expires_at > ? AND EXISTS (SELECT 1 FROM commands WHERE id = ? AND user_id = ? AND state = 'awaiting_confirmation' AND (session_id IS NULL OR EXISTS (SELECT 1 FROM sessions WHERE id = commands.session_id AND user_id = ? AND deleted_at IS NULL)))",
            vec![
                db::text(&now),
                db::text(&token_row.id),
                db::text(user_id),
                db::text(&command.command_hash),
                db::text(&now),
                db::text(command_id),
                db::text(user_id),
                db::text(user_id),
            ],
        )?,
        db::prepare(
            db,
            "INSERT INTO audit_logs (id, user_id, session_id, action, metadata_json, created_at) SELECT ?, ?, ?, 'command.confirm', ?, ? WHERE EXISTS (SELECT 1 FROM commands WHERE id = ? AND user_id = ? AND state = 'awaiting_confirmation' AND (session_id IS NULL OR EXISTS (SELECT 1 FROM sessions WHERE id = commands.session_id AND user_id = ? AND deleted_at IS NULL)))",
            vec![
                db::text(&new_id("aud")?),
                db::text(user_id),
                db::optional_text(command.session_id.as_deref()),
                db::text(&json!({"command_id": command_id}).to_string()),
                db::text(&now),
                db::text(command_id),
                db::text(user_id),
                db::text(user_id),
            ],
        )?,
        db::prepare(
            db,
            "UPDATE commands SET state = 'queued', version = ?, updated_at = ? WHERE id = ? AND user_id = ? AND state = 'awaiting_confirmation' AND (session_id IS NULL OR EXISTS (SELECT 1 FROM sessions WHERE id = commands.session_id AND user_id = ? AND deleted_at IS NULL))",
            vec![
                db::number(next_version),
                db::text(&now),
                db::text(command_id),
                db::text(user_id),
                db::text(user_id),
            ],
        )?,
        db::prepare(
            db,
            "INSERT INTO outbox_events (id, user_id, topic, aggregate_id, payload_json, idempotency_key, state, created_at, updated_at) SELECT ?, ?, 'command.execute', ?, ?, ?, 'queued', ?, ? WHERE EXISTS (SELECT 1 FROM commands WHERE id = ? AND user_id = ? AND state = 'queued' AND version = ?)",
            vec![
                db::text(&outbox_id),
                db::text(user_id),
                db::text(command_id),
                db::text(&json!({"command_id": command_id}).to_string()),
                db::text(&format!("confirm:{command_id}")),
                db::text(&now),
                db::text(&now),
                db::text(command_id),
                db::text(user_id),
                db::number(next_version),
            ],
        )?,
        db::prepare(
            db,
            "INSERT INTO phone_changes (user_id, entity_type, entity_id, session_id, version, created_at) SELECT ?, 'command', ?, ?, ?, ? WHERE EXISTS (SELECT 1 FROM commands WHERE id = ? AND user_id = ? AND state = 'queued' AND version = ?)",
            vec![
                db::text(user_id),
                db::text(command_id),
                db::optional_text(command.session_id.as_deref()),
                db::number(next_version),
                db::text(&now),
                db::text(command_id),
                db::text(user_id),
                db::number(next_version),
            ],
        )?,
    ];
    if let Err(error) = db.batch(std::mem::take(&mut statements)).await {
        let current = get_for_user(db, user_id, command_id).await?;
        if current.as_ref().is_some_and(|row| row.state == "queued") {
            return Err(ApiError::conflict("Command was already confirmed"));
        }
        return Err(error.into());
    }
    let updated = get_for_user(db, user_id, command_id)
        .await?
        .ok_or_else(|| ApiError::new(500, "command_error", "Command disappeared"))?;
    Ok(response(&updated, None))
}

pub async fn cancel(db: &D1Database, user_id: &str, command_id: &str) -> ApiResult<Value> {
    let command = get_for_user(db, user_id, command_id)
        .await?
        .ok_or_else(|| ApiError::not_found("Command not found"))?;
    if let Some(session_id) = command.session_id.as_deref() {
        ensure_session_live(db, user_id, session_id).await?;
    }
    if !matches!(
        command.state.as_str(),
        "pending" | "validated" | "awaiting_confirmation" | "queued"
    ) {
        return Err(ApiError::conflict(
            "Command cannot be cancelled in its current state",
        ));
    }
    let now = db::now_iso();
    let next_version = command.version + 1;
    let statements = vec![
        db::prepare(
            db,
            "UPDATE commands SET state = 'cancelled', version = ?, updated_at = ? WHERE id = ? AND user_id = ? AND state IN ('pending', 'validated', 'awaiting_confirmation', 'queued')",
            vec![
                db::number(next_version),
                db::text(&now),
                db::text(command_id),
                db::text(user_id),
            ],
        )?,
        db::prepare(
            db,
            "UPDATE outbox_events SET state = 'failed', last_error = 'command_cancelled', updated_at = ? WHERE aggregate_id = ? AND user_id = ? AND state IN ('queued', 'retrying')",
            vec![db::text(&now), db::text(command_id), db::text(user_id)],
        )?,
        db::prepare(
            db,
            "INSERT INTO audit_logs (id, user_id, session_id, action, metadata_json, created_at) VALUES (?, ?, ?, 'command.cancel', ?, ?)",
            vec![
                db::text(&new_id("aud")?),
                db::text(user_id),
                db::optional_text(command.session_id.as_deref()),
                db::text(&json!({"command_id": command_id}).to_string()),
                db::text(&now),
            ],
        )?,
        db::prepare(
            db,
            "INSERT INTO phone_changes (user_id, entity_type, entity_id, session_id, version, created_at) VALUES (?, 'command', ?, ?, ?, ?)",
            vec![
                db::text(user_id),
                db::text(command_id),
                db::optional_text(command.session_id.as_deref()),
                db::number(next_version),
                db::text(&now),
            ],
        )?,
    ];
    if let Err(error) = db.batch(statements).await {
        return Err(error.into());
    }
    let updated = get_for_user(db, user_id, command_id)
        .await?
        .ok_or_else(|| ApiError::new(500, "command_error", "Command disappeared"))?;
    Ok(response(&updated, None))
}

pub async fn undo(db: &D1Database, user_id: &str, command_id: &str) -> ApiResult<Value> {
    let command = get_for_user(db, user_id, command_id)
        .await?
        .ok_or_else(|| ApiError::not_found("Command not found"))?;
    if let Some(session_id) = command.session_id.as_deref() {
        ensure_session_live(db, user_id, session_id).await?;
    }
    if command.state != "succeeded"
        || !matches!(command.intent.as_str(), "create_reminder" | "create_draft")
    {
        return Err(ApiError::conflict("Command is not currently undoable"));
    }
    let undo = crate::action_effects::undo(db, user_id, &command).await?;
    let updated = get_for_user(db, user_id, command_id)
        .await?
        .ok_or_else(|| ApiError::new(500, "command_error", "Command disappeared"))?;
    let mut payload = response(&updated, None);
    if let Some(object) = payload.as_object_mut() {
        object.insert("undo_result".to_string(), undo);
    }
    Ok(payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envelope() -> CommandEnvelope {
        CommandEnvelope {
            schema_version: 1,
            command_id: "cmd_test".to_string(),
            intent: "search_history".to_string(),
            args: Map::new(),
            risk_level: "low".to_string(),
            needs_confirmation: false,
            idempotency_key: "idem_test".to_string(),
            confidence: 0.9,
            locale: "zh-Hans-HK".to_string(),
            timezone: "Asia/Hong_Kong".to_string(),
            device_id: None,
            session_id: None,
            model_version: Some("test".to_string()),
        }
    }

    #[test]
    fn validates_v1_envelope_and_rejects_invalid_confidence() {
        let mut valid = envelope();
        assert!(validate_envelope(&valid).is_ok());
        valid.confidence = 2.0;
        assert_eq!(
            validate_envelope(&valid),
            Err(CommandValidationError::InvalidConfidence)
        );
    }

    #[test]
    fn backend_risk_policy_overrides_model_flag() {
        let mut low = envelope();
        assert!(!requires_confirmation(&low, false));
        low.risk_level = "destructive".to_string();
        assert!(requires_confirmation(&low, false));
        assert!(requires_confirmation(&envelope(), true));
    }

    #[test]
    fn registry_rejects_unimplemented_intents() {
        assert_eq!(
            registry_requires_confirmation("search_history"),
            Some(false)
        );
        assert_eq!(registry_requires_confirmation("send_message"), Some(true));
        assert_eq!(registry_requires_confirmation("run_arbitrary_code"), None);
    }

    #[test]
    fn state_transitions_are_terminal_and_ordered() {
        assert!(valid_transition(None, "pending"));
        assert!(valid_transition(Some("pending"), "validated"));
        assert!(valid_transition(Some("validated"), "queued"));
        assert!(valid_transition(Some("running"), "unknown"));
        assert!(valid_transition(Some("unknown"), "succeeded"));
        assert!(!valid_transition(Some("succeeded"), "running"));
        assert!(!valid_transition(Some("pending"), "running"));
    }

    #[test]
    fn canonical_hash_is_stable_and_changes_with_arguments() {
        let first = envelope();
        let mut second = first.clone();
        second
            .args
            .insert("q".to_string(), Value::String("rust".to_string()));
        assert_eq!(
            canonical_hash(&first).unwrap(),
            canonical_hash(&first).unwrap()
        );
        assert_ne!(
            canonical_hash(&first).unwrap(),
            canonical_hash(&second).unwrap()
        );
    }

    #[test]
    fn command_arguments_cannot_carry_provider_credentials() {
        let mut value = envelope();
        value.args.insert(
            "provider".to_string(),
            json!({"api_key": "secret", "recipient": "user@example.com"}),
        );
        assert_eq!(
            validate_envelope(&value),
            Err(CommandValidationError::SensitiveArgument)
        );
    }

    #[test]
    fn ownership_is_exact_and_non_empty() {
        assert!(require_owner("user_1", "user_1"));
        assert!(!require_owner("user_1", "user_2"));
        assert!(!require_owner("", ""));
    }
}
