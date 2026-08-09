use serde_json::{json, Value};
use worker::{Env, Fetch, Headers, Method, Request, RequestInit};

use crate::auth::{config_value, secret_value, sha256_hex};
use crate::error::{ApiError, ApiResult};

/// Selects how a command effect is materialized.
///
/// `Internal` is deliberately limited to local development/test behavior. It
/// persists a durable D1 effect or queue record, but it does not claim that an
/// external reminder or message provider delivered anything. `External` is a
/// reserved adapter boundary: until a concrete provider is registered, it
/// fails closed and remains retryable. `Disabled` is an intentional permanent
/// failure for environments that do not allow the effect at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionProviderMode {
    Internal,
    External,
    Disabled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionProviderConfig {
    mode: ActionProviderMode,
    reminder_enabled: bool,
    message_enabled: bool,
    reminder_url: Option<String>,
    message_url: Option<String>,
    reminder_cancel_url: Option<String>,
    reminder_status_url: Option<String>,
    message_status_url: Option<String>,
    reminder_token: Option<String>,
    message_token: Option<String>,
}

impl ActionProviderConfig {
    pub fn mode(&self) -> ActionProviderMode {
        self.mode
    }

    pub fn enabled(&self, intent: &str) -> bool {
        match intent {
            "create_reminder" => self.reminder_enabled,
            "send_message" => self.message_enabled,
            "create_draft" => true,
            _ => false,
        }
    }

    pub fn endpoint(&self, intent: &str) -> Option<&str> {
        match intent {
            "create_reminder" => self.reminder_url.as_deref(),
            "send_message" => self.message_url.as_deref(),
            _ => None,
        }
    }

    pub fn cancel_endpoint(&self, intent: &str) -> Option<&str> {
        match intent {
            "create_reminder" => self.reminder_cancel_url.as_deref(),
            _ => None,
        }
    }

    pub fn status_endpoint(&self, intent: &str) -> Option<&str> {
        match intent {
            "create_reminder" => self.reminder_status_url.as_deref(),
            "send_message" => self.message_status_url.as_deref(),
            _ => None,
        }
    }

    fn token(&self, intent: &str) -> Option<&str> {
        match intent {
            "create_reminder" => self.reminder_token.as_deref(),
            "send_message" => self.message_token.as_deref(),
            _ => None,
        }
    }

    pub fn ready(&self) -> bool {
        if self.mode != ActionProviderMode::External {
            return self.mode == ActionProviderMode::Internal;
        }
        ["create_reminder", "send_message"]
            .into_iter()
            .filter(|intent| self.enabled(intent))
            .all(|intent| {
                self.endpoint(intent).is_some()
                    && self.status_endpoint(intent).is_some()
                    && self.token(intent).is_some()
                    && (intent != "create_reminder" || self.cancel_endpoint(intent).is_some())
            })
    }
}

impl ActionProviderMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Internal => "internal",
            Self::External => "external",
            Self::Disabled => "disabled",
        }
    }

    pub fn local_effects_allowed(self) -> bool {
        matches!(self, Self::Internal)
    }
}

pub fn parse_mode(raw: &str) -> ApiResult<ActionProviderMode> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "internal" => Ok(ActionProviderMode::Internal),
        "external" => Ok(ActionProviderMode::External),
        "disabled" => Ok(ActionProviderMode::Disabled),
        _ => Err(ApiError::new(
            500,
            "configuration_error",
            "ACTION_PROVIDER_MODE must be internal, external, or disabled",
        )),
    }
}

pub fn load(env: &Env) -> ApiResult<ActionProviderConfig> {
    let node_env = config_value(env, "NODE_ENV", "development")
        .trim()
        .to_ascii_lowercase();
    let default = if node_env == "production" {
        "external"
    } else {
        "internal"
    };
    let mode = parse_mode(&config_value(env, "ACTION_PROVIDER_MODE", default))?;
    let enabled_default = if node_env == "production" {
        "false"
    } else {
        "true"
    };
    let reminder_enabled = bool_value(env, "ACTION_REMINDER_ENABLED", enabled_default)?;
    let message_enabled = bool_value(env, "ACTION_MESSAGE_ENABLED", enabled_default)?;
    let reminder_url = optional_endpoint(env, "ACTION_REMINDER_URL", &node_env)?;
    let message_url = optional_endpoint(env, "ACTION_MESSAGE_URL", &node_env)?;
    let reminder_cancel_url = optional_endpoint(env, "ACTION_REMINDER_CANCEL_URL", &node_env)?;
    let reminder_status_url = optional_endpoint(env, "ACTION_REMINDER_STATUS_URL", &node_env)?;
    let message_status_url = optional_endpoint(env, "ACTION_MESSAGE_STATUS_URL", &node_env)?;
    let reminder_token = secret_value(env, "ACTION_REMINDER_TOKEN");
    let message_token = secret_value(env, "ACTION_MESSAGE_TOKEN");

    if mode == ActionProviderMode::External {
        validate_external_action(
            "create_reminder",
            reminder_enabled,
            reminder_url.as_deref(),
            reminder_cancel_url.as_deref(),
            reminder_status_url.as_deref(),
            reminder_token.as_deref(),
        )?;
        validate_external_action(
            "send_message",
            message_enabled,
            message_url.as_deref(),
            None,
            message_status_url.as_deref(),
            message_token.as_deref(),
        )?;
    }

    Ok(ActionProviderConfig {
        mode,
        reminder_enabled,
        message_enabled,
        reminder_url,
        message_url,
        reminder_cancel_url,
        reminder_status_url,
        message_status_url,
        reminder_token,
        message_token,
    })
}

fn optional_endpoint(env: &Env, name: &str, node_env: &str) -> ApiResult<Option<String>> {
    let raw = config_value(env, name, "");
    let value = raw.trim();
    if value.is_empty() || value.starts_with("REPLACE_") {
        return Ok(None);
    }
    let allowed_local = matches!(node_env, "development" | "test")
        && (value.starts_with("http://127.0.0.1:")
            || value.starts_with("http://localhost:")
            || value.starts_with("http://[::1]:"));
    if !value.starts_with("https://") && !allowed_local {
        return Err(ApiError::new(
            500,
            "configuration_error",
            format!("{name} must be an HTTPS URL (localhost is allowed only for local tests)"),
        ));
    }
    Ok(Some(value.trim_end_matches('/').to_string()))
}

fn validate_external_action(
    intent: &str,
    enabled: bool,
    endpoint: Option<&str>,
    cancel_endpoint: Option<&str>,
    status_endpoint: Option<&str>,
    token: Option<&str>,
) -> ApiResult<()> {
    if !enabled {
        return Ok(());
    }
    if endpoint.is_none()
        || status_endpoint.is_none()
        || (intent == "create_reminder" && cancel_endpoint.is_none())
        || token.is_none_or(|value| value.trim().is_empty())
    {
        return Err(ApiError::new(
            500,
            "configuration_error",
            format!(
                "External {intent} requires delivery, status, and secret configuration before it can be enabled"
            ),
        ));
    }
    Ok(())
}

fn bool_value(env: &Env, name: &str, default: &str) -> ApiResult<bool> {
    match config_value(env, name, default)
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(ApiError::new(
            500,
            "configuration_error",
            format!("{name} must be true or false"),
        )),
    }
}

pub fn unavailable(mode: ActionProviderMode, intent: &str) -> ApiError {
    match mode {
        ActionProviderMode::External => ApiError::new(
            503,
            "provider_not_configured",
            format!("No external provider adapter is configured for {intent}"),
        ),
        ActionProviderMode::Disabled => ApiError::new(
            409,
            "provider_disabled",
            format!("The provider effect is disabled for {intent}"),
        ),
        ActionProviderMode::Internal => ApiError::new(
            500,
            "provider_error",
            "Internal provider mode was used outside a local effect",
        ),
    }
}

pub fn disabled(intent: &str) -> ApiError {
    ApiError::new(
        409,
        "action_disabled",
        format!("The action is disabled for {intent}"),
    )
}

pub fn ready(config: &ActionProviderConfig) -> bool {
    config.ready()
}

/// Namespace provider idempotency by the authenticated user and action.
/// Command idempotency keys are only unique within one user, while provider
/// keys and the action-attempt uniqueness constraint are global.
pub fn scoped_idempotency_key(user_id: &str, operation: &str, command_key: &str) -> String {
    format!(
        "kk_{}",
        sha256_hex(&format!("v1:{operation}:{user_id}:{command_key}"))
    )
}

pub fn action_attempt_provider(intent: &str) -> Option<&'static str> {
    match intent {
        "create_reminder" => Some("action.reminder"),
        "send_message" => Some("action.message"),
        "create_draft" => Some("local.draft"),
        _ => None,
    }
}

pub fn scoped_action_idempotency_key(user_id: &str, intent: &str, command_key: &str) -> String {
    let operation = action_attempt_provider(intent).unwrap_or(intent);
    scoped_idempotency_key(user_id, operation, command_key)
}

#[derive(Debug, Clone)]
pub struct ProviderResponse {
    pub provider_id: Option<String>,
    pub state: ProviderDeliveryState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderDeliveryState {
    Succeeded,
    Pending,
    Failed,
    Unknown,
}

#[derive(Debug, Clone)]
pub struct ProviderStatus {
    pub state: ProviderDeliveryState,
    pub provider_id: Option<String>,
}

/// Send a minimal, idempotent provider request. The provider is deliberately
/// an HTTPS webhook boundary rather than a vendor SDK: the backend owns the
/// command/idempotency contract while a deployment can select a reminder or
/// messaging service independently. The bearer token is read only from the
/// Wrangler secret store and never included in errors or response bodies.
pub async fn send(
    _env: &Env,
    config: &ActionProviderConfig,
    intent: &str,
    idempotency_key: &str,
    payload: Value,
) -> ApiResult<ProviderResponse> {
    if config.mode != ActionProviderMode::External {
        return Err(unavailable(config.mode, intent));
    }
    let endpoint = config
        .endpoint(intent)
        .ok_or_else(|| unavailable(config.mode, intent))?;
    let token = config
        .token(intent)
        .ok_or_else(|| unavailable(config.mode, intent))?;
    let body = post_json(endpoint, token, intent, idempotency_key, payload).await?;
    let provider_id = provider_id_from_body(&body, intent);
    Ok(ProviderResponse {
        provider_id,
        state: send_delivery_state(intent, &body),
    })
}

/// Ask a provider for the authoritative state of a request whose network
/// result was unknown. A status endpoint is required for enabled production
/// actions so a Worker restart cannot silently re-run an external side effect.
pub async fn status(
    _env: &Env,
    config: &ActionProviderConfig,
    intent: &str,
    idempotency_key: &str,
    payload: Value,
) -> ApiResult<ProviderStatus> {
    if config.mode != ActionProviderMode::External {
        return Err(unavailable(config.mode, intent));
    }
    let endpoint = config
        .status_endpoint(intent)
        .ok_or_else(|| unavailable(config.mode, intent))?;
    let token = config
        .token(intent)
        .ok_or_else(|| unavailable(config.mode, intent))?;
    let body = post_json(endpoint, token, intent, idempotency_key, payload).await?;
    let state = body
        .get("state")
        .or_else(|| body.get("status"))
        .or_else(|| body.get("delivery_state"))
        .and_then(Value::as_str)
        .map(|raw| status_delivery_state(intent, raw))
        .unwrap_or(ProviderDeliveryState::Unknown);
    let provider_id = provider_id_from_body(&body, intent);
    Ok(ProviderStatus { state, provider_id })
}

/// Cancel a provider-side reversible effect. Local state is changed only
/// after this operation receives a successful provider response.
pub async fn cancel(
    _env: &Env,
    config: &ActionProviderConfig,
    intent: &str,
    idempotency_key: &str,
    payload: Value,
) -> ApiResult<ProviderResponse> {
    if config.mode != ActionProviderMode::External {
        return Err(unavailable(config.mode, intent));
    }
    let endpoint = config
        .cancel_endpoint(intent)
        .ok_or_else(|| unavailable(config.mode, intent))?;
    let token = config
        .token(intent)
        .ok_or_else(|| unavailable(config.mode, intent))?;
    let body = post_json(endpoint, token, intent, idempotency_key, payload).await?;
    let provider_id = provider_id_from_body(&body, intent);
    Ok(ProviderResponse {
        provider_id,
        state: ProviderDeliveryState::Succeeded,
    })
}

fn provider_id_from_body(body: &Value, intent: &str) -> Option<String> {
    let keys: &[&str] = if intent == "send_message" {
        &["provider_id", "id", "message_id"]
    } else {
        &["provider_id", "id", "reminder_id"]
    };
    keys.iter().find_map(|key| {
        body.get(*key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    })
}

fn send_delivery_state(intent: &str, body: &Value) -> ProviderDeliveryState {
    let raw = body
        .get("state")
        .or_else(|| body.get("status"))
        .or_else(|| body.get("delivery_state"))
        .and_then(Value::as_str);
    let Some(raw) = raw else {
        // A reminder delivery endpoint creates a scheduled provider resource;
        // a message endpoint may only have accepted an asynchronous send.
        return if intent == "send_message" {
            ProviderDeliveryState::Pending
        } else {
            ProviderDeliveryState::Succeeded
        };
    };
    if intent == "create_reminder"
        && matches!(
            raw.trim().to_ascii_lowercase().as_str(),
            "accepted" | "scheduled"
        )
    {
        return ProviderDeliveryState::Succeeded;
    }
    parse_delivery_state(raw)
}

fn status_delivery_state(intent: &str, raw: &str) -> ProviderDeliveryState {
    if intent == "create_reminder"
        && matches!(
            raw.trim().to_ascii_lowercase().as_str(),
            "accepted" | "scheduled"
        )
    {
        ProviderDeliveryState::Succeeded
    } else {
        parse_delivery_state(raw)
    }
}

fn parse_delivery_state(raw: &str) -> ProviderDeliveryState {
    match raw.trim().to_ascii_lowercase().as_str() {
        "succeeded" | "success" | "sent" | "delivered" | "complete" | "completed" => {
            ProviderDeliveryState::Succeeded
        }
        "accepted" | "pending" | "queued" | "processing" | "running" | "scheduled" => {
            ProviderDeliveryState::Pending
        }
        "failed" | "failure" | "rejected" | "cancelled" | "canceled" | "expired" => {
            ProviderDeliveryState::Failed
        }
        _ => ProviderDeliveryState::Unknown,
    }
}

async fn post_json(
    endpoint: &str,
    token: &str,
    intent: &str,
    idempotency_key: &str,
    payload: Value,
) -> ApiResult<Value> {
    let headers = Headers::new();
    headers.set("accept", "application/json")?;
    headers.set("content-type", "application/json")?;
    headers.set("authorization", &format!("Bearer {token}"))?;
    headers.set("x-idempotency-key", idempotency_key)?;
    headers.set("x-knock-knock-intent", intent)?;

    let mut init = RequestInit::new();
    init.with_method(Method::Post).with_headers(headers);
    init.with_body(Some(worker::wasm_bindgen::JsValue::from_str(
        &payload.to_string(),
    )));
    let request = Request::new_with_init(endpoint, &init).map_err(|_| {
        ApiError::new(
            503,
            "provider_network_error",
            "Provider request could not be created",
        )
    })?;
    let mut response = Fetch::Request(request)
        .send()
        .await
        .map_err(|_| ApiError::new(503, "provider_network_error", "Provider request failed"))?;
    let status = response.status_code();
    let raw = response.text().await.unwrap_or_default();
    if !(200..300).contains(&status) {
        let mapped_status = if status == 429 {
            429
        } else if status == 408 || status == 425 || status >= 500 {
            503
        } else {
            424
        };
        return Err(ApiError::new(
            mapped_status,
            "provider_rejected",
            "The configured provider rejected the request",
        ));
    }
    Ok(if raw.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str::<Value>(&raw).unwrap_or_else(|_| json!({}))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_mode_is_explicit_and_fail_closed() {
        assert_eq!(
            parse_mode("internal").unwrap(),
            ActionProviderMode::Internal
        );
        assert_eq!(
            parse_mode(" external ").unwrap(),
            ActionProviderMode::External
        );
        assert_eq!(
            parse_mode("DISABLED").unwrap(),
            ActionProviderMode::Disabled
        );
        assert!(parse_mode("sendgrid").is_err());
    }

    #[test]
    fn external_provider_is_not_reported_ready() {
        let internal = ActionProviderConfig {
            mode: ActionProviderMode::Internal,
            reminder_enabled: true,
            message_enabled: true,
            reminder_url: None,
            message_url: None,
            reminder_cancel_url: None,
            reminder_status_url: None,
            message_status_url: None,
            reminder_token: None,
            message_token: None,
        };
        let external = ActionProviderConfig {
            mode: ActionProviderMode::External,
            reminder_enabled: true,
            message_enabled: true,
            reminder_url: None,
            message_url: None,
            reminder_cancel_url: None,
            reminder_status_url: None,
            message_status_url: None,
            reminder_token: None,
            message_token: None,
        };
        let disabled = ActionProviderConfig {
            mode: ActionProviderMode::Disabled,
            reminder_enabled: true,
            message_enabled: true,
            reminder_url: None,
            message_url: None,
            reminder_cancel_url: None,
            reminder_status_url: None,
            message_status_url: None,
            reminder_token: None,
            message_token: None,
        };
        assert!(ready(&internal));
        assert!(!ready(&external));
        assert!(!ready(&disabled));
    }

    #[test]
    fn disabled_actions_are_permanent_conflicts() {
        let error = disabled("send_message");
        assert_eq!(error.status, 409);
        assert!(!error.retryable);
    }

    #[test]
    fn external_action_requires_endpoint_and_secret() {
        assert!(validate_external_action("send_message", false, None, None, None, None).is_ok());
        assert!(validate_external_action(
            "send_message",
            true,
            None,
            None,
            Some("https://status"),
            Some("token")
        )
        .is_err());
        assert!(validate_external_action(
            "send_message",
            true,
            Some("https://provider"),
            None,
            Some("https://status"),
            None
        )
        .is_err());
        assert!(validate_external_action(
            "send_message",
            true,
            Some("https://provider"),
            None,
            Some("https://status"),
            Some("token")
        )
        .is_ok());
        assert!(validate_external_action(
            "create_reminder",
            true,
            Some("https://provider"),
            Some("https://cancel"),
            Some("https://status"),
            Some("token")
        )
        .is_ok());
    }

    #[test]
    fn provider_status_values_are_conservative() {
        assert_eq!(
            parse_delivery_state("delivered"),
            ProviderDeliveryState::Succeeded
        );
        assert_eq!(
            parse_delivery_state("processing"),
            ProviderDeliveryState::Pending
        );
        assert_eq!(
            parse_delivery_state("accepted"),
            ProviderDeliveryState::Pending
        );
        assert_eq!(
            parse_delivery_state("cancelled"),
            ProviderDeliveryState::Failed
        );
        assert_eq!(
            parse_delivery_state("vendor-specific"),
            ProviderDeliveryState::Unknown
        );
    }

    #[test]
    fn asynchronous_message_acceptance_requires_status_reconciliation() {
        assert_eq!(
            send_delivery_state("send_message", &json!({"provider_id": "msg-1"})),
            ProviderDeliveryState::Pending
        );
        assert_eq!(
            send_delivery_state(
                "send_message",
                &json!({"provider_id": "msg-1", "state": "accepted"})
            ),
            ProviderDeliveryState::Pending
        );
        assert_eq!(
            send_delivery_state(
                "send_message",
                &json!({"provider_id": "msg-1", "state": "delivered"})
            ),
            ProviderDeliveryState::Succeeded
        );
    }

    #[test]
    fn scheduled_reminder_response_is_success_only_as_a_provider_resource() {
        assert_eq!(
            send_delivery_state("create_reminder", &json!({"provider_id": "rem-1"})),
            ProviderDeliveryState::Succeeded
        );
        assert_eq!(
            send_delivery_state(
                "create_reminder",
                &json!({"provider_id": "rem-1", "state": "scheduled"})
            ),
            ProviderDeliveryState::Succeeded
        );
    }

    #[test]
    fn scoped_provider_keys_are_stable_and_user_bound() {
        let first = scoped_idempotency_key("user-a", "action.reminder", "same-key");
        assert_eq!(
            first,
            scoped_idempotency_key("user-a", "action.reminder", "same-key")
        );
        assert_ne!(
            first,
            scoped_idempotency_key("user-b", "action.reminder", "same-key")
        );
        assert_ne!(
            first,
            scoped_idempotency_key("user-a", "action.message", "same-key")
        );
        assert!(first.starts_with("kk_"));
        assert_eq!(first.len(), 67);
        assert_eq!(
            first,
            scoped_action_idempotency_key("user-a", "create_reminder", "same-key")
        );
    }
}
