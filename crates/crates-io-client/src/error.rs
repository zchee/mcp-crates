//! Error types for crates.io data access.
//!
//! The type deliberately carries no dependency on any RPC framework: callers
//! translate [`Error::category`] and [`Error::kind`] into whatever their own
//! protocol expects.

/// Broad classification of a failure, for callers that need to map errors onto
/// their own protocol's error codes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Category {
    /// The caller supplied something invalid; retrying will not help.
    InvalidInput,
    /// The requested resource does not exist.
    NotFound,
    /// Something went wrong talking to an upstream host.
    Upstream,
}

/// Errors produced while reading crates.io data.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// A crate name did not satisfy the crates.io naming rules.
    #[error("invalid crate name {name:?}: names must be 1-64 characters of [A-Za-z0-9_-]")]
    InvalidCrateName {
        /// The rejected name.
        name: String,
    },

    /// A version string or requirement could not be parsed.
    #[error("invalid version selector {value:?}: {reason}")]
    InvalidVersion {
        /// The rejected selector.
        value: String,
        /// Why parsing failed.
        reason: String,
    },

    /// A caller argument was outside its accepted range.
    #[error("invalid argument: {0}")]
    InvalidArgument(String),

    /// The crate does not exist on crates.io.
    #[error("crate {name:?} was not found on crates.io")]
    CrateNotFound {
        /// The crate that was looked up.
        name: String,
    },

    /// The crate exists but no version matches the selector.
    #[error("crate {name:?} has no version matching {selector:?}")]
    VersionNotFound {
        /// The crate that was looked up.
        name: String,
        /// The version or requirement that matched nothing.
        selector: String,
    },

    /// Documentation is not available for the requested crate version.
    #[error("no documentation is available for {name:?} {version}: {reason}")]
    DocsUnavailable {
        /// The crate that was looked up.
        name: String,
        /// The resolved version.
        version: String,
        /// Why documentation could not be served.
        reason: String,
    },

    /// An upstream host answered with an unexpected status code.
    #[error("{url} returned HTTP {status}{}", detail_suffix(.detail))]
    Upstream {
        /// The request URL.
        url: String,
        /// The HTTP status code.
        status: u16,
        /// Human-readable detail extracted from the response body, if any.
        detail: Option<String>,
    },

    /// A transport-level failure occurred.
    #[error("network failure while requesting {url}: {source}")]
    Network {
        /// The request URL.
        url: String,
        /// The underlying transport error.
        #[source]
        source: reqwest::Error,
    },

    /// A response body could not be decoded into the expected shape.
    #[error("could not decode the response from {url}: {message}")]
    Decode {
        /// The request URL.
        url: String,
        /// What went wrong.
        message: String,
    },

    /// A response body exceeded the configured size ceiling.
    #[error("the response from {url} exceeded the {limit} byte ceiling")]
    BodyTooLarge {
        /// The request URL.
        url: String,
        /// The configured ceiling in bytes.
        limit: usize,
    },

    /// The local rate-limit queue is saturated.
    ///
    /// Reported instead of queueing indefinitely, so a caller gets a prompt,
    /// actionable answer rather than an unbounded stall.
    #[error(
        "request to {host} was shed: the local rate-limit queue already extends {queued_ms}ms \
         into the future (ceiling {ceiling_ms}ms)"
    )]
    RateLimitQueueFull {
        /// The host whose budget is saturated.
        host: String,
        /// How far ahead the queue already extends.
        queued_ms: u64,
        /// The configured ceiling.
        ceiling_ms: u64,
    },

    /// A URL was malformed, or a redirect left the set of permitted hosts.
    #[error("refusing to request {url}: {reason}")]
    UnsupportedUrl {
        /// The offending URL.
        url: String,
        /// Why it was refused.
        reason: String,
    },
}

fn detail_suffix(detail: &Option<String>) -> String {
    match detail {
        Some(d) if !d.is_empty() => format!(": {d}"),
        _ => String::new(),
    }
}

impl Error {
    /// Broad classification, for mapping onto a caller's own error codes.
    #[must_use]
    pub fn category(&self) -> Category {
        match self {
            Self::InvalidCrateName { .. }
            | Self::InvalidVersion { .. }
            | Self::InvalidArgument(_) => Category::InvalidInput,
            Self::CrateNotFound { .. }
            | Self::VersionNotFound { .. }
            | Self::DocsUnavailable { .. } => Category::NotFound,
            _ => Category::Upstream,
        }
    }

    /// A stable machine-readable discriminant, suitable for structured error
    /// payloads so that callers can branch without parsing prose.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::InvalidCrateName { .. } => "invalid_crate_name",
            Self::InvalidVersion { .. } => "invalid_version",
            Self::InvalidArgument(_) => "invalid_argument",
            Self::CrateNotFound { .. } => "crate_not_found",
            Self::VersionNotFound { .. } => "version_not_found",
            Self::DocsUnavailable { .. } => "docs_unavailable",
            Self::Upstream { .. } => "upstream_error",
            Self::Network { .. } => "network_error",
            Self::Decode { .. } => "decode_error",
            Self::BodyTooLarge { .. } => "body_too_large",
            Self::RateLimitQueueFull { .. } => "rate_limited",
            Self::UnsupportedUrl { .. } => "unsupported_url",
        }
    }

    /// Whether repeating the same request later has a realistic chance of
    /// succeeding.
    #[must_use]
    pub fn retryable(&self) -> bool {
        match self {
            Self::Network { .. } | Self::RateLimitQueueFull { .. } => true,
            Self::Upstream { status, .. } => *status == 429 || *status >= 500,
            _ => false,
        }
    }

    /// Whether an automatic in-process retry is worthwhile.
    ///
    /// Distinct from [`Error::retryable`]: a shed request is retryable by the
    /// caller after a pause, but retrying it immediately would only be shed
    /// again.
    #[must_use]
    pub(crate) fn is_transient(&self) -> bool {
        match self {
            Self::Network { .. } => true,
            Self::Upstream { status, .. } => *status == 429 || *status >= 500,
            _ => false,
        }
    }
}

/// Convenience alias for fallible operations in this crate.
pub type Result<T, E = Error> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn categories_match_the_variant_intent() {
        let invalid = Error::InvalidCrateName {
            name: "not a crate".into(),
        };
        assert_eq!(invalid.category(), Category::InvalidInput);
        assert!(!invalid.retryable());

        let missing = Error::CrateNotFound {
            name: "nope".into(),
        };
        assert_eq!(missing.category(), Category::NotFound);

        let flaky = Error::Upstream {
            url: "https://crates.io/api/v1/crates".into(),
            status: 503,
            detail: None,
        };
        assert_eq!(flaky.category(), Category::Upstream);
        assert!(flaky.retryable() && flaky.is_transient());

        let shed = Error::RateLimitQueueFull {
            host: "crates.io".into(),
            queued_ms: 40_000,
            ceiling_ms: 30_000,
        };
        assert!(shed.retryable(), "the caller may retry after a pause");
        assert!(
            !shed.is_transient(),
            "an immediate in-process retry would be shed again"
        );
    }

    #[test]
    fn upstream_detail_is_appended_only_when_present() {
        let bare = Error::Upstream {
            url: "https://x.invalid".into(),
            status: 500,
            detail: None,
        };
        assert_eq!(bare.to_string(), "https://x.invalid returned HTTP 500");

        let detailed = Error::Upstream {
            url: "https://x.invalid".into(),
            status: 404,
            detail: Some("Not Found".into()),
        };
        assert_eq!(
            detailed.to_string(),
            "https://x.invalid returned HTTP 404: Not Found"
        );
    }
}
