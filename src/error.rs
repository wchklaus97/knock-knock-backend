use worker::{Error as WorkerError, Response};

#[derive(Debug, Clone)]
pub struct ApiError {
    pub status: u16,
    pub code: String,
    pub message: String,
    pub retryable: bool,
    pub retry_after: Option<u64>,
    pub request_id: Option<String>,
}

impl ApiError {
    pub fn new(status: u16, code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            status,
            code: code.into(),
            message: message.into(),
            retryable: matches!(status, 408 | 425 | 429) || status >= 500,
            retry_after: None,
            request_id: None,
        }
    }

    pub fn with_request_id(mut self, request_id: impl Into<String>) -> Self {
        self.request_id = Some(request_id.into());
        self
    }

    pub fn rate_limited(retry_after: u64) -> Self {
        let mut error = Self::new(429, "rate_limited", "Too many requests");
        error.retry_after = Some(retry_after);
        error
    }

    pub fn validation(message: impl Into<String>) -> Self {
        Self::new(400, "validation_error", message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(404, "not_found", message)
    }

    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(401, "unauthorized", message)
    }

    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::new(403, "forbidden", message)
    }

    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(409, "conflict", message)
    }

    pub fn gone(message: impl Into<String>) -> Self {
        Self::new(410, "gone", message)
    }

    pub fn session(message: impl Into<String>, status: u16) -> Self {
        Self::new(status, "session_error", message)
    }

    pub fn action(message: impl Into<String>, status: u16) -> Self {
        Self::new(status, "action_error", message)
    }

    pub fn response(&self) -> worker::Result<Response> {
        let message = if self.status >= 500 {
            "Internal server error"
        } else {
            &self.message
        };
        let request_id = self.request_id.as_deref().unwrap_or("req_unassigned");
        let body = serde_json::json!({
            "error": {
                "code": self.code,
                "message": message,
                "retryable": self.retryable,
                "request_id": request_id,
                "retry_after": self.retry_after,
                "details": {},
            },
        });
        Ok(Response::from_json(&body)?.with_status(self.status))
    }
}

impl From<WorkerError> for ApiError {
    fn from(error: WorkerError) -> Self {
        Self::new(500, "internal_error", format!("{error:?}"))
    }
}

impl From<serde_json::Error> for ApiError {
    fn from(error: serde_json::Error) -> Self {
        Self::validation(error.to_string())
    }
}

pub type ApiResult<T> = std::result::Result<T, ApiError>;
