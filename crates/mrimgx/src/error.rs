//! Error type of the crate.

use std::io;

/// Result alias used throughout the crate.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors returned when opening or reading a Reflect X image.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Underlying I/O failure.
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    /// The file is not a Reflect X image or its structure cannot be parsed.
    #[error("not a valid Reflect X image: {0}")]
    BadFormat(String),
    /// A feature of the image is not supported by this reader.
    #[error("unsupported image: {0}")]
    Unsupported(String),
    /// The image is encrypted and no password was given.
    #[error("the image is encrypted and needs a password")]
    PasswordRequired,
    /// The password does not match the image.
    #[error("wrong password")]
    WrongPassword,
    /// A file of the backup set (split part or earlier increment) could not be found.
    #[error("missing backup set file: {0}")]
    MissingFile(String),
    /// A checksum mismatch or an inconsistent structure was found.
    #[error("corrupt image: {0}")]
    Corrupt(String),
    /// A disk or partition index is out of range.
    #[error("not found: {0}")]
    NotFound(String),
}

impl Error {
    pub(crate) fn corrupt(msg: impl Into<String>) -> Self {
        Self::Corrupt(msg.into())
    }
}

impl From<Error> for io::Error {
    fn from(e: Error) -> Self {
        match e {
            Error::Io(e) => e,
            Error::Corrupt(_) | Error::BadFormat(_) => io::Error::new(io::ErrorKind::InvalidData, e),
            Error::MissingFile(_) | Error::NotFound(_) => io::Error::new(io::ErrorKind::NotFound, e),
            e => io::Error::other(e),
        }
    }
}
