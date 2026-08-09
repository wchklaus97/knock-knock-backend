use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};

use crate::error::{ApiError, ApiResult};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cursor {
    pub sort_key: String,
    pub id: String,
}

pub fn encode(sort_key: &str, id: &str) -> String {
    let raw = format!("{sort_key}\u{1f}{id}");
    URL_SAFE_NO_PAD.encode(raw.as_bytes())
}

pub fn decode(raw: Option<&str>) -> ApiResult<Option<Cursor>> {
    let Some(raw) = raw else { return Ok(None) };
    if raw.trim().is_empty() {
        return Err(ApiError::validation("Cursor must not be empty"));
    }
    let bytes = URL_SAFE_NO_PAD
        .decode(raw)
        .map_err(|_| ApiError::validation("Invalid pagination cursor"))?;
    let value =
        String::from_utf8(bytes).map_err(|_| ApiError::validation("Invalid pagination cursor"))?;
    let (sort_key, id) = value
        .split_once('\u{1f}')
        .ok_or_else(|| ApiError::validation("Invalid pagination cursor"))?;
    if sort_key.is_empty() || id.is_empty() {
        return Err(ApiError::validation("Invalid pagination cursor"));
    }
    Ok(Some(Cursor {
        sort_key: sort_key.to_owned(),
        id: id.to_owned(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursors_round_trip_and_reject_invalid_values() {
        let encoded = encode("2026-08-09T00:00:00Z", "msg_1");
        assert_eq!(
            decode(Some(&encoded)).unwrap(),
            Some(Cursor {
                sort_key: "2026-08-09T00:00:00Z".into(),
                id: "msg_1".into(),
            })
        );
        assert!(decode(Some("not-a-cursor")).is_err());
        assert!(decode(Some("")).is_err());
    }
}
