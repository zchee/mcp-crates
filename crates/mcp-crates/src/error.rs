//! Translating client errors into MCP wire errors.

use crates_io_client::{Category, Error};
use rmcp::{ErrorData, model::ErrorCode};
use serde_json::json;

/// Convert a client error into the MCP error the caller receives.
///
/// The structured payload carries a stable `kind` discriminant and a
/// `retryable` flag, so a caller can decide what to do without parsing the
/// human-readable message.
#[must_use]
pub fn to_error_data(err: &Error) -> ErrorData {
    let code = match err.category() {
        Category::InvalidInput => ErrorCode::INVALID_PARAMS,
        Category::NotFound => ErrorCode::RESOURCE_NOT_FOUND,
        Category::Upstream => ErrorCode::INTERNAL_ERROR,
        // `Category` is non-exhaustive; an unfamiliar class is reported as an
        // internal error rather than misclassified as the caller's fault.
        _ => ErrorCode::INTERNAL_ERROR,
    };
    ErrorData::new(
        code,
        err.to_string(),
        Some(json!({ "kind": err.kind(), "retryable": err.retryable() })),
    )
}

/// Report a caller mistake that never reached the client layer.
#[must_use]
pub fn invalid_argument(message: impl Into<String>) -> ErrorData {
    ErrorData::new(
        ErrorCode::INVALID_PARAMS,
        message.into(),
        Some(json!({ "kind": "invalid_argument", "retryable": false })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_categories_map_onto_json_rpc_codes() {
        let cases = [
            (Error::InvalidCrateName { name: "bad name".into() }, ErrorCode::INVALID_PARAMS),
            (Error::CrateNotFound { name: "nope".into() }, ErrorCode::RESOURCE_NOT_FOUND),
            (
                Error::Upstream { url: "https://crates.io".into(), status: 500, detail: None },
                ErrorCode::INTERNAL_ERROR,
            ),
        ];
        for (err, expected) in cases {
            assert_eq!(to_error_data(&err).code, expected, "{err}");
        }
    }

    #[test]
    fn the_payload_carries_a_stable_discriminant_and_retry_hint() {
        let err = Error::Upstream { url: "https://crates.io".into(), status: 503, detail: None };
        let data = to_error_data(&err).data.expect("payload present");

        assert_eq!(data["kind"], "upstream_error");
        assert_eq!(data["retryable"], true);
    }

    #[test]
    fn a_missing_crate_is_not_advertised_as_retryable() {
        let err = Error::CrateNotFound { name: "nope".into() };
        let data = to_error_data(&err).data.expect("payload present");

        assert_eq!(data["kind"], "crate_not_found");
        assert_eq!(data["retryable"], false);
    }
}
