use worker::wasm_bindgen::JsValue;
use worker::{D1Database, D1PreparedStatement, Date};

use crate::error::ApiResult;

pub fn now_iso() -> String {
    worker::js_sys::Date::new_0().to_iso_string().into()
}

pub fn add_seconds_iso(seconds: i64) -> String {
    let millis = Date::now().as_millis() as f64 + (seconds as f64 * 1000.0);
    worker::js_sys::Date::new(&JsValue::from_f64(millis))
        .to_iso_string()
        .into()
}

pub fn is_expired(value: &str) -> bool {
    let millis = worker::js_sys::Date::new(&JsValue::from_str(value)).get_time();
    !millis.is_finite() || millis <= Date::now().as_millis() as f64
}

pub fn text(value: &str) -> JsValue {
    JsValue::from_str(value)
}

pub fn optional_text(value: Option<&str>) -> JsValue {
    value.map(JsValue::from_str).unwrap_or_else(JsValue::null)
}

pub fn number(value: i64) -> JsValue {
    JsValue::from_f64(value as f64)
}

pub fn decimal(value: f64) -> JsValue {
    JsValue::from_f64(value)
}

pub fn prepare(db: &D1Database, sql: &str, values: Vec<JsValue>) -> ApiResult<D1PreparedStatement> {
    Ok(db.prepare(sql).bind(&values)?)
}

pub async fn first<T>(db: &D1Database, sql: &str, values: Vec<JsValue>) -> ApiResult<Option<T>>
where
    T: for<'de> serde::Deserialize<'de>,
{
    Ok(prepare(db, sql, values)?.first(None).await?)
}

pub async fn all<T>(db: &D1Database, sql: &str, values: Vec<JsValue>) -> ApiResult<Vec<T>>
where
    T: for<'de> serde::Deserialize<'de>,
{
    let result = prepare(db, sql, values)?.all().await?;
    Ok(result.results()?)
}

pub async fn run(db: &D1Database, sql: &str, values: Vec<JsValue>) -> ApiResult<worker::D1Result> {
    Ok(prepare(db, sql, values)?.run().await?)
}

pub fn changes(result: &worker::D1Result) -> usize {
    result
        .meta()
        .ok()
        .flatten()
        .and_then(|meta| meta.changes)
        .unwrap_or(0)
}

pub fn parse_json_array(raw: Option<&str>) -> Vec<String> {
    raw.and_then(|value| serde_json::from_str::<Vec<String>>(value).ok())
        .unwrap_or_default()
}
