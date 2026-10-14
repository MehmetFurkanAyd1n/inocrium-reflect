use std::io;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Image(#[from] irimg::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("operation cancelled")]
    Cancelled,
    #[error("{0}")]
    Invalid(String),
}

impl Error {
    pub fn invalid(msg: impl Into<String>) -> Self {
        Error::Invalid(msg.into())
    }
}

impl From<Error> for io::Error {
    fn from(e: Error) -> Self {
        match e {
            Error::Io(e) => e,
            Error::Image(e) => e.into(),
            other => io::Error::other(other),
        }
    }
}
