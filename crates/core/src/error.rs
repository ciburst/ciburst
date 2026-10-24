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
}

pub type Result<T> = std::result::Result<T, Error>;
