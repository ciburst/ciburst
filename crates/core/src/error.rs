use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("invalid config:\n  {}", .0.join("\n  "))]
    Config(Vec<String>),
    #[error("gitlab {method} {path}: {status} {message}")]
    Gitlab {
        status: u16,
        method: String,
        path: String,
        message: String,
    },
    #[error("http: {0}")]
    Http(String),
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Provider(String),
    /// A provider call that may succeed when repeated: 429, 503, or for an idempotent call
    /// another 5xx or a transport timeout (spec 6.6).
    #[error("{0}")]
    Retryable(String),
    #[error("{0}")]
    Other(String),
}

impl Error {
    pub fn provider(message: impl Into<String>) -> Self {
        Error::Provider(message.into())
    }

    pub fn other(message: impl Into<String>) -> Self {
        Error::Other(message.into())
    }

    pub fn retryable(message: impl Into<String>) -> Self {
        Error::Retryable(message.into())
    }

    /// Whether repeating the call may succeed (spec 6.6).
    pub fn is_retryable(&self) -> bool {
        matches!(self, Error::Retryable(_))
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_retryable_errors_are_retryable() {
        assert!(Error::retryable("scaleway: 503").is_retryable());
        assert_eq!(
            Error::retryable("scaleway: 503").to_string(),
            "scaleway: 503"
        );
        assert!(!Error::provider("scaleway: 400").is_retryable());
        assert!(!Error::other("x").is_retryable());
    }
}
