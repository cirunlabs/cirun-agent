use reqwest::Error as ReqwestError;
use serde::de::StdError;
use std::fmt;

#[derive(Debug)]
pub enum MedaError {
    RequestError(ReqwestError),
    ApiError(String),
}

impl fmt::Display for MedaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MedaError::RequestError(err) => write!(f, "Request error: {}", err),
            MedaError::ApiError(msg) => write!(f, "API error: {}", msg),
        }
    }
}

impl StdError for MedaError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            MedaError::RequestError(err) => Some(err),
            MedaError::ApiError(_) => None,
        }
    }
}

impl From<ReqwestError> for MedaError {
    fn from(error: ReqwestError) -> Self {
        MedaError::RequestError(error)
    }
}
