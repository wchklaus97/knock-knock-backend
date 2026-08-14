use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use worker::wasm_bindgen::JsValue;
use worker::{D1Database, Date};

use crate::auth::{new_id, sha256_hex};
use crate::db;
use crate::error::{ApiError, ApiResult};
use crate::pagination;

pub(crate) const SUBJECT_MAX_CHARACTERS: usize = 100;
pub(crate) const PREDICATE_MAX_CHARACTERS: usize = 100;
pub(crate) const DISPLAY_TEXT_MAX_CHARACTERS: usize = 2_000;
pub(crate) const LOCALE_MIN_CHARACTERS: usize = 2;
pub(crate) const LOCALE_MAX_CHARACTERS: usize = 35;
pub(crate) const IDEMPOTENCY_KEY_MIN_CHARACTERS: usize = 8;
pub(crate) const IDEMPOTENCY_KEY_MAX_CHARACTERS: usize = 200;
pub(crate) const VALUE_JSON_MAX_BYTES: usize = 8 * 1024;
pub(crate) const MEMORY_PAGE_MAX_ITEMS: i32 = 50;

const MEMORY_SELECT: &str = "SELECT id, schema_version, kind, subject, predicate, value_json, display_text, locale, source_type, source_session_id, source_message_id, user_confirmed, confidence, request_hash, version, retention_expires_at, created_at, updated_at FROM memory_items";

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateMemoryRequest {
    pub schema_version: i32,
    pub kind: String,
    pub subject: String,
    pub predicate: String,
    pub value: Value,
    pub display_text: String,
    pub locale: String,
    pub source_type: String,
    pub source_session_id: Option<String>,
    pub source_message_id: Option<String>,
    pub user_confirmed: bool,
    pub confidence: f64,
    pub idempotency_key: String,
    pub retention_expires_at: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct ValidatedMemoryRequest {
    schema_version: i32,
    kind: String,
    subject: String,
    predicate: String,
    value: Value,
    display_text: String,
    locale: String,
    source_type: String,
    source_session_id: Option<String>,
    source_message_id: Option<String>,
    user_confirmed: bool,
    confidence: f64,
    idempotency_key: String,
    retention_expires_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct MemoryItemRow {
    id: String,
    schema_version: i32,
    kind: String,
    subject: String,
    predicate: String,
    value_json: String,
    display_text: String,
    locale: String,
    source_type: String,
    source_session_id: Option<String>,
    source_message_id: Option<String>,
    user_confirmed: i32,
    confidence: f64,
    request_hash: String,
    version: i64,
    retention_expires_at: Option<String>,
    created_at: String,
    updated_at: String,
}

pub struct CreateMemoryResult {
    pub memory: Value,
    pub created: bool,
    pub memory_id: String,
    pub kind: String,
    pub source_type: String,
}

pub struct DeleteMemoryResult {
    pub deleted_at: String,
    pub kind: String,
    pub source_type: String,
}

fn valid_kind(value: &str) -> bool {
    matches!(
        value,
        "fact" | "preference" | "relationship" | "project" | "goal" | "constraint"
    )
}

fn normalized_text(
    field: &str,
    value: String,
    max_characters: usize,
    multiline: bool,
) -> ApiResult<String> {
    let value = value.trim().to_owned();
    let characters = value.chars().count();
    if characters == 0 || characters > max_characters {
        return Err(ApiError::validation(format!(
            "{field} must contain 1-{max_characters} characters"
        )));
    }
    if value
        .chars()
        .any(|character| character.is_control() && !(multiline && matches!(character, '\n' | '\t')))
    {
        return Err(ApiError::validation(format!(
            "{field} contains unsupported control characters"
        )));
    }
    Ok(value)
}

fn normalized_optional_id(field: &str, value: Option<String>) -> ApiResult<Option<String>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let value = value.trim().to_owned();
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b':'))
    {
        return Err(ApiError::validation(format!("Invalid {field}")));
    }
    Ok(Some(value))
}

fn valid_locale(value: &str) -> bool {
    let characters = value.chars().count();
    (LOCALE_MIN_CHARACTERS..=LOCALE_MAX_CHARACTERS).contains(&characters)
        && value.is_ascii()
        && value.split('-').all(|part| {
            !part.is_empty()
                && part.len() <= 8
                && part.bytes().all(|byte| byte.is_ascii_alphanumeric())
        })
}

fn valid_idempotency_key(value: &str) -> bool {
    let characters = value.chars().count();
    (IDEMPOTENCY_KEY_MIN_CHARACTERS..=IDEMPOTENCY_KEY_MAX_CHARACTERS).contains(&characters)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b':'))
}

fn validate_request(input: CreateMemoryRequest) -> ApiResult<ValidatedMemoryRequest> {
    if input.schema_version != 1 {
        return Err(ApiError::validation("schema_version must be 1"));
    }
    let kind = input.kind.trim().to_owned();
    if !valid_kind(&kind) {
        return Err(ApiError::validation("Invalid memory kind"));
    }
    if input.source_type != "explicit_user" {
        return Err(ApiError::validation(
            "Public memory writes require source_type=explicit_user",
        ));
    }
    if !input.user_confirmed {
        return Err(ApiError::validation(
            "Public memory writes require user_confirmed=true",
        ));
    }
    if !input.confidence.is_finite() || !(0.0..=1.0).contains(&input.confidence) {
        return Err(ApiError::validation("confidence must be between 0 and 1"));
    }

    let subject = normalized_text("subject", input.subject, SUBJECT_MAX_CHARACTERS, false)?;
    let predicate = normalized_text(
        "predicate",
        input.predicate,
        PREDICATE_MAX_CHARACTERS,
        false,
    )?;
    let display_text = normalized_text(
        "display_text",
        input.display_text,
        DISPLAY_TEXT_MAX_CHARACTERS,
        true,
    )?;

    let locale = input.locale.trim().to_owned();
    if !valid_locale(&locale) {
        return Err(ApiError::validation(
            "locale must be a 2-35 character language tag",
        ));
    }
    let idempotency_key = input.idempotency_key.trim().to_owned();
    if !valid_idempotency_key(&idempotency_key) {
        return Err(ApiError::validation(
            "idempotency_key must contain 8-200 safe characters",
        ));
    }

    let value_bytes = serde_json::to_vec(&input.value)?;
    if value_bytes.len() > VALUE_JSON_MAX_BYTES {
        return Err(ApiError::validation(
            "value must serialize to at most 8192 bytes",
        ));
    }

    let source_session_id = normalized_optional_id("source_session_id", input.source_session_id)?;
    let source_message_id = normalized_optional_id("source_message_id", input.source_message_id)?;
    if source_message_id.is_some() && source_session_id.is_none() {
        return Err(ApiError::validation(
            "source_message_id requires source_session_id",
        ));
    }

    let retention_expires_at = input
        .retention_expires_at
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty());
    if retention_expires_at
        .as_deref()
        .is_some_and(|value| value.len() > 64 || value.chars().any(char::is_control))
    {
        return Err(ApiError::validation("Invalid retention_expires_at"));
    }

    Ok(ValidatedMemoryRequest {
        schema_version: 1,
        kind,
        subject,
        predicate,
        value: input.value,
        display_text,
        locale,
        source_type: "explicit_user".into(),
        source_session_id,
        source_message_id,
        user_confirmed: true,
        confidence: input.confidence,
        idempotency_key,
        retention_expires_at,
    })
}

fn normalize_retention(value: Option<String>) -> ApiResult<Option<String>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let milliseconds = crate::commands::parse_rfc3339_millis(&value)
        .ok_or_else(|| ApiError::validation("retention_expires_at must be RFC3339"))?;
    if milliseconds <= Date::now().as_millis() as i64 {
        return Err(ApiError::validation(
            "retention_expires_at must be a future RFC3339 timestamp",
        ));
    }
    let parsed = worker::js_sys::Date::new(&JsValue::from_f64(milliseconds as f64));
    Ok(Some(parsed.to_iso_string().into()))
}

fn canonicalize_json(value: &Value) -> Value {
    match value {
        Value::Object(object) => {
            let mut keys = object.keys().collect::<Vec<_>>();
            keys.sort_unstable();
            let mut canonical = serde_json::Map::new();
            for key in keys {
                canonical.insert(key.clone(), canonicalize_json(&object[key]));
            }
            Value::Object(canonical)
        }
        Value::Array(values) => Value::Array(values.iter().map(canonicalize_json).collect()),
        _ => value.clone(),
    }
}

fn request_hash(input: &ValidatedMemoryRequest) -> ApiResult<String> {
    // Hash normalized semantic fields only. IDs and timestamps are generated
    // after this point and therefore cannot change replay identity.
    let mut canonical = input.clone();
    canonical.value = canonicalize_json(&canonical.value);
    Ok(sha256_hex(&serde_json::to_string(&canonical)?))
}

fn create_insert_sql() -> &'static str {
    "INSERT INTO memory_items (id, user_id, schema_version, kind, subject, predicate, value_json, display_text, locale, source_type, source_session_id, source_message_id, user_confirmed, confidence, idempotency_key, request_hash, version, retention_expires_at, created_at, updated_at, deleted_at) SELECT ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 1, ?, ?, ?, NULL WHERE (? IS NULL OR EXISTS (SELECT 1 FROM sessions WHERE id = ? AND user_id = ? AND deleted_at IS NULL AND (retention_expires_at IS NULL OR retention_expires_at > ?))) AND (? IS NULL OR EXISTS (SELECT 1 FROM session_messages WHERE id = ? AND user_id = ? AND session_id = ? AND (retention_expires_at IS NULL OR retention_expires_at > ?))) ON CONFLICT(user_id, idempotency_key) DO NOTHING"
}

fn list_sql(has_cursor: bool) -> &'static str {
    if has_cursor {
        "SELECT id, schema_version, kind, subject, predicate, value_json, display_text, locale, source_type, source_session_id, source_message_id, user_confirmed, confidence, request_hash, version, retention_expires_at, created_at, updated_at FROM memory_items WHERE user_id = ? AND deleted_at IS NULL AND (retention_expires_at IS NULL OR retention_expires_at > ?) AND (created_at < ? OR (created_at = ? AND id < ?)) ORDER BY created_at DESC, id DESC LIMIT ?"
    } else {
        "SELECT id, schema_version, kind, subject, predicate, value_json, display_text, locale, source_type, source_session_id, source_message_id, user_confirmed, confidence, request_hash, version, retention_expires_at, created_at, updated_at FROM memory_items WHERE user_id = ? AND deleted_at IS NULL AND (retention_expires_at IS NULL OR retention_expires_at > ?) ORDER BY created_at DESC, id DESC LIMIT ?"
    }
}

fn delete_sql() -> &'static str {
    "UPDATE memory_items SET deleted_at = ?, updated_at = ? WHERE id = ? AND user_id = ? AND deleted_at IS NULL AND (retention_expires_at IS NULL OR retention_expires_at > ?)"
}

fn memory_value(row: &MemoryItemRow) -> Value {
    let value = serde_json::from_str(&row.value_json).unwrap_or(Value::Null);
    json!({
        "memory_id": row.id,
        "schema_version": row.schema_version,
        "kind": row.kind,
        "subject": row.subject,
        "predicate": row.predicate,
        "value": value,
        "display_text": row.display_text,
        "locale": row.locale,
        "source_type": row.source_type,
        "source_session_id": row.source_session_id,
        "source_message_id": row.source_message_id,
        "user_confirmed": row.user_confirmed != 0,
        "confidence": row.confidence,
        "version": row.version,
        "retention_expires_at": row.retention_expires_at,
        "created_at": row.created_at,
        "updated_at": row.updated_at,
    })
}

fn ensure_matching_request_hash(stored: &str, requested: &str) -> ApiResult<()> {
    if stored == requested {
        Ok(())
    } else {
        Err(ApiError::conflict(
            "idempotency_key was already used with a different request",
        ))
    }
}

pub async fn create(
    db: &D1Database,
    user_id: &str,
    input: CreateMemoryRequest,
) -> ApiResult<CreateMemoryResult> {
    let mut input = validate_request(input)?;
    input.retention_expires_at = normalize_retention(input.retention_expires_at)?;
    let request_hash = request_hash(&input)?;
    let value_json = serde_json::to_string(&input.value)?;
    let memory_id = new_id("memory")?;
    let now = db::now_iso();

    // The unique index is the concurrency arbiter. A losing request does not
    // overwrite anything; every request then reads the winner and compares the
    // canonical hash before deciding whether this is a replay or a conflict.
    let inserted = db::run(
        db,
        create_insert_sql(),
        vec![
            db::text(&memory_id),
            db::text(user_id),
            db::number(input.schema_version as i64),
            db::text(&input.kind),
            db::text(&input.subject),
            db::text(&input.predicate),
            db::text(&value_json),
            db::text(&input.display_text),
            db::text(&input.locale),
            db::text(&input.source_type),
            db::optional_text(input.source_session_id.as_deref()),
            db::optional_text(input.source_message_id.as_deref()),
            db::bool_number(input.user_confirmed),
            db::decimal(input.confidence),
            db::text(&input.idempotency_key),
            db::text(&request_hash),
            db::optional_text(input.retention_expires_at.as_deref()),
            db::text(&now),
            db::text(&now),
            db::optional_text(input.source_session_id.as_deref()),
            db::optional_text(input.source_session_id.as_deref()),
            db::text(user_id),
            db::text(&now),
            db::optional_text(input.source_message_id.as_deref()),
            db::optional_text(input.source_message_id.as_deref()),
            db::text(user_id),
            db::optional_text(input.source_session_id.as_deref()),
            db::text(&now),
        ],
    )
    .await?;

    let row: MemoryItemRow = db::first(
        db,
        &format!("{MEMORY_SELECT} WHERE user_id = ? AND idempotency_key = ?"),
        vec![db::text(user_id), db::text(&input.idempotency_key)],
    )
    .await?
    .ok_or_else(|| {
        ApiError::validation(
            "source_session_id/source_message_id must belong to the authenticated user and same session",
        )
    })?;
    ensure_matching_request_hash(&row.request_hash, &request_hash)?;

    Ok(CreateMemoryResult {
        memory: memory_value(&row),
        created: db::changes(&inserted) > 0,
        memory_id: row.id,
        kind: row.kind,
        source_type: row.source_type,
    })
}

pub async fn list_for_user(
    db: &D1Database,
    user_id: &str,
    before: Option<&str>,
    limit: i32,
) -> ApiResult<Value> {
    let before = pagination::decode(before)?;
    let safe_limit = limit.clamp(1, MEMORY_PAGE_MAX_ITEMS) as i64;
    let now = db::now_iso();
    let rows: Vec<MemoryItemRow> = if let Some(cursor) = before {
        db::all(
            db,
            list_sql(true),
            vec![
                db::text(user_id),
                db::text(&now),
                db::text(&cursor.sort_key),
                db::text(&cursor.sort_key),
                db::text(&cursor.id),
                db::number(safe_limit + 1),
            ],
        )
        .await?
    } else {
        db::all(
            db,
            list_sql(false),
            vec![
                db::text(user_id),
                db::text(&now),
                db::number(safe_limit + 1),
            ],
        )
        .await?
    };
    let has_more = rows.len() as i64 > safe_limit;
    let rows = rows
        .into_iter()
        .take(safe_limit as usize)
        .collect::<Vec<_>>();
    let next_cursor = has_more
        .then(|| {
            rows.last()
                .map(|row| pagination::encode(&row.created_at, &row.id))
        })
        .flatten();
    Ok(json!({
        "memories": rows.iter().map(memory_value).collect::<Vec<_>>(),
        "next_cursor": next_cursor,
        "has_more": has_more,
    }))
}

pub async fn get_for_user(
    db: &D1Database,
    user_id: &str,
    memory_id: &str,
) -> ApiResult<Option<Value>> {
    let now = db::now_iso();
    let row: Option<MemoryItemRow> = db::first(
        db,
        &format!("{MEMORY_SELECT} WHERE id = ? AND user_id = ? AND deleted_at IS NULL AND (retention_expires_at IS NULL OR retention_expires_at > ?)"),
        vec![db::text(memory_id), db::text(user_id), db::text(&now)],
    )
    .await?;
    Ok(row.as_ref().map(memory_value))
}

pub async fn soft_delete(
    db: &D1Database,
    user_id: &str,
    memory_id: &str,
) -> ApiResult<DeleteMemoryResult> {
    let now = db::now_iso();
    #[derive(Deserialize)]
    struct AuditFields {
        kind: String,
        source_type: String,
    }
    let audit_fields: AuditFields = db::first(
        db,
        "SELECT kind, source_type FROM memory_items WHERE id = ? AND user_id = ? AND deleted_at IS NULL AND (retention_expires_at IS NULL OR retention_expires_at > ?)",
        vec![db::text(memory_id), db::text(user_id), db::text(&now)],
    )
    .await?
    .ok_or_else(|| ApiError::not_found("Memory not found"))?;
    let result = db::run(
        db,
        delete_sql(),
        vec![
            db::text(&now),
            db::text(&now),
            db::text(memory_id),
            db::text(user_id),
            db::text(&now),
        ],
    )
    .await?;
    if db::changes(&result) == 0 {
        return Err(ApiError::not_found("Memory not found"));
    }
    Ok(DeleteMemoryResult {
        deleted_at: now,
        kind: audit_fields.kind,
        source_type: audit_fields.source_type,
    })
}

pub fn create_audit_metadata(result: &CreateMemoryResult) -> Value {
    json!({
        "memory_id": result.memory_id,
        "kind": result.kind,
        "source_type": result.source_type,
        "result": if result.created { "created" } else { "replay" },
    })
}

pub fn delete_audit_metadata(memory_id: &str, result: &DeleteMemoryResult) -> Value {
    json!({
        "memory_id": memory_id,
        "kind": result.kind,
        "source_type": result.source_type,
    })
}

pub async fn soft_delete_expired_for_user(
    db: &D1Database,
    user_id: &str,
    now: &str,
) -> ApiResult<usize> {
    let result = db::run(
        db,
        "UPDATE memory_items SET deleted_at = ?, updated_at = ? WHERE user_id = ? AND deleted_at IS NULL AND retention_expires_at IS NOT NULL AND retention_expires_at <= ?",
        vec![
            db::text(now),
            db::text(now),
            db::text(user_id),
            db::text(now),
        ],
    )
    .await?;
    Ok(db::changes(&result))
}

pub async fn soft_delete_expired_all(db: &D1Database, now: &str, limit: i64) -> ApiResult<usize> {
    let result = db::run(
        db,
        "UPDATE memory_items SET deleted_at = ?, updated_at = ? WHERE id IN (SELECT id FROM memory_items WHERE deleted_at IS NULL AND retention_expires_at IS NOT NULL AND retention_expires_at <= ? ORDER BY created_at ASC, id ASC LIMIT ?)",
        vec![
            db::text(now),
            db::text(now),
            db::text(now),
            db::number(limit),
        ],
    )
    .await?;
    Ok(db::changes(&result))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_request() -> CreateMemoryRequest {
        CreateMemoryRequest {
            schema_version: 1,
            kind: "preference".into(),
            subject: "user".into(),
            predicate: "preferred_editor".into(),
            value: json!({"name": "Zed", "source_url": "https://untrusted.test"}),
            display_text: "The user prefers Zed.".into(),
            locale: "en-HK".into(),
            source_type: "explicit_user".into(),
            source_session_id: None,
            source_message_id: None,
            user_confirmed: true,
            confidence: 1.0,
            idempotency_key: "memory-test-0001".into(),
            retention_expires_at: None,
        }
    }

    #[test]
    fn public_creation_requires_v1_explicit_confirmation() {
        assert!(validate_request(valid_request()).is_ok());

        let mut wrong_version = valid_request();
        wrong_version.schema_version = 2;
        assert!(validate_request(wrong_version).is_err());

        let mut unconfirmed = valid_request();
        unconfirmed.user_confirmed = false;
        assert!(validate_request(unconfirmed).is_err());

        let mut trusted = valid_request();
        trusted.source_type = "trusted_system".into();
        assert!(validate_request(trusted).is_err());
    }

    #[test]
    fn text_locale_json_confidence_and_key_limits_are_strict() {
        let mut request = valid_request();
        request.subject = "家".repeat(SUBJECT_MAX_CHARACTERS);
        request.predicate = "x".repeat(PREDICATE_MAX_CHARACTERS);
        request.display_text = "記".repeat(DISPLAY_TEXT_MAX_CHARACTERS);
        assert!(validate_request(request).is_ok());

        let mut oversized_subject = valid_request();
        oversized_subject.subject = "家".repeat(SUBJECT_MAX_CHARACTERS + 1);
        assert!(validate_request(oversized_subject).is_err());

        let mut blank_predicate = valid_request();
        blank_predicate.predicate = "   ".into();
        assert!(validate_request(blank_predicate).is_err());

        let mut oversized_display = valid_request();
        oversized_display.display_text = "x".repeat(DISPLAY_TEXT_MAX_CHARACTERS + 1);
        assert!(validate_request(oversized_display).is_err());

        let mut invalid_locale = valid_request();
        invalid_locale.locale = "en--HK".into();
        assert!(validate_request(invalid_locale).is_err());

        let mut short_key = valid_request();
        short_key.idempotency_key = "short".into();
        assert!(validate_request(short_key).is_err());

        let mut oversized_value = valid_request();
        oversized_value.value = json!({"text": "x".repeat(VALUE_JSON_MAX_BYTES)});
        assert!(validate_request(oversized_value).is_err());

        let mut invalid_confidence = valid_request();
        invalid_confidence.confidence = 1.01;
        assert!(validate_request(invalid_confidence).is_err());
    }

    #[test]
    fn unknown_json_fields_and_internal_names_are_rejected() {
        let unknown = json!({
            "schema_version": 1,
            "kind": "fact",
            "subject": "user",
            "predicate": "timezone",
            "value": "Asia/Hong_Kong",
            "display_text": "The user's timezone is Asia/Hong_Kong.",
            "locale": "en-HK",
            "source_type": "explicit_user",
            "user_confirmed": true,
            "confidence": 1.0,
            "idempotency_key": "memory-test-0002",
            "request_hash": "must-not-be-public"
        });
        assert!(serde_json::from_value::<CreateMemoryRequest>(unknown).is_err());

        let internal_value_name = json!({
            "schema_version": 1,
            "kind": "fact",
            "subject": "user",
            "predicate": "timezone",
            "value_json": "Asia/Hong_Kong",
            "display_text": "The user's timezone is Asia/Hong_Kong.",
            "locale": "en-HK",
            "source_type": "explicit_user",
            "user_confirmed": true,
            "confidence": 1.0,
            "idempotency_key": "memory-test-0003"
        });
        assert!(serde_json::from_value::<CreateMemoryRequest>(internal_value_name).is_err());
    }

    #[test]
    fn canonical_hash_is_stable_and_conflicts_are_detected() {
        let first = validate_request(valid_request()).unwrap();
        let mut same = valid_request();
        same.value = json!({"source_url": "https://untrusted.test", "name": "Zed"});
        let same = validate_request(same).unwrap();
        assert_eq!(request_hash(&first).unwrap(), request_hash(&same).unwrap());

        let mut first_with_nested = valid_request();
        first_with_nested.value = serde_json::from_str(
            r#"{"name":"Zed","nested":{"a":2,"z":1},"source_url":"https://untrusted.test"}"#,
        )
        .unwrap();
        let mut same_nested = valid_request();
        same_nested.value = serde_json::from_str(
            r#"{"source_url":"https://untrusted.test","nested":{"z":1,"a":2},"name":"Zed"}"#,
        )
        .unwrap();
        let first_with_nested = validate_request(first_with_nested).unwrap();
        let same_nested = validate_request(same_nested).unwrap();
        assert_eq!(
            request_hash(&first_with_nested).unwrap(),
            request_hash(&same_nested).unwrap()
        );

        let mut changed = valid_request();
        changed.display_text = "The user prefers another editor.".into();
        let changed = validate_request(changed).unwrap();
        let first_hash = request_hash(&first).unwrap();
        let changed_hash = request_hash(&changed).unwrap();
        assert_ne!(first_hash, changed_hash);
        assert!(ensure_matching_request_hash(&first_hash, &first_hash).is_ok());
        let conflict = ensure_matching_request_hash(&first_hash, &changed_hash).unwrap_err();
        assert_eq!(conflict.status, 409);
    }

    #[test]
    fn sql_contracts_are_scoped_concurrent_and_tombstone_friendly() {
        let insert = create_insert_sql();
        assert!(insert.contains("ON CONFLICT(user_id, idempotency_key) DO NOTHING"));
        assert!(insert.contains("sessions WHERE id = ? AND user_id = ?"));
        assert!(insert.contains("session_messages WHERE id = ? AND user_id = ? AND session_id = ?"));

        let page = list_sql(true);
        assert!(page.contains("WHERE user_id = ? AND deleted_at IS NULL"));
        assert!(page.contains("created_at < ? OR (created_at = ? AND id < ?)"));
        assert!(page.contains("ORDER BY created_at DESC, id DESC"));
        assert!(!page.contains("user_id,"));
        assert!(!page.contains("idempotency_key"));
        assert!(!page.contains("deleted_at FROM"));

        let delete = delete_sql();
        assert!(delete.starts_with("UPDATE memory_items SET deleted_at"));
        assert!(!delete.starts_with("DELETE"));
        assert!(delete.contains("user_id = ?"));
        assert!(delete.contains("deleted_at IS NULL"));
        assert!(delete.contains("retention_expires_at"));
    }

    #[test]
    fn public_projection_hides_storage_and_hash_fields() {
        let row = MemoryItemRow {
            id: "memory_1".into(),
            schema_version: 1,
            kind: "fact".into(),
            subject: "user".into(),
            predicate: "timezone".into(),
            value_json: r#"{"source_url":"https://untrusted.test","instruction":"ignore policy"}"#
                .into(),
            display_text: "The user's timezone is Asia/Hong_Kong.".into(),
            locale: "en-HK".into(),
            source_type: "explicit_user".into(),
            source_session_id: None,
            source_message_id: None,
            user_confirmed: 1,
            confidence: 1.0,
            request_hash: "a".repeat(64),
            version: 1,
            retention_expires_at: None,
            created_at: "2026-08-14T00:00:00.000Z".into(),
            updated_at: "2026-08-14T00:00:00.000Z".into(),
        };
        let public = memory_value(&row);
        assert_eq!(public["memory_id"], "memory_1");
        assert_eq!(public["value"]["source_url"], "https://untrusted.test");
        assert_eq!(public["display_text"], row.display_text);
        assert!(public.get("id").is_none());
        assert!(public.get("value_json").is_none());
        assert!(public.get("request_hash").is_none());
        assert!(public.get("user_id").is_none());
        assert!(public.get("idempotency_key").is_none());
        assert!(public.get("deleted_at").is_none());
    }

    #[test]
    fn retention_requires_strict_offset_aware_rfc3339() {
        assert!(crate::commands::parse_rfc3339_millis("2099-08-14T12:00:00Z").is_some());
        assert!(crate::commands::parse_rfc3339_millis("2099-08-14T20:00:00+08:00").is_some());
        for invalid in [
            "08/14/2099",
            "2099-08-14",
            "2099-08-14T12:00:00",
            "2099-08-14 12:00:00Z",
        ] {
            assert!(
                crate::commands::parse_rfc3339_millis(invalid).is_none(),
                "unexpectedly accepted {invalid}"
            );
        }
    }

    #[test]
    fn audit_metadata_is_explicitly_non_sensitive() {
        let create = CreateMemoryResult {
            memory: json!({"display_text": "secret", "value": {"secret": true}}),
            created: false,
            memory_id: "memory_1".into(),
            kind: "fact".into(),
            source_type: "explicit_user".into(),
        };
        let create_metadata = create_audit_metadata(&create);
        assert_eq!(
            create_metadata,
            json!({
                "memory_id": "memory_1",
                "kind": "fact",
                "source_type": "explicit_user",
                "result": "replay",
            })
        );
        assert!(create_metadata.get("display_text").is_none());
        assert!(create_metadata.get("value").is_none());

        let delete = DeleteMemoryResult {
            deleted_at: "2099-08-14T12:00:00.000Z".into(),
            kind: "fact".into(),
            source_type: "explicit_user".into(),
        };
        let delete_metadata = delete_audit_metadata("memory_1", &delete);
        assert_eq!(
            delete_metadata,
            json!({
                "memory_id": "memory_1",
                "kind": "fact",
                "source_type": "explicit_user",
            })
        );
    }
}
