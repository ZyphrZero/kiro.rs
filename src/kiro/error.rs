//! Shared typed errors for Kiro upstream calls.

/// HTTP 429 for the client, with an optional Retry-After.
///
/// Usually the upstream still returned 429 after any applicable failover/retry. The same type also
/// carries the gateway's own per-account RPM rejection (no request reached Kiro), so every caller
/// keeps mapping it to 429; `is_local` only changes how it is labelled in traces and logs.
#[derive(Debug, Clone)]
pub struct UpstreamRateLimitError {
    retry_after: Option<String>,
    local: bool,
}

impl std::fmt::Display for UpstreamRateLimitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.local {
            "local per-account RPM limit reached"
        } else {
            "upstream rate limited"
        })
    }
}

impl std::error::Error for UpstreamRateLimitError {}

/// Kiro rejected the serialized conversation because it exceeded the model's
/// context limit. Keeping this typed at the provider boundary lets compact
/// retry once without reverse-engineering an HTTP error response.
#[derive(Debug, Clone, thiserror::Error)]
#[error("upstream context window exceeded")]
pub struct UpstreamContextOverflowError;

/// Model capacity is temporarily unavailable, independently of account health.
#[derive(Debug, Clone, thiserror::Error)]
#[error("upstream model temporarily unavailable")]
pub struct UpstreamModelUnavailableError {
    pub retry_after: String,
}

#[derive(Debug, thiserror::Error)]
#[error("upstream response deadline exceeded")]
pub struct UpstreamTimeoutError {
    #[source]
    pub source: reqwest::Error,
}

impl UpstreamRateLimitError {
    pub(crate) fn new(retry_after: Option<String>) -> Self {
        Self {
            retry_after: retry_after.and_then(normalize_retry_after),
            local: false,
        }
    }

    /// The gateway's own per-account RPM limit is full: rejected before any upstream call.
    pub(crate) fn local_rpm_limit(retry_after_secs: u64) -> Self {
        Self {
            retry_after: Some(retry_after_secs.to_string()),
            local: true,
        }
    }

    /// True when the gateway rejected the request itself instead of Kiro returning 429.
    pub fn is_local(&self) -> bool {
        self.local
    }

    pub(crate) fn from_headers(headers: &http::HeaderMap) -> Self {
        let retry_after = headers
            .get(http::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        Self::new(retry_after)
    }

    pub fn retry_after(&self) -> Option<&str> {
        self.retry_after.as_deref()
    }

    /// Without an explicit upstream delay, a short local retry is still useful.
    pub(crate) fn should_retry_locally(&self) -> bool {
        self.retry_after.is_none()
    }
}

fn normalize_retry_after(value: String) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }

    if value.parse::<u64>().is_ok() || httpdate::parse_http_date(value).is_ok() {
        Some(value.to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_delta_seconds_and_http_date() {
        let seconds = UpstreamRateLimitError::new(Some(" 1800 ".to_string()));
        assert_eq!(seconds.retry_after(), Some("1800"));
        assert!(!seconds.should_retry_locally());

        let date = "Sun, 12 Jul 2026 02:30:00 GMT";
        let http_date = UpstreamRateLimitError::new(Some(date.to_string()));
        assert_eq!(http_date.retry_after(), Some(date));
        assert!(!http_date.should_retry_locally());
    }

    #[test]
    fn rejects_invalid_retry_after() {
        let error = UpstreamRateLimitError::new(Some("not-a-retry-delay".to_string()));
        assert_eq!(error.retry_after(), None);
        assert!(error.should_retry_locally());
    }

    #[test]
    fn local_rpm_limit_is_labelled_apart_from_upstream_429() {
        let local = UpstreamRateLimitError::local_rpm_limit(42);
        assert!(local.is_local());
        assert_eq!(local.retry_after(), Some("42"));
        assert!(!local.should_retry_locally());
        assert_eq!(local.to_string(), "local per-account RPM limit reached");

        let upstream = UpstreamRateLimitError::new(Some("42".to_string()));
        assert!(!upstream.is_local());
        assert_eq!(upstream.to_string(), "upstream rate limited");
    }
}
