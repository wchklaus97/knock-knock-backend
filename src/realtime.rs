use serde_json::{json, Value};

const ENTITY_TYPES: [&str; 5] = ["session", "message", "command", "push", "retrieval"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CursorError {
    Empty,
    Invalid,
    Negative,
}

impl std::fmt::Display for CursorError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Empty => "cursor is empty",
            Self::Invalid => "cursor must be a decimal integer",
            Self::Negative => "cursor must not be negative",
        })
    }
}

impl std::error::Error for CursorError {}

pub fn parse_cursor(raw: Option<&str>) -> Result<Option<i64>, CursorError> {
    let Some(raw) = raw else { return Ok(None) };
    if raw.trim().is_empty() {
        return Err(CursorError::Empty);
    }
    let cursor = raw.parse::<i64>().map_err(|_| CursorError::Invalid)?;
    if cursor < 0 {
        return Err(CursorError::Negative);
    }
    Ok(Some(cursor))
}

pub fn valid_entity_type(entity_type: &str) -> bool {
    ENTITY_TYPES.contains(&entity_type)
}

pub fn change_payload(
    cursor: i64,
    entity_type: &str,
    entity_id: &str,
    session_id: Option<&str>,
    version: i64,
) -> Result<Value, CursorError> {
    if cursor < 0 || !valid_entity_type(entity_type) || entity_id.trim().is_empty() {
        return Err(CursorError::Invalid);
    }
    Ok(json!({
        "cursor": cursor.to_string(),
        "entity_type": entity_type,
        "entity_id": entity_id,
        "session_id": session_id,
        "version": version,
    }))
}

pub fn sse_frame(id: &str, event_type: &str, data: &Value) -> Result<String, CursorError> {
    if id.trim().is_empty() || event_type.trim().is_empty() {
        return Err(CursorError::Invalid);
    }
    let body = serde_json::to_string(data).map_err(|_| CursorError::Invalid)?;
    Ok(format!("id: {id}\nevent: {event_type}\ndata: {body}\n\n"))
}

pub fn normalize_limit(value: Option<i64>) -> i64 {
    value.unwrap_or(50).clamp(1, 100)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_is_opaque_decimal_and_orderable() {
        assert_eq!(parse_cursor(None).unwrap(), None);
        assert_eq!(parse_cursor(Some("42")).unwrap(), Some(42));
        assert!(parse_cursor(Some("old|session")).is_err());
        assert!(parse_cursor(Some("-1")).is_err());
    }

    #[test]
    fn change_payload_has_user_sync_fields() {
        let payload = change_payload(9, "session", "ses_1", Some("ses_1"), 3).unwrap();
        assert_eq!(payload["cursor"], "9");
        assert_eq!(payload["entity_id"], "ses_1");
        assert_eq!(payload["version"], 3);
        assert!(change_payload(9, "unknown", "id", None, 1).is_err());
    }

    #[test]
    fn sse_is_notification_only_and_contains_json_data() {
        let frame = sse_frame("9", "session.updated", &json!({"session_id": "ses_1"})).unwrap();
        assert!(frame.contains("id: 9"));
        assert!(frame.contains("event: session.updated"));
        assert!(frame.contains("\"session_id\":\"ses_1\""));
    }

    #[test]
    fn limits_are_bounded_for_mobile_reads() {
        assert_eq!(normalize_limit(None), 50);
        assert_eq!(normalize_limit(Some(0)), 1);
        assert_eq!(normalize_limit(Some(500)), 100);
    }
}
