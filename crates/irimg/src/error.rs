use std::io;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("not an inocrium image (bad magic)")]
    BadMagic,
    #[error("unsupported image format version {0}")]
    UnsupportedVersion(u32),
    #[error("image header is corrupt")]
    CorruptHeader,
    #[error("image footer is missing or corrupt; the image may be incomplete")]
    CorruptFooter,
    #[error("image metadata is corrupt")]
    CorruptMetadata,
    #[error("image file {0} is missing")]
    MissingFile(String),
    #[error("image file {0} belongs to a different image")]
    ForeignFile(String),
    #[error("the image is encrypted; a password is required")]
    PasswordRequired,
    #[error("wrong password")]
    WrongPassword,
    #[error("block {block} of stream {stream} on disk {disk} failed authentication or hash check")]
    CorruptBlock { disk: u32, stream: u32, block: u64 },
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

impl From<Error> for io::Error {
    fn from(e: Error) -> Self {
        match e {
            Error::Io(e) => e,
            other => io::Error::new(io::ErrorKind::InvalidData, other),
        }
    }
}
