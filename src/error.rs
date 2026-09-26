//! Application error taxonomy and mapping to MCP error payloads.

use serde_json::json;
use thiserror::Error;

/// Errors produced by the UIA engine layer and the tool layer.
///
/// Every variant maps to a stable machine-readable `code` string that is
/// embedded in the JSON error body returned to MCP clients.
#[derive(Debug, Error)]
pub enum AppError {
    /// Tool arguments failed validation (bad locator, bad enum value, ...).
    #[error("invalid parameters: {0}")]
    InvalidParams(String),

    /// No element matched a locator / reference resolution.
    #[error("element not found: {0}")]
    ElementNotFound(String),

    /// A reference's stored query no longer resolves to a unique element.
    #[error("stale element: {0}")]
    StaleElement(String),

    /// The element does not support the required UI Automation pattern.
    #[error("unsupported pattern: {0}")]
    UnsupportedPattern(String),

    /// An operation did not complete within its deadline.
    #[error("timeout: {0}")]
    Timeout(String),

    /// A UI Automation / COM call failed.
    #[error("uia error: {0}")]
    Uia(String),

    /// Any other internal failure.
    #[error("internal error: {0}")]
    Internal(String),
}

impl AppError {
    /// Stable machine-readable error code.
    pub fn code(&self) -> &'static str {
        match self {
            AppError::InvalidParams(_) => "InvalidParams",
            AppError::ElementNotFound(_) => "ElementNotFound",
            AppError::StaleElement(_) => "StaleElement",
            AppError::UnsupportedPattern(_) => "UnsupportedPattern",
            AppError::Timeout(_) => "Timeout",
            AppError::Uia(_) => "Uia",
            AppError::Internal(_) => "Internal",
        }
    }

    /// Human-readable message.
    pub fn message(&self) -> String {
        self.to_string()
    }

    /// Optional hint telling the model how to recover.
    pub fn hint(&self) -> Option<&'static str> {
        match self {
            AppError::InvalidParams(_) => {
                Some("Check the tool schema and the locator syntax reference in the README.")
            }
            AppError::ElementNotFound(_) => Some(
                "Re-run snapshot or find_elements to discover the current element refs, then retry.",
            ),
            AppError::StaleElement(_) => Some(
                "The element reference is out of date. Re-run find_elements to obtain a fresh ref and retry.",
            ),
            AppError::UnsupportedPattern(_) => Some(
                "Inspect the element with describe to see which patterns it supports.",
            ),
            AppError::Timeout(_) => Some(
                "The UI did not reach the expected state in time. Increase timeout_ms or poll_ms.",
            ),
            AppError::Uia(_) => Some(
                "The target application may be busy, hung or running at a higher integrity level (run the server elevated to access elevated targets).",
            ),
            AppError::Internal(_) => None,
        }
    }

    /// Serialize to the standard error JSON body:
    /// `{"error": {"code": "...", "message": "...", "hint": "..."}}`
    pub fn to_json_body(&self) -> serde_json::Value {
        let mut body = json!({
            "error": {
                "code": self.code(),
                "message": self.message(),
            }
        });
        if let Some(hint) = self.hint() {
            body["error"]["hint"] = json!(hint);
        }
        body
    }
}

impl From<uiautomation::Error> for AppError {
    fn from(e: uiautomation::Error) -> Self {
        AppError::Uia(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_codes_are_stable() {
        assert_eq!(AppError::InvalidParams("x".into()).code(), "InvalidParams");
        assert_eq!(AppError::ElementNotFound("x".into()).code(), "ElementNotFound");
        assert_eq!(AppError::StaleElement("x".into()).code(), "StaleElement");
        assert_eq!(AppError::UnsupportedPattern("x".into()).code(), "UnsupportedPattern");
        assert_eq!(AppError::Timeout("x".into()).code(), "Timeout");
        assert_eq!(AppError::Uia("x".into()).code(), "Uia");
        assert_eq!(AppError::Internal("x".into()).code(), "Internal");
    }

    #[test]
    fn error_body_shape() {
        let body = AppError::ElementNotFound("no Button".into()).to_json_body();
        assert_eq!(body["error"]["code"], "ElementNotFound");
        assert!(body["error"]["message"].as_str().unwrap().contains("no Button"));
        assert!(body["error"]["hint"].is_string());
    }
}
