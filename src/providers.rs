use worker::Env;

use crate::auth::config_value;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActionProviderConfig {
    mode: ActionProviderMode,
    reminder_enabled: bool,
    message_enabled: bool,
}

impl ActionProviderConfig {
    pub fn mode(self) -> ActionProviderMode {
        self.mode
    }

    pub fn enabled(self, intent: &str) -> bool {
        match intent {
            "create_reminder" => self.reminder_enabled,
            "send_message" => self.message_enabled,
            "create_draft" => true,
            _ => false,
        }
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
    Ok(ActionProviderConfig {
        mode,
        reminder_enabled: bool_value(env, "ACTION_REMINDER_ENABLED", enabled_default)?,
        message_enabled: bool_value(env, "ACTION_MESSAGE_ENABLED", enabled_default)?,
    })
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

pub fn ready(mode: ActionProviderMode) -> bool {
    // The internal D1 effect is intentionally not represented as external
    // delivery. A concrete external adapter must make this return true.
    matches!(mode, ActionProviderMode::Internal)
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
        assert!(ready(ActionProviderMode::Internal));
        assert!(!ready(ActionProviderMode::External));
        assert!(!ready(ActionProviderMode::Disabled));
    }

    #[test]
    fn disabled_actions_are_permanent_conflicts() {
        let error = disabled("send_message");
        assert_eq!(error.status, 409);
        assert!(!error.retryable);
    }
}
