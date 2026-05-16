use reqwest::Error as ReqwestError;
use serde::de::StdError;
use std::fmt;

#[derive(Debug)]
pub enum MedaError {
    RequestError(ReqwestError),
    ApiError(String),
    /// Meda admission-control rejection (HTTP 503). Distinguished from
    /// ApiError because the caller MUST treat it as transient
    /// backpressure rather than a real provision failure — the host is
    /// at capacity, not broken. `code` is the structured reason
    /// (MEM_EXHAUSTED / CPU_EXHAUSTED / DISK_EXHAUSTED), `message` is
    /// meda's operator-facing detail string, `retry_after_secs` comes
    /// from the response's Retry-After header (10 by meda default).
    HostFull {
        code: String,
        message: String,
        retry_after_secs: u64,
    },
}

impl fmt::Display for MedaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MedaError::RequestError(err) => write!(f, "Request error: {}", err),
            MedaError::ApiError(msg) => write!(f, "API error: {}", msg),
            MedaError::HostFull {
                code,
                message,
                retry_after_secs,
            } => write!(
                f,
                "Host at capacity ({code}): {message} (retry after {retry_after_secs}s)"
            ),
        }
    }
}

impl StdError for MedaError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            MedaError::RequestError(err) => Some(err),
            MedaError::ApiError(_) | MedaError::HostFull { .. } => None,
        }
    }
}

impl From<ReqwestError> for MedaError {
    fn from(error: ReqwestError) -> Self {
        MedaError::RequestError(error)
    }
}
