//! Agent boot-time configuration knobs.
//!
//! Everything here is a pure function over env vars + defaults so it can be
//! unit-tested without spinning the agent. `main.rs` should call into here
//! and never re-parse env directly.

use std::env;

const DEFAULT_API_URL: &str = "https://api.cirun.io/api/v1";

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("invalid CIRUN_API_URL '{0}': {1}")]
    InvalidApiUrl(String, String),
    #[error(
        "refusing insecure CIRUN_API_URL '{0}' — scheme must be https. \
         Set CIRUN_AGENT_INSECURE_HTTP=1 to allow http (dev only)."
    )]
    InsecureScheme(String),
}

/// Resolve and validate the cirun api base URL. Reads `CIRUN_API_URL` (defaults
/// to production https), then enforces an https scheme unless
/// `CIRUN_AGENT_INSECURE_HTTP=1` is set (local-dev escape hatch). Returns the
/// canonical url string the rest of the agent should use.
///
/// Why: the bearer token + the verbatim `provision_script` flow over this URL.
/// An accidental `http://...` setting would leak the token and let any
/// on-path attacker hand the agent arbitrary code to run as root.
pub fn resolve_api_url() -> Result<String, ConfigError> {
    let raw = env::var("CIRUN_API_URL").unwrap_or_else(|_| DEFAULT_API_URL.to_string());
    let allow_http = env::var("CIRUN_AGENT_INSECURE_HTTP")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    validate_api_url(&raw, allow_http).map(|_| raw)
}

fn validate_api_url(raw: &str, allow_http: bool) -> Result<(), ConfigError> {
    let parsed = url::Url::parse(raw)
        .map_err(|e| ConfigError::InvalidApiUrl(raw.to_string(), e.to_string()))?;
    match parsed.scheme() {
        "https" => Ok(()),
        "http" if allow_http => Ok(()),
        "http" => Err(ConfigError::InsecureScheme(raw.to_string())),
        other => Err(ConfigError::InvalidApiUrl(
            raw.to_string(),
            format!("unsupported scheme '{other}'"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn https_is_accepted() {
        assert!(validate_api_url("https://api.cirun.io/api/v1", false).is_ok());
        assert!(validate_api_url("https://localhost:7777", false).is_ok());
    }

    #[test]
    fn plain_http_is_rejected_by_default() {
        let err = validate_api_url("http://localhost:7777", false).unwrap_err();
        assert!(matches!(err, ConfigError::InsecureScheme(_)));
    }

    #[test]
    fn http_is_allowed_with_escape_hatch() {
        assert!(validate_api_url("http://localhost:7777", true).is_ok());
    }

    #[test]
    fn other_schemes_are_rejected() {
        for s in [
            "ftp://api.cirun.io",
            "file:///etc/passwd",
            "javascript:alert(1)",
        ] {
            let err = validate_api_url(s, true).unwrap_err();
            assert!(matches!(err, ConfigError::InvalidApiUrl(_, _)));
        }
    }

    #[test]
    fn malformed_url_is_rejected() {
        let err = validate_api_url("not a url", true).unwrap_err();
        assert!(matches!(err, ConfigError::InvalidApiUrl(_, _)));
    }
}
