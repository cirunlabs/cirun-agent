use reqwest::Error as ReqwestError;
use serde::de::StdError;
use std::fmt;

#[derive(Debug)]
pub enum LumeError {
    RequestError(ReqwestError),
    ApiError(String),
}

impl fmt::Display for LumeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LumeError::RequestError(err) => write!(f, "Request error: {}", err),
            LumeError::ApiError(msg) => write!(f, "API error: {}", msg),
        }
    }
}

impl StdError for LumeError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            LumeError::RequestError(err) => Some(err),
            LumeError::ApiError(_) => None,
        }
    }
}

impl From<ReqwestError> for LumeError {
    fn from(error: ReqwestError) -> Self {
        LumeError::RequestError(error)
    }
}
