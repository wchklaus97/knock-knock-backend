use serde::Deserialize;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use worker::{D1Database, Env};

use crate::auth::{new_id, sha256_hex};
use crate::db;
use crate::error::{ApiError, ApiResult};
use crate::models::{CommandEnvelope, CommandRow};
use crate::pagination;
use crate::providers::{self, ActionProviderConfig};

#[derive(Debug, Deserialize)]
struct IdOnly {
    #[serde(rename = "id")]
    _id: String,
}

#[derive(Debug, Deserialize)]
struct CommandScopeRow {
    id: String,
    user_id: String,
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
    EnvelopeTooLarge,
    SensitiveArgument,
    InvalidActionArguments,
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
            Self::EnvelopeTooLarge => "command envelope exceeds the maximum size",
            Self::SensitiveArgument => "command arguments cannot contain credentials or secrets",
            Self::InvalidActionArguments => "command arguments do not match the registered action",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for CommandValidationError {}

const RISKS: [&str; 4] = ["low", "medium", "high", "destructive"];
const MAX_ENVELOPE_BYTES: usize = 64 * 1024;
const MAX_ARGUMENT_BYTES: usize = 48 * 1024;
const MAX_MODEL_VERSION: usize = 128;
const MAX_CONFIRMATION_TOKEN: usize = 256;
const COMMAND_TTL_SECONDS: i64 = 900;
const CONFIRMATION_TTL_SECONDS: i64 = 600;
const UNDO_WINDOW_SECONDS: f64 = 600.0;
const MAX_ACTION_TITLE: usize = 200;
const MAX_ACTION_RECIPIENT: usize = 320;
const MAX_ACTION_BODY: usize = 8_000;
const MAX_ACTION_DUE_AT: usize = 64;
#[allow(dead_code)]
const STATES: [&str; 11] = [
    "pending",
    "validated",
    "awaiting_confirmation",
    "queued",
    "running",
    "succeeded",
    "failed",
    "expired",
    "cancelled",
    "retryable",
    "unknown",
];

/// The backend action registry is the only authority for command risk and
/// confirmation. The model may suggest a risk, but it cannot raise or lower
/// the policy that is persisted and shown to the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActionPolicy {
    pub risk_level: &'static str,
    pub requires_confirmation: bool,
    pub reversible: bool,
    pub title: &'static str,
}

pub fn registry_policy(intent: &str) -> Option<ActionPolicy> {
    match intent {
        "search_history" => Some(ActionPolicy {
            risk_level: "low",
            requires_confirmation: false,
            reversible: false,
            title: "Search history",
        }),
        "create_reminder" => Some(ActionPolicy {
            risk_level: "low",
            requires_confirmation: false,
            reversible: true,
            title: "Create reminder",
        }),
        "create_draft" => Some(ActionPolicy {
            risk_level: "low",
            requires_confirmation: false,
            reversible: true,
            title: "Create draft",
        }),
        "send_message" => Some(ActionPolicy {
            risk_level: "high",
            requires_confirmation: true,
            reversible: false,
            title: "Send message",
        }),
        _ => None,
    }
}

pub fn registry_requires_confirmation(intent: &str) -> Option<bool> {
    registry_policy(intent).map(|policy| policy.requires_confirmation)
}

/// One semantic string field in an action's fail-closed argument contract.
/// `canonical` is what new clients should emit; aliases are compatibility-only.
#[derive(Debug, Clone, Copy)]
struct ActionStringField {
    canonical: &'static str,
    compatibility_aliases: &'static [&'static str],
    required: bool,
    min_length: usize,
    max_length: usize,
}

impl ActionStringField {
    fn keys(&self) -> impl Iterator<Item = &'static str> + '_ {
        std::iter::once(self.canonical).chain(self.compatibility_aliases.iter().copied())
    }

    fn accepts_key(&self, key: &str) -> bool {
        self.keys().any(|candidate| candidate == key)
    }

    fn validates(&self, args: &Map<String, Value>) -> bool {
        let mut present = self.keys().filter_map(|key| args.get(key));
        let Some(value) = present.next() else {
            return !self.required;
        };

        // More than one name for the same semantic field is ambiguous even
        // when the values happen to be equal.
        if present.next().is_some() {
            return false;
        }

        let Some(value) = value.as_str() else {
            return false;
        };
        let length = value.trim().chars().count();
        length >= self.min_length && length <= self.max_length
    }
}

// Exact CommandEnvelope v1 argument contracts. Compatibility aliases remain
// accepted one-at-a-time, but canonical names are the only names new clients
// should produce.
const SEARCH_HISTORY_FIELDS: &[ActionStringField] = &[ActionStringField {
    canonical: "q",
    compatibility_aliases: &["query", "text"],
    required: true,
    min_length: 1,
    max_length: crate::history::SEARCH_QUERY_MAX_CHARACTERS,
}];
const CREATE_REMINDER_FIELDS: &[ActionStringField] = &[
    ActionStringField {
        canonical: "title",
        compatibility_aliases: &["text", "message"],
        required: true,
        min_length: 1,
        max_length: MAX_ACTION_TITLE,
    },
    ActionStringField {
        canonical: "due_at",
        compatibility_aliases: &["time", "datetime"],
        required: true,
        min_length: 1,
        max_length: MAX_ACTION_DUE_AT,
    },
];
const CREATE_DRAFT_FIELDS: &[ActionStringField] = &[
    ActionStringField {
        canonical: "body",
        compatibility_aliases: &["content", "text"],
        required: true,
        min_length: 1,
        max_length: MAX_ACTION_BODY,
    },
    ActionStringField {
        canonical: "title",
        compatibility_aliases: &["subject"],
        required: false,
        min_length: 0,
        max_length: MAX_ACTION_TITLE,
    },
    ActionStringField {
        canonical: "recipient",
        compatibility_aliases: &["to"],
        required: false,
        min_length: 0,
        max_length: MAX_ACTION_RECIPIENT,
    },
];
const SEND_MESSAGE_FIELDS: &[ActionStringField] = &[
    ActionStringField {
        canonical: "body",
        compatibility_aliases: &["message", "text"],
        required: true,
        min_length: 1,
        max_length: MAX_ACTION_BODY,
    },
    ActionStringField {
        canonical: "recipient",
        compatibility_aliases: &["to", "email", "phone"],
        required: true,
        min_length: 1,
        max_length: MAX_ACTION_RECIPIENT,
    },
];

fn action_argument_fields(intent: &str) -> Option<&'static [ActionStringField]> {
    match intent {
        "search_history" => Some(SEARCH_HISTORY_FIELDS),
        "create_reminder" => Some(CREATE_REMINDER_FIELDS),
        "create_draft" => Some(CREATE_DRAFT_FIELDS),
        "send_message" => Some(SEND_MESSAGE_FIELDS),
        _ => None,
    }
}

/// Validate the argument shape for a registered action before it can enter
/// the durable queue. The executor repeats this check immediately before an
/// effect, so legacy or manually repaired rows cannot bypass the same rules.
pub fn validate_action_args(
    intent: &str,
    args: &Map<String, Value>,
) -> Result<(), CommandValidationError> {
    let fields =
        action_argument_fields(intent).ok_or(CommandValidationError::InvalidActionArguments)?;
    let valid = args
        .keys()
        .all(|key| fields.iter().any(|field| field.accepts_key(key)))
        && fields.iter().all(|field| field.validates(args));
    valid
        .then_some(())
        .ok_or(CommandValidationError::InvalidActionArguments)
}

/// Check the persisted policy again at the execution boundary. This is a
/// defense against stale or manually repaired rows, where create-time policy
/// enforcement is no longer enough to protect a side effect.
pub fn execution_policy_matches(intent: &str, risk_level: &str, needs_confirmation: bool) -> bool {
    registry_policy(intent).is_some_and(|policy| {
        policy.risk_level == risk_level && policy.requires_confirmation == needs_confirmation
    })
}

fn action_metadata(intent: &str) -> Value {
    registry_policy(intent)
        .map(|policy| {
            json!({
                "title": policy.title,
                "risk": policy.risk_level,
                "confirm_required": policy.requires_confirmation,
                "reversible": policy.reversible,
            })
        })
        .unwrap_or(Value::Null)
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
    if envelope.command_id.chars().count() > 128
        || envelope.intent.chars().count() > 128
        || envelope.idempotency_key.chars().count() > 200
        || envelope.locale.chars().count() > 32
        || envelope.timezone.chars().count() > 64
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
    if envelope.locale.chars().count() < 2 {
        return Err(CommandValidationError::InvalidFieldLength);
    }
    if envelope.timezone.trim().is_empty() {
        return Err(CommandValidationError::MissingTimezone);
    }
    for (field, max_length) in [
        (envelope.device_id.as_deref(), 128),
        (envelope.session_id.as_deref(), 128),
        (envelope.model_version.as_deref(), MAX_MODEL_VERSION),
    ] {
        if let Some(field) = field {
            if field.trim().is_empty() || field.chars().count() > max_length {
                return Err(CommandValidationError::InvalidFieldLength);
            }
        }
    }
    let args_bytes =
        serde_json::to_vec(&envelope.args).map_err(|_| CommandValidationError::EnvelopeTooLarge)?;
    if args_bytes.len() > MAX_ARGUMENT_BYTES {
        return Err(CommandValidationError::EnvelopeTooLarge);
    }
    let envelope_bytes =
        serde_json::to_vec(envelope).map_err(|_| CommandValidationError::EnvelopeTooLarge)?;
    if envelope_bytes.len() > MAX_ENVELOPE_BYTES {
        return Err(CommandValidationError::EnvelopeTooLarge);
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

/// Backend policy always wins over the model's needs_confirmation and
/// risk_level values. Keeping the envelope parameter makes the call site
/// explicit and leaves room for future policy checks without reintroducing
/// client authority.
pub fn requires_confirmation(_envelope: &CommandEnvelope, registry_requires: bool) -> bool {
    registry_requires
}

fn authoritative_envelope(envelope: &CommandEnvelope, policy: ActionPolicy) -> CommandEnvelope {
    let mut authoritative = envelope.clone();
    authoritative.risk_level = policy.risk_level.to_string();
    authoritative.needs_confirmation = policy.requires_confirmation;
    authoritative
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
        "pending" => matches!(
            to,
            "validated" | "failed" | "expired" | "cancelled" | "retryable"
        ),
        "validated" => matches!(
            to,
            "awaiting_confirmation" | "queued" | "failed" | "expired" | "cancelled" | "retryable"
        ),
        "awaiting_confirmation" => matches!(to, "queued" | "cancelled" | "expired"),
        "queued" => matches!(
            to,
            "running" | "cancelled" | "expired" | "retryable" | "unknown"
        ),
        "running" => matches!(to, "succeeded" | "failed" | "retryable" | "unknown"),
        "retryable" => matches!(
            to,
            "running" | "retryable" | "failed" | "expired" | "cancelled" | "unknown"
        ),
        "unknown" => matches!(to, "running" | "succeeded" | "failed" | "expired"),
        "succeeded" | "failed" | "expired" | "cancelled" => false,
        _ => false,
    }
}

/// Hash the canonical wire representation used by confirmation tokens.
/// Object keys are sorted recursively so equivalent JSON objects produce the
/// same hash even if serde_json is later configured to preserve insertion
/// order.
pub fn canonical_hash(envelope: &CommandEnvelope) -> Result<String, serde_json::Error> {
    let value = canonicalize_value(json!({
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
    }));
    let bytes = serde_json::to_vec(&value)?;
    Ok(hex_encode(Sha256::digest(bytes)))
}

fn canonicalize_value(value: Value) -> Value {
    match value {
        Value::Object(object) => {
            let mut entries = object.into_iter().collect::<Vec<_>>();
            entries.sort_by(|left, right| left.0.cmp(&right.0));
            let mut canonical = Map::new();
            for (key, value) in entries {
                canonical.insert(key, canonicalize_value(value));
            }
            Value::Object(canonical)
        }
        Value::Array(values) => Value::Array(values.into_iter().map(canonicalize_value).collect()),
        other => other,
    }
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
    canonicalize_value(Value::Object(args.clone()))
        .as_object()
        .cloned()
        .unwrap_or_default()
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
                "retryable": matches!(row.state.as_str(), "retryable" | "unknown"),
            })
        })
        .unwrap_or(Value::Null)
}

/// Build the only command text intended for direct UI/TTS presentation.
/// It deliberately never interpolates command arguments, search results,
/// recipients, message bodies, provider IDs, URLs, or raw provider errors.
fn presentation_value(row: &CommandRow) -> Value {
    let result = result_value(row);
    let result_object = result.as_object();
    let was_undone = result_object.is_some_and(|value| value.contains_key("undo"));
    let result_kind = result_object
        .and_then(|value| value.get("kind"))
        .and_then(Value::as_str);
    let external_delivery = result_object
        .and_then(|value| value.get("external_delivery"))
        .and_then(Value::as_str);
    let delivery_state = result_object
        .and_then(|value| value.get("delivery_state"))
        .and_then(Value::as_str);

    let (code, display_text, voice_script, terminal) = match row.state.as_str() {
        "pending" | "validated" | "queued" => {
            ("command.queued", "The command is queued.", None, false)
        }
        "awaiting_confirmation" => (
            "command.awaiting_confirmation",
            "Confirmation is required before this action can run.",
            Some("Please confirm this action in Knock Knock."),
            false,
        ),
        "running" => ("command.running", "The command is running.", None, false),
        "retryable" => (
            "command.retryable",
            "The backend will retry this command.",
            None,
            false,
        ),
        "unknown" => (
            "command.unknown",
            "Completion could not be verified. Check status before trying again.",
            None,
            false,
        ),
        "succeeded" if was_undone => (
            "command.undone",
            "The action was undone.",
            Some("The action was undone."),
            true,
        ),
        "succeeded" => match (row.intent.as_str(), result_kind) {
            ("search_history", Some("history_search")) => (
                "history_search.completed",
                "History search completed. Review the results on screen.",
                Some("History search completed."),
                true,
            ),
            ("create_reminder", Some("reminder")) => (
                "reminder.created",
                "Reminder created.",
                Some("Reminder created."),
                true,
            ),
            ("create_draft", Some("draft")) => {
                ("draft.saved", "Draft saved.", Some("Draft saved."), true)
            }
            ("send_message", Some("message"))
                if external_delivery == Some("sent") && delivery_state == Some("sent") =>
            {
                (
                    "send_message.sent",
                    "Message sent.",
                    Some("Message sent."),
                    true,
                )
            }
            ("send_message", Some("message")) => (
                "send_message.queued_locally",
                "Message saved to the local outbox; external delivery is not confirmed.",
                Some("Message queued locally."),
                true,
            ),
            _ => (
                "command.succeeded",
                "The command completed.",
                Some("The command completed."),
                true,
            ),
        },
        "failed" => (
            "command.failed",
            "The backend could not complete this command.",
            Some("The command failed."),
            true,
        ),
        "expired" => (
            "command.expired",
            "The command expired before it could complete.",
            Some("The command expired."),
            true,
        ),
        "cancelled" => (
            "command.cancelled",
            "The command was cancelled.",
            Some("The command was cancelled."),
            true,
        ),
        _ => (
            "command.reconciling",
            "The command is being reconciled with the backend.",
            None,
            false,
        ),
    };

    json!({
        "schema_version": 1,
        "code": code,
        "locale": "en",
        "display_text": display_text,
        "voice_script": voice_script,
        "terminal": terminal,
    })
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
        "presentation": presentation_value(row),
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

fn command_was_undone(row: &CommandRow) -> bool {
    result_value(row)
        .as_object()
        .is_some_and(|result| result.contains_key("undo"))
}

fn command_is_reversible_success(row: &CommandRow) -> bool {
    if row.state != "succeeded"
        || !registry_policy(&row.intent).is_some_and(|policy| policy.reversible)
    {
        return false;
    }

    let result = result_value(row);
    let kind = result
        .as_object()
        .and_then(|value| value.get("kind"))
        .and_then(Value::as_str);
    matches!(
        (row.intent.as_str(), kind),
        ("create_reminder", Some("reminder")) | ("create_draft", Some("draft"))
    )
}

fn timestamp_millis(timestamp: &str) -> Option<f64> {
    let millis =
        worker::js_sys::Date::new(&worker::wasm_bindgen::JsValue::from_str(timestamp)).get_time();
    (millis.is_finite() && millis >= 0.0).then_some(millis)
}

fn undo_window_open_at(
    row: &CommandRow,
    completed_at_millis: Option<f64>,
    now_millis: f64,
) -> bool {
    if !command_is_reversible_success(row) || command_was_undone(row) || !now_millis.is_finite() {
        return false;
    }
    let Some(completed_at_millis) = completed_at_millis.filter(|value| value.is_finite()) else {
        return false;
    };
    let elapsed_millis = now_millis - completed_at_millis;
    (0.0..UNDO_WINDOW_SECONDS * 1_000.0).contains(&elapsed_millis)
}

fn undo_request_allowed_at(
    row: &CommandRow,
    completed_at_millis: Option<f64>,
    now_millis: f64,
) -> bool {
    command_is_reversible_success(row)
        && (command_was_undone(row) || undo_window_open_at(row, completed_at_millis, now_millis))
}

fn response_at(
    row: &CommandRow,
    confirmation_token: Option<&str>,
    completed_at_millis: Option<f64>,
    now_millis: f64,
) -> Value {
    let undo_command_id =
        undo_window_open_at(row, completed_at_millis, now_millis).then(|| row.id.clone());
    json!({
        "command_id": row.id,
        "state": row.state,
        "command": envelope_from_row(row),
        "action": action_metadata(&row.intent),
        "presentation": presentation_value(row),
        "confirmation_token": confirmation_token,
        "result": result_value(row),
        "error": error_value(row),
        "undo_command_id": undo_command_id,
        "version": row.version,
        "created_at": row.created_at,
        "updated_at": row.updated_at,
    })
}

pub fn response(row: &CommandRow, confirmation_token: Option<&str>) -> Value {
    response_at(
        row,
        confirmation_token,
        timestamp_millis(&row.updated_at),
        worker::js_sys::Date::now(),
    )
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

fn expirable_state(state: &str) -> bool {
    matches!(
        state,
        "pending" | "validated" | "awaiting_confirmation" | "queued" | "retryable" | "unknown"
    )
}

/// Persist an expiration transition exactly once. The `changes()` fences make
/// audit, sync, and outbox cleanup conditional on the command update that won
/// the race; a second sweeper or request is therefore a no-op.
async fn mark_expired(
    db: &D1Database,
    user_id: &str,
    command: &CommandRow,
    reason: &str,
    confirmation_token_expired: bool,
) -> ApiResult<bool> {
    if !expirable_state(&command.state) {
        return Ok(false);
    }
    let now = db::now_iso();
    let next_version = command.version + 1;
    let condition = if confirmation_token_expired {
        "state = 'awaiting_confirmation' AND EXISTS (SELECT 1 FROM confirmation_tokens WHERE command_id = commands.id AND user_id = commands.user_id AND used_at IS NULL AND expires_at <= ?)"
    } else {
        "expires_at IS NOT NULL AND expires_at <= ?"
    };
    let statements = vec![
        db::prepare(
            db,
            &format!(
                "UPDATE commands SET state = 'expired', error_code = ?, result_json = NULL, version = ?, updated_at = ? WHERE id = ? AND user_id = ? AND state IN ('pending', 'validated', 'awaiting_confirmation', 'queued', 'retryable', 'unknown') AND version = ? AND {condition}"
            ),
            vec![
                db::text(reason),
                db::number(next_version),
                db::text(&now),
                db::text(&command.id),
                db::text(user_id),
                db::number(command.version),
                db::text(&now),
            ],
        )?,
        db::prepare(
            db,
            "INSERT INTO audit_logs (id, user_id, session_id, action, metadata_json, created_at) SELECT ?, ?, ?, 'command.expired', ?, ? WHERE changes() = 1",
            vec![
                db::text(&new_id("aud")?),
                db::text(user_id),
                db::optional_text(command.session_id.as_deref()),
                db::text(&json!({"command_id": command.id, "reason": reason, "version": next_version}).to_string()),
                db::text(&now),
            ],
        )?,
        db::prepare(
            db,
            "INSERT INTO phone_changes (user_id, entity_type, entity_id, session_id, version, created_at) SELECT ?, 'command', ?, ?, ?, ? WHERE changes() = 1",
            vec![
                db::text(user_id),
                db::text(&command.id),
                db::optional_text(command.session_id.as_deref()),
                db::number(next_version),
                db::text(&now),
            ],
        )?,
        db::prepare(
            db,
            "UPDATE outbox_events SET state = 'failed', next_attempt_at = NULL, last_error = ?, lease_token = NULL, lease_expires_at = NULL, updated_at = ? WHERE aggregate_id = ? AND user_id = ? AND state IN ('queued', 'retrying', 'unknown') AND changes() = 1 AND EXISTS (SELECT 1 FROM commands WHERE id = ? AND user_id = ? AND state = 'expired' AND version = ?)",
            vec![
                db::text(reason),
                db::text(&now),
                db::text(&command.id),
                db::text(user_id),
                db::text(&command.id),
                db::text(user_id),
                db::number(next_version),
            ],
        )?,
    ];
    let results = db.batch(statements).await?;
    Ok(results.first().map(db::changes).unwrap_or(0) == 1)
}

/// Expire an active command before a read/confirm/cancel operation can act on
/// stale authorization or queue work that is no longer valid.
pub async fn expire_if_due(
    db: &D1Database,
    user_id: &str,
    command_id: &str,
) -> ApiResult<Option<CommandRow>> {
    let Some(command) = get_for_user(db, user_id, command_id).await? else {
        return Ok(None);
    };
    if expirable_state(&command.state) && command.expires_at.as_deref().is_some_and(db::is_expired)
    {
        let _ = mark_expired(db, user_id, &command, "command_expired", false).await?;
    }
    get_for_user(db, user_id, command_id).await
}

/// Scheduled workers call this bounded sweep before claiming outbox work. It
/// also closes an awaiting-confirmation command when its one-time token ages
/// out, preventing an otherwise permanent `awaiting_confirmation` record.
pub async fn expire_due(db: &D1Database) -> ApiResult<usize> {
    let now = db::now_iso();
    let rows: Vec<CommandScopeRow> = db::all(
        db,
        "SELECT id, user_id FROM commands WHERE (state IN ('pending', 'validated', 'awaiting_confirmation', 'queued', 'retryable', 'unknown') AND expires_at IS NOT NULL AND expires_at <= ?) OR (state = 'awaiting_confirmation' AND EXISTS (SELECT 1 FROM confirmation_tokens WHERE command_id = commands.id AND user_id = commands.user_id AND used_at IS NULL AND expires_at <= ?)) ORDER BY updated_at ASC LIMIT 50",
        vec![db::text(&now), db::text(&now)],
    )
    .await?;
    let mut expired = 0;
    for scope in rows {
        let Some(command) = get_for_user(db, &scope.user_id, &scope.id).await? else {
            continue;
        };
        let command_expired = command.expires_at.as_deref().is_some_and(db::is_expired);
        let token_expired = command.state == "awaiting_confirmation" && !command_expired;
        if mark_expired(
            db,
            &scope.user_id,
            &command,
            if token_expired {
                "confirmation_expired"
            } else {
                "command_expired"
            },
            token_expired,
        )
        .await?
        {
            expired += 1;
        }
    }
    Ok(expired)
}

fn confirmation_replay_eligible_at(command: &CommandRow, now: &str) -> bool {
    command.state == "awaiting_confirmation"
        && command.needs_confirmation == 1
        && command
            .expires_at
            .as_deref()
            .is_some_and(|expires_at| expires_at > now)
}

fn confirmation_replay_command_update_sql() -> &'static str {
    "UPDATE commands SET version = version + 1, updated_at = ? WHERE id = ? AND user_id = ? AND idempotency_key = ? AND command_hash = ? AND version = ? AND state = 'awaiting_confirmation' AND needs_confirmation = 1 AND expires_at IS NOT NULL AND expires_at > ? AND (session_id IS NULL OR EXISTS (SELECT 1 FROM sessions WHERE sessions.id = commands.session_id AND sessions.user_id = commands.user_id AND sessions.deleted_at IS NULL))"
}

fn confirmation_replay_invalidation_sql() -> &'static str {
    "UPDATE confirmation_tokens SET used_at = ? WHERE command_id = ? AND user_id = ? AND used_at IS NULL AND changes() = 1 AND EXISTS (SELECT 1 FROM commands AS replay_command WHERE replay_command.id = ? AND replay_command.user_id = ? AND replay_command.idempotency_key = ? AND replay_command.command_hash = ? AND replay_command.version = ? AND replay_command.state = 'awaiting_confirmation' AND replay_command.needs_confirmation = 1 AND replay_command.expires_at IS NOT NULL AND replay_command.expires_at > ? AND (replay_command.session_id IS NULL OR EXISTS (SELECT 1 FROM sessions WHERE sessions.id = replay_command.session_id AND sessions.user_id = replay_command.user_id AND sessions.deleted_at IS NULL)))"
}

fn confirmation_replay_insert_sql() -> &'static str {
    "INSERT INTO confirmation_tokens (id, command_id, user_id, token_hash, command_hash, expires_at, created_at) SELECT ?, replay_command.id, replay_command.user_id, ?, replay_command.command_hash, MIN(replay_command.expires_at, ?), ? FROM commands AS replay_command WHERE changes() = 1 AND replay_command.id = ? AND replay_command.user_id = ? AND replay_command.idempotency_key = ? AND replay_command.command_hash = ? AND replay_command.version = ? AND replay_command.state = 'awaiting_confirmation' AND replay_command.needs_confirmation = 1 AND replay_command.expires_at IS NOT NULL AND replay_command.expires_at > ? AND (replay_command.session_id IS NULL OR EXISTS (SELECT 1 FROM sessions WHERE sessions.id = replay_command.session_id AND sessions.user_id = replay_command.user_id AND sessions.deleted_at IS NULL)) AND NOT EXISTS (SELECT 1 FROM confirmation_tokens WHERE confirmation_tokens.command_id = replay_command.id AND confirmation_tokens.user_id = replay_command.user_id AND confirmation_tokens.used_at IS NULL)"
}

/// Rotate the write-only confirmation authority for an exact idempotent replay.
/// D1 marks every previous token used and inserts the replacement sequentially
/// in one batch transaction, preserving an audit trail while the partial unique
/// index permits at most one unused token for the command.
async fn reissue_confirmation_token_for_replay(
    db: &D1Database,
    user_id: &str,
    idempotency_key: &str,
    command_hash: &str,
    command: &CommandRow,
) -> ApiResult<Option<(String, i64)>> {
    let now = db::now_iso();
    if command.user_id != user_id
        || command.idempotency_key != idempotency_key
        || command.command_hash != command_hash
        || !confirmation_replay_eligible_at(command, &now)
    {
        return Ok(None);
    }

    let token = new_id("ctok")?;
    let token_hash = sha256_hex(&token);
    let token_id = new_id("cont")?;
    let token_expires_at = db::add_seconds_iso(CONFIRMATION_TTL_SECONDS);
    let next_version = command.version + 1;
    let results = db
        .batch(vec![
            db::prepare(
                db,
                confirmation_replay_command_update_sql(),
                vec![
                    db::text(&now),
                    db::text(&command.id),
                    db::text(user_id),
                    db::text(idempotency_key),
                    db::text(command_hash),
                    db::number(command.version),
                    db::text(&now),
                ],
            )?,
            db::prepare(
                db,
                confirmation_replay_invalidation_sql(),
                vec![
                    db::text(&now),
                    db::text(&command.id),
                    db::text(user_id),
                    db::text(&command.id),
                    db::text(user_id),
                    db::text(idempotency_key),
                    db::text(command_hash),
                    db::number(next_version),
                    db::text(&now),
                ],
            )?,
            db::prepare(
                db,
                confirmation_replay_insert_sql(),
                vec![
                    db::text(&token_id),
                    db::text(&token_hash),
                    db::text(&token_expires_at),
                    db::text(&now),
                    db::text(&command.id),
                    db::text(user_id),
                    db::text(idempotency_key),
                    db::text(command_hash),
                    db::number(next_version),
                    db::text(&now),
                ],
            )?,
            db::prepare(
                db,
                "INSERT INTO audit_logs (id, user_id, session_id, action, metadata_json, created_at) SELECT ?, ?, ?, 'command.confirmation_reissued', ?, ? WHERE changes() = 1 AND EXISTS (SELECT 1 FROM commands WHERE id = ? AND user_id = ? AND state = 'awaiting_confirmation' AND version = ?)",
                vec![
                    db::text(&new_id("aud")?),
                    db::text(user_id),
                    db::optional_text(command.session_id.as_deref()),
                    db::text(
                        &json!({"command_id": command.id, "version": next_version}).to_string(),
                    ),
                    db::text(&now),
                    db::text(&command.id),
                    db::text(user_id),
                    db::number(next_version),
                ],
            )?,
            db::prepare(
                db,
                "INSERT INTO phone_changes (user_id, entity_type, entity_id, session_id, version, created_at) SELECT ?, 'command', ?, ?, ?, ? WHERE changes() = 1 AND EXISTS (SELECT 1 FROM commands WHERE id = ? AND user_id = ? AND state = 'awaiting_confirmation' AND version = ?)",
                vec![
                    db::text(user_id),
                    db::text(&command.id),
                    db::optional_text(command.session_id.as_deref()),
                    db::number(next_version),
                    db::text(&now),
                    db::text(&command.id),
                    db::text(user_id),
                    db::number(next_version),
                ],
            )?,
        ])
        .await?;

    Ok((results.get(2).map(db::changes).unwrap_or(0) == 1).then_some((token, next_version)))
}

fn replay_token_for_response(
    issued: Option<(String, i64)>,
    response_version: i64,
) -> Option<String> {
    issued.and_then(|(token, issued_version)| (issued_version == response_version).then_some(token))
}

pub async fn create(db: &D1Database, user_id: &str, envelope: CommandEnvelope) -> ApiResult<Value> {
    if user_id.trim().is_empty() {
        return Err(ApiError::unauthorized("Authenticated user is required"));
    }
    validate_envelope(&envelope).map_err(validation_error)?;
    let policy =
        registry_policy(&envelope.intent).ok_or_else(|| registry_error(&envelope.intent))?;
    validate_action_args(&envelope.intent, &envelope.args).map_err(validation_error)?;
    validate_scope(db, user_id, &envelope).await?;
    let authoritative = authoritative_envelope(&envelope, policy);
    let command_hash = canonical_hash(&authoritative)?;

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
        let issued = reissue_confirmation_token_for_replay(
            db,
            user_id,
            &envelope.idempotency_key,
            &command_hash,
            &existing,
        )
        .await?;
        let replayed = get_for_user(db, user_id, &existing.id)
            .await?
            .ok_or_else(|| ApiError::new(500, "command_error", "Command disappeared"))?;
        let token = replay_token_for_response(issued, replayed.version);
        return Ok(response(&replayed, token.as_deref()));
    }

    let command_id = envelope.command_id.clone();
    let now = db::now_iso();
    let expires_at = db::add_seconds_iso(COMMAND_TTL_SECONDS);
    let registry_confirmation = registry_requires_confirmation(&envelope.intent)
        .ok_or_else(|| registry_error(&envelope.intent))?;
    let confirmation_required = requires_confirmation(&envelope, registry_confirmation);
    let final_version = 2_i64;
    let outbox_idempotency_key =
        providers::scoped_idempotency_key(user_id, "command.execute", &envelope.idempotency_key);
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
                db::text(policy.risk_level),
                db::bool_number(confirmation_required),
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
        let token_expires_at = db::add_seconds_iso(CONFIRMATION_TTL_SECONDS);
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
                db::text(&outbox_idempotency_key),
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
                let issued = reissue_confirmation_token_for_replay(
                    db,
                    user_id,
                    &envelope.idempotency_key,
                    &command_hash,
                    &existing,
                )
                .await?;
                let replayed = get_for_user(db, user_id, &existing.id)
                    .await?
                    .ok_or_else(|| ApiError::new(500, "command_error", "Command disappeared"))?;
                let token = replay_token_for_response(issued, replayed.version);
                return Ok(response(&replayed, token.as_deref()));
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
    if user_id.trim().is_empty() {
        return Err(ApiError::unauthorized("Authenticated user is required"));
    }
    let token = token.trim();
    if token.is_empty() {
        return Err(ApiError::validation("confirmation_token is required"));
    }
    if token.chars().count() > MAX_CONFIRMATION_TOKEN {
        return Err(ApiError::validation("confirmation_token is too long"));
    }
    let mut command = get_for_user(db, user_id, command_id)
        .await?
        .ok_or_else(|| ApiError::not_found("Command not found"))?;
    if expirable_state(&command.state) && command.expires_at.as_deref().is_some_and(db::is_expired)
    {
        let _ = mark_expired(db, user_id, &command, "command_expired", false).await?;
        command = get_for_user(db, user_id, command_id)
            .await?
            .ok_or_else(|| ApiError::not_found("Command not found"))?;
    }
    if command.state == "expired" {
        return Err(ApiError::gone("Command has expired"));
    }
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
    if token_row.command_hash != command.command_hash {
        return Err(ApiError::unauthorized(
            "Confirmation token does not match command",
        ));
    }
    if db::is_expired(&token_row.expires_at) {
        let _ = mark_expired(db, user_id, &command, "confirmation_expired", true).await?;
        return Err(ApiError::gone("Confirmation token expired"));
    }

    let now = db::now_iso();
    let next_version = command.version + 1;
    let outbox_id = new_id("out")?;
    let outbox_idempotency_key =
        providers::scoped_idempotency_key(user_id, "command.execute.confirm", command_id);
    let mut statements = vec![
        db::prepare(
            db,
            "UPDATE confirmation_tokens SET used_at = ? WHERE id = ? AND user_id = ? AND command_hash = ? AND used_at IS NULL AND expires_at > ? AND EXISTS (SELECT 1 FROM commands WHERE id = ? AND user_id = ? AND state = 'awaiting_confirmation' AND version = ? AND (expires_at IS NULL OR expires_at > ?) AND (session_id IS NULL OR EXISTS (SELECT 1 FROM sessions WHERE id = commands.session_id AND user_id = ? AND deleted_at IS NULL)))",
            vec![
                db::text(&now),
                db::text(&token_row.id),
                db::text(user_id),
                db::text(&command.command_hash),
                db::text(&now),
                db::text(command_id),
                db::text(user_id),
                db::number(command.version),
                db::text(&now),
                db::text(user_id),
            ],
        )?,
        db::prepare(
            db,
            "UPDATE commands SET state = 'queued', version = ?, updated_at = ? WHERE id = ? AND user_id = ? AND state = 'awaiting_confirmation' AND version = ? AND changes() = 1 AND (expires_at IS NULL OR expires_at > ?) AND (session_id IS NULL OR EXISTS (SELECT 1 FROM sessions WHERE id = commands.session_id AND user_id = ? AND deleted_at IS NULL))",
            vec![
                db::number(next_version),
                db::text(&now),
                db::text(command_id),
                db::text(user_id),
                db::number(command.version),
                db::text(&now),
                db::text(user_id),
            ],
        )?,
        db::prepare(
            db,
            "INSERT INTO outbox_events (id, user_id, topic, aggregate_id, payload_json, idempotency_key, state, created_at, updated_at) SELECT ?, ?, 'command.execute', ?, ?, ?, 'queued', ?, ? WHERE changes() = 1",
            vec![
                db::text(&outbox_id),
                db::text(user_id),
                db::text(command_id),
                db::text(&json!({"command_id": command_id}).to_string()),
                db::text(&outbox_idempotency_key),
                db::text(&now),
                db::text(&now),
            ],
        )?,
        db::prepare(
            db,
            "INSERT INTO audit_logs (id, user_id, session_id, action, metadata_json, created_at) SELECT ?, ?, ?, 'command.confirm', ?, ? WHERE changes() = 1",
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
            "INSERT INTO phone_changes (user_id, entity_type, entity_id, session_id, version, created_at) SELECT ?, 'command', ?, ?, ?, ? WHERE changes() = 1",
            vec![
                db::text(user_id),
                db::text(command_id),
                db::optional_text(command.session_id.as_deref()),
                db::number(next_version),
                db::text(&now),
            ],
        )?,
    ];
    let results = match db.batch(std::mem::take(&mut statements)).await {
        Ok(results) => results,
        Err(error) => {
            let current = get_for_user(db, user_id, command_id).await?;
            if current.as_ref().is_some_and(|row| row.state == "queued") {
                return Err(ApiError::conflict("Confirmation token was already used"));
            }
            return Err(error.into());
        }
    };
    if results.first().map(db::changes).unwrap_or(0) == 0
        || results.get(1).map(db::changes).unwrap_or(0) == 0
    {
        let current = get_for_user(db, user_id, command_id).await?;
        if current.as_ref().is_some_and(|row| row.state == "expired") {
            return Err(ApiError::gone("Command has expired"));
        }
        return Err(ApiError::conflict("Confirmation token was already used"));
    }
    let updated = get_for_user(db, user_id, command_id)
        .await?
        .ok_or_else(|| ApiError::new(500, "command_error", "Command disappeared"))?;
    Ok(response(&updated, None))
}

pub async fn cancel(db: &D1Database, user_id: &str, command_id: &str) -> ApiResult<Value> {
    if user_id.trim().is_empty() {
        return Err(ApiError::unauthorized("Authenticated user is required"));
    }
    let mut command = get_for_user(db, user_id, command_id)
        .await?
        .ok_or_else(|| ApiError::not_found("Command not found"))?;
    if expirable_state(&command.state) && command.expires_at.as_deref().is_some_and(db::is_expired)
    {
        let _ = mark_expired(db, user_id, &command, "command_expired", false).await?;
        command = get_for_user(db, user_id, command_id)
            .await?
            .ok_or_else(|| ApiError::not_found("Command not found"))?;
    }
    if let Some(session_id) = command.session_id.as_deref() {
        ensure_session_live(db, user_id, session_id).await?;
    }
    if command.state == "cancelled" || command.state == "expired" {
        return Ok(response(&command, None));
    }
    if !matches!(
        command.state.as_str(),
        "pending" | "validated" | "awaiting_confirmation" | "queued" | "retryable"
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
            "UPDATE commands SET state = 'cancelled', error_code = NULL, version = ?, updated_at = ? WHERE id = ? AND user_id = ? AND state IN ('pending', 'validated', 'awaiting_confirmation', 'queued', 'retryable') AND version = ?",
            vec![
                db::number(next_version),
                db::text(&now),
                db::text(command_id),
                db::text(user_id),
                db::number(command.version),
            ],
        )?,
        db::prepare(
            db,
            "INSERT INTO audit_logs (id, user_id, session_id, action, metadata_json, created_at) SELECT ?, ?, ?, 'command.cancel', ?, ? WHERE changes() = 1",
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
            "INSERT INTO phone_changes (user_id, entity_type, entity_id, session_id, version, created_at) SELECT ?, 'command', ?, ?, ?, ? WHERE changes() = 1",
            vec![
                db::text(user_id),
                db::text(command_id),
                db::optional_text(command.session_id.as_deref()),
                db::number(next_version),
                db::text(&now),
            ],
        )?,
        db::prepare(
            db,
            "UPDATE outbox_events SET state = 'failed', next_attempt_at = NULL, last_error = 'command_cancelled', lease_token = NULL, lease_expires_at = NULL, updated_at = ? WHERE aggregate_id = ? AND user_id = ? AND state IN ('queued', 'retrying', 'unknown') AND changes() = 1",
            vec![db::text(&now), db::text(command_id), db::text(user_id)],
        )?,
    ];
    let results = db.batch(statements).await?;
    if results.first().map(db::changes).unwrap_or(0) == 0 {
        let current = get_for_user(db, user_id, command_id)
            .await?
            .ok_or_else(|| ApiError::new(500, "command_error", "Command disappeared"))?;
        if matches!(current.state.as_str(), "cancelled" | "expired") {
            return Ok(response(&current, None));
        }
        return Err(ApiError::conflict(
            "Command changed before it could be cancelled",
        ));
    }
    let updated = get_for_user(db, user_id, command_id)
        .await?
        .ok_or_else(|| ApiError::new(500, "command_error", "Command disappeared"))?;
    Ok(response(&updated, None))
}

pub async fn undo(
    env: &Env,
    db: &D1Database,
    user_id: &str,
    command_id: &str,
    provider_config: ActionProviderConfig,
) -> ApiResult<Value> {
    let command = get_for_user(db, user_id, command_id)
        .await?
        .ok_or_else(|| ApiError::not_found("Command not found"))?;
    if let Some(session_id) = command.session_id.as_deref() {
        ensure_session_live(db, user_id, session_id).await?;
    }
    if !command_is_reversible_success(&command) {
        return Err(ApiError::conflict("Command is not currently undoable"));
    }
    if !undo_request_allowed_at(
        &command,
        timestamp_millis(&command.updated_at),
        worker::js_sys::Date::now(),
    ) {
        return Err(ApiError::conflict("Command undo window has expired"));
    }
    let undo = crate::action_effects::undo(env, db, user_id, &command, provider_config).await?;
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

    fn command_row(intent: &str, state: &str, result: Option<Value>) -> CommandRow {
        CommandRow {
            id: "cmd_test".to_string(),
            user_id: "usr_test".to_string(),
            device_id: Some("dev_test".to_string()),
            session_id: None,
            schema_version: 1,
            intent: intent.to_string(),
            args_json: json!({
                "body": "private body",
                "recipient": "private recipient",
                "q": "private query"
            })
            .to_string(),
            risk_level: "low".to_string(),
            needs_confirmation: 0,
            idempotency_key: "idem_test".to_string(),
            confidence: Some(0.99),
            locale: "en-HK".to_string(),
            timezone: "Asia/Hong_Kong".to_string(),
            state: state.to_string(),
            command_hash: "hash".to_string(),
            result_json: result.map(|value| value.to_string()),
            error_code: None,
            expires_at: None,
            model_version: Some("1.0.0".to_string()),
            version: 7,
            created_at: "2026-08-11T00:00:00Z".to_string(),
            updated_at: "2026-08-11T00:00:01Z".to_string(),
        }
    }

    fn action_args(value: Value) -> Map<String, Value> {
        value.as_object().cloned().unwrap()
    }

    fn assert_valid_args(intent: &str, value: Value) {
        assert!(validate_action_args(intent, &action_args(value)).is_ok());
    }

    fn assert_invalid_args(intent: &str, value: Value) {
        assert_eq!(
            validate_action_args(intent, &action_args(value)),
            Err(CommandValidationError::InvalidActionArguments)
        );
    }

    #[test]
    fn presentation_never_interpolates_sensitive_command_or_result_text() {
        let mut row = command_row(
            "search_history",
            "succeeded",
            Some(json!({
                "kind": "history_search",
                "data": {
                    "query": "private query",
                    "items": [{"content": "private result"}],
                }
            })),
        );
        row.locale = "zh-Hans-HK".to_string();
        let presentation = presentation_value(&row);
        let rendered = presentation.to_string();
        assert_eq!(presentation["locale"], json!("en"));
        assert!(rendered.contains("history_search.completed"));
        assert!(!rendered.contains("private query"));
        assert!(!rendered.contains("private result"));
        assert!(!rendered.contains("private recipient"));
    }

    #[test]
    fn command_summary_excludes_raw_result_and_error_text() {
        let mut row = command_row(
            "search_history",
            "failed",
            Some(json!({
                "items": [{
                    "content": "private result text",
                    "url": "https://private.example/message"
                }]
            })),
        );
        row.error_code = Some("private provider error text".to_string());

        let value = summary(&row);
        let serialized = value.to_string();
        assert!(value.get("presentation").is_some());
        assert!(value.get("result").is_none());
        assert!(value.get("error").is_none());
        assert!(!serialized.contains("private result text"));
        assert!(!serialized.contains("https://private.example/message"));
        assert!(!serialized.contains("private provider error text"));
    }

    #[test]
    fn message_presentation_distinguishes_external_delivery_from_local_queue() {
        let delivered = command_row(
            "send_message",
            "succeeded",
            Some(json!({
                "kind": "message",
                "delivery_state": "sent",
                "external_delivery": "sent",
            })),
        );
        let local = command_row(
            "send_message",
            "succeeded",
            Some(json!({
                "kind": "message",
                "delivery_state": "sent",
                "external_delivery": "not_configured",
            })),
        );

        assert_eq!(
            presentation_value(&delivered)["code"],
            json!("send_message.sent")
        );
        assert_eq!(
            presentation_value(&local)["code"],
            json!("send_message.queued_locally")
        );
        assert_eq!(presentation_value(&local)["terminal"], json!(true));
    }

    #[test]
    fn confirmation_replay_requires_live_confirmation_state_and_expiry() {
        let now = "2026-08-11T00:00:00.000Z";
        let mut command = command_row("send_message", "awaiting_confirmation", None);
        command.needs_confirmation = 1;
        command.expires_at = Some("2026-08-11T00:01:00.000Z".to_string());
        assert!(confirmation_replay_eligible_at(&command, now));

        for state in ["queued", "succeeded", "failed", "expired", "cancelled"] {
            command.state = state.to_string();
            assert!(!confirmation_replay_eligible_at(&command, now));
        }

        command.state = "awaiting_confirmation".to_string();
        command.needs_confirmation = 0;
        assert!(!confirmation_replay_eligible_at(&command, now));

        command.needs_confirmation = 1;
        command.expires_at = Some(now.to_string());
        assert!(!confirmation_replay_eligible_at(&command, now));
        command.expires_at = None;
        assert!(!confirmation_replay_eligible_at(&command, now));
    }

    #[test]
    fn confirmation_replay_sql_revokes_then_rebinds_with_command_guards() {
        let update = confirmation_replay_command_update_sql();
        let invalidation = confirmation_replay_invalidation_sql();
        let insert = confirmation_replay_insert_sql();

        assert!(update.starts_with("UPDATE commands SET version = version + 1"));
        assert!(invalidation.starts_with("UPDATE confirmation_tokens SET used_at = ?"));
        assert!(invalidation.contains("command_id = ? AND user_id = ? AND used_at IS NULL"));
        assert!(invalidation.contains("changes() = 1"));
        assert!(insert.starts_with("INSERT INTO confirmation_tokens"));
        assert!(insert.contains(
            "SELECT ?, replay_command.id, replay_command.user_id, ?, replay_command.command_hash"
        ));
        assert!(insert.contains("MIN(replay_command.expires_at, ?)"));
        assert!(insert.contains("WHERE changes() = 1"));
        assert!(insert.contains("NOT EXISTS (SELECT 1 FROM confirmation_tokens"));

        for guard in [
            "replay_command.user_id = ?",
            "replay_command.idempotency_key = ?",
            "replay_command.command_hash = ?",
            "replay_command.version = ?",
            "replay_command.state = 'awaiting_confirmation'",
            "replay_command.needs_confirmation = 1",
            "replay_command.expires_at IS NOT NULL",
            "replay_command.expires_at > ?",
            "sessions.user_id = replay_command.user_id",
            "sessions.deleted_at IS NULL",
        ] {
            assert!(
                invalidation.contains(guard),
                "missing invalidation guard: {guard}"
            );
            assert!(insert.contains(guard), "missing insert guard: {guard}");
        }
    }

    #[test]
    fn confirmation_replay_never_returns_a_token_for_a_newer_command_version() {
        assert_eq!(
            replay_token_for_response(Some(("ctok_current".to_string(), 8)), 8),
            Some("ctok_current".to_string())
        );
        assert_eq!(
            replay_token_for_response(Some(("ctok_stale".to_string(), 8)), 9),
            None
        );
        assert_eq!(replay_token_for_response(None, 9), None);
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
    fn rejects_unknown_envelope_fields_before_command_validation() {
        let mut value = serde_json::to_value(envelope()).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .insert("untrusted_policy_override".into(), json!(true));
        assert!(serde_json::from_value::<CommandEnvelope>(value).is_err());
    }

    #[test]
    fn rejects_oversized_nested_arguments_and_optional_fields() {
        let mut oversized = envelope();
        oversized
            .args
            .insert("body".into(), Value::String("x".repeat(MAX_ARGUMENT_BYTES)));
        assert_eq!(
            validate_envelope(&oversized),
            Err(CommandValidationError::EnvelopeTooLarge)
        );

        let mut invalid_optional = envelope();
        invalid_optional.device_id = Some(" ".into());
        assert_eq!(
            validate_envelope(&invalid_optional),
            Err(CommandValidationError::InvalidFieldLength)
        );
    }

    #[test]
    fn backend_registry_policy_overrides_model_risk_and_flag() {
        let mut low = envelope();
        assert!(!requires_confirmation(&low, false));
        low.risk_level = "destructive".to_string();
        low.needs_confirmation = true;
        assert!(!requires_confirmation(&low, false));
        assert!(requires_confirmation(&envelope(), true));
    }

    #[test]
    fn registry_exposes_authoritative_risk_and_confirmation() {
        assert_eq!(
            registry_policy("search_history")
                .map(|policy| (policy.risk_level, policy.requires_confirmation)),
            Some(("low", false))
        );
        assert_eq!(registry_requires_confirmation("send_message"), Some(true));
        assert_eq!(
            registry_policy("send_message").map(|policy| policy.risk_level),
            Some("high")
        );
        assert_eq!(registry_requires_confirmation("run_arbitrary_code"), None);
    }

    #[test]
    fn search_history_argument_contract_is_fail_closed() {
        for valid in [
            json!({"q": "x"}),
            json!({"q": "家"}),
            json!({"q": "history"}),
            json!({"query": "history"}),
            json!({"text": "history"}),
        ] {
            assert_valid_args("search_history", valid);
        }

        for invalid in [
            json!({}),
            json!({"q": "   "}),
            json!({"q": "x".repeat(crate::history::SEARCH_QUERY_MAX_CHARACTERS + 1)}),
            json!({"q": 7}),
            json!({"q": "history", "limit": 5}),
            json!({"q": "history", "query": "history"}),
        ] {
            assert_invalid_args("search_history", invalid);
        }
        assert_invalid_args("unregistered_intent", json!({}));
    }

    #[test]
    fn create_reminder_argument_contract_is_fail_closed() {
        for valid in [
            json!({"title": "Pay rent", "due_at": "2099-01-01T09:00:00Z"}),
            json!({"text": "Pay rent", "time": "2099-01-01T09:00:00Z"}),
            json!({"message": "Pay rent", "datetime": "2099-01-01T09:00:00Z"}),
        ] {
            assert_valid_args("create_reminder", valid);
        }

        for invalid in [
            json!({"title": "Pay rent"}),
            json!({"title": 7, "due_at": "2099-01-01T09:00:00Z"}),
            json!({"title": "Pay rent", "due_at": false}),
            json!({"title": "Pay rent", "due_at": "later", "repeat": "daily"}),
            json!({"title": "Pay rent", "text": "Pay rent", "due_at": "later"}),
            json!({"title": "Pay rent", "due_at": "later", "time": "later"}),
        ] {
            assert_invalid_args("create_reminder", invalid);
        }
    }

    #[test]
    fn create_draft_argument_contract_is_fail_closed() {
        for valid in [
            json!({"body": "Hello", "title": "Greeting", "recipient": "user@example.com"}),
            json!({"content": "Hello", "subject": "Greeting", "to": "user@example.com"}),
            json!({"text": "Hello"}),
        ] {
            assert_valid_args("create_draft", valid);
        }

        for invalid in [
            json!({}),
            json!({"body": "Hello", "format": "markdown"}),
            json!({"body": "Hello", "content": "Hello"}),
            json!({"body": "Hello", "title": "Greeting", "subject": "Greeting"}),
            json!({"body": "Hello", "recipient": "user@example.com", "to": "user@example.com"}),
            json!({"body": "Hello", "title": 7}),
            json!({"body": "Hello", "recipient": null}),
            json!({"body": "x".repeat(MAX_ACTION_BODY + 1)}),
        ] {
            assert_invalid_args("create_draft", invalid);
        }
    }

    #[test]
    fn send_message_argument_contract_is_fail_closed() {
        for valid in [
            json!({"body": "Hello", "recipient": "+85255550123"}),
            json!({"message": "Hello", "email": "user@example.com"}),
            json!({"text": "Hello", "phone": "+85255550123"}),
            json!({"body": "Hello", "to": "user@example.com"}),
        ] {
            assert_valid_args("send_message", valid);
        }

        for invalid in [
            json!({}),
            json!({"body": "Hello", "recipient": 7}),
            json!({"body": ["Hello"], "recipient": "user@example.com"}),
            json!({"body": "Hello", "recipient": "user@example.com", "cc": "other@example.com"}),
            json!({"body": "Hello", "message": "Hello", "recipient": "user@example.com"}),
            json!({"body": "Hello", "recipient": "user@example.com", "email": "user@example.com"}),
        ] {
            assert_invalid_args("send_message", invalid);
        }
    }

    #[test]
    fn undo_window_boundaries_are_deterministic() {
        let command = command_row(
            "create_reminder",
            "succeeded",
            Some(json!({"kind": "reminder"})),
        );
        let completed_at = 1_000_000.0;
        let last_eligible_millisecond = completed_at + (UNDO_WINDOW_SECONDS * 1_000.0) - 1.0;
        let expires_at = completed_at + (UNDO_WINDOW_SECONDS * 1_000.0);

        assert!(undo_window_open_at(
            &command,
            Some(completed_at),
            completed_at
        ));
        assert!(undo_window_open_at(
            &command,
            Some(completed_at),
            last_eligible_millisecond
        ));
        assert!(!undo_window_open_at(
            &command,
            Some(completed_at),
            expires_at
        ));
        assert!(!undo_window_open_at(
            &command,
            Some(completed_at),
            completed_at - 1.0
        ));
        assert!(!undo_window_open_at(&command, None, completed_at));
    }

    #[test]
    fn undo_command_id_is_returned_only_while_eligible() {
        let completed_at = 1_000_000.0;
        for (intent, kind) in [("create_reminder", "reminder"), ("create_draft", "draft")] {
            let command = command_row(intent, "succeeded", Some(json!({"kind": kind})));
            assert_eq!(
                response_at(&command, None, Some(completed_at), completed_at + 1.0)
                    ["undo_command_id"],
                json!("cmd_test")
            );
        }

        for command in [
            command_row(
                "search_history",
                "succeeded",
                Some(json!({"kind": "history_search"})),
            ),
            command_row(
                "send_message",
                "succeeded",
                Some(json!({"kind": "message"})),
            ),
            command_row(
                "create_reminder",
                "failed",
                Some(json!({"kind": "reminder"})),
            ),
            command_row("create_reminder", "succeeded", None),
        ] {
            assert_eq!(
                response_at(&command, None, Some(completed_at), completed_at + 1.0)
                    ["undo_command_id"],
                Value::Null
            );
        }
    }

    #[test]
    fn completed_undo_is_not_advertised_but_replay_remains_idempotent() {
        let completed_at = 1_000_000.0;
        let mut command = command_row("create_draft", "succeeded", Some(json!({"kind": "draft"})));
        command.result_json = Some(
            json!({
                "kind": "draft",
                "undo": {"kind": "undo", "status": "cancelled"}
            })
            .to_string(),
        );

        assert_eq!(
            response_at(&command, None, Some(completed_at), completed_at + 1.0)["undo_command_id"],
            Value::Null
        );
        assert!(undo_request_allowed_at(
            &command,
            Some(completed_at),
            completed_at + (UNDO_WINDOW_SECONDS * 10_000.0)
        ));
    }

    #[test]
    fn execution_policy_must_match_the_registered_action() {
        assert!(execution_policy_matches("send_message", "high", true));
        assert!(!execution_policy_matches("send_message", "low", true));
        assert!(!execution_policy_matches("send_message", "high", false));
        assert!(!execution_policy_matches("unknown", "low", false));
    }

    #[test]
    fn authoritative_envelope_normalizes_model_risk_before_hashing() {
        let mut model = envelope();
        model.risk_level = "destructive".to_string();
        model.needs_confirmation = true;
        let policy = registry_policy("search_history").unwrap();
        let authoritative = authoritative_envelope(&model, policy);
        assert_eq!(authoritative.risk_level, "low");
        assert!(!authoritative.needs_confirmation);
        assert_ne!(
            canonical_hash(&model).unwrap(),
            canonical_hash(&authoritative).unwrap()
        );
    }

    #[test]
    fn state_transitions_are_terminal_and_ordered() {
        assert!(valid_transition(None, "pending"));
        assert!(valid_transition(Some("pending"), "validated"));
        assert!(valid_transition(Some("validated"), "queued"));
        assert!(valid_state("retryable"));
        assert!(valid_transition(Some("running"), "retryable"));
        assert!(valid_transition(Some("retryable"), "running"));
        assert!(valid_transition(Some("retryable"), "cancelled"));
        assert!(valid_transition(Some("retryable"), "unknown"));
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
