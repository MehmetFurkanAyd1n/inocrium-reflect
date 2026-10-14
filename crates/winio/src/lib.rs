//! Windows storage, security and host primitives for disk imaging.
#![cfg(windows)]

mod disk;
mod handle;
mod secrets;
mod system;
mod vhd;
mod volume;

pub use disk::*;
pub use secrets::*;
pub use system::*;
pub use vhd::*;
pub use volume::*;

/// Result of a Windows operation.
pub type Result<T> = std::result::Result<T, Error>;

/// Contextual storage or Win32 failure.
#[derive(thiserror::Error, Debug)]
pub enum Error {
    /// Standard I/O failure.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// Win32 failure with operation context.
    #[error("{context}: {source}")]
    Win32 { context: String, source: windows::core::Error },
    /// Unsupported operation or invalid data.
    #[error("{0}")]
    Other(String),
}
impl From<Error> for std::io::Error {
    fn from(value: Error) -> Self {
        match value {
            Error::Io(e) => e,
            Error::Win32 { source, .. } if source.code().0 as u32 & 0xffff0000 == 0x80070000 => {
                Self::from_raw_os_error(source.code().0 & 0xffff)
            }
            e => Self::other(e),
        }
    }
}
pub(crate) fn win(context: impl Into<String>, source: windows::core::Error) -> Error {
    Error::Win32 { context: context.into(), source }
}
