//! Userspace iSCSI target and Microsoft iSCSI initiator control.
mod chap;
mod login;
mod pdu;
mod scsi;
mod server;

#[cfg(windows)]
pub mod initiator;

use blockdev::BlockDevice;
pub use chap::{generate_chap_secret, ChapSecret};
pub use server::TargetServer;
use std::{io, net::SocketAddr, sync::Arc};

/// A disk exposed as a numbered logical unit (its index in `TargetConfig::luns`).
pub struct LunConfig {
    /// Backing device.
    pub device: Arc<dyn BlockDevice>,
    /// ASCII inquiry vendor, at most eight bytes.
    pub vendor: String,
    /// ASCII inquiry product, at most sixteen bytes.
    pub product: String,
    /// ASCII inquiry revision, at most four bytes.
    pub revision: String,
    /// ASCII VPD serial number, at most 252 bytes.
    pub serial: String,
    /// Stable identifier, advertised as a sixteen-byte NAA-6 designator.
    pub device_id: [u8; 16],
}

/// Listener, authentication and disk configuration.
pub struct TargetConfig {
    /// Listener address; port zero requests an ephemeral port.
    pub bind: SocketAddr,
    /// Target's qualified name.
    pub iqn: String,
    /// One-way CHAP credentials, or unauthenticated login.
    pub chap: Option<ChapSecret>,
    /// Logical units, numbered consecutively from zero.
    pub luns: Vec<LunConfig>,
}

/// Construct an image target name. The caller must supply an IQN-safe identifier.
pub fn iqn_for(id: &str) -> String {
    format!("iqn.2026-01.org.inocrium:image-{id}")
}

/// Errors from initiator management.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// An iscsidsc or service-manager Win32 error.
    #[error("{operation}: Win32 error {code}")]
    Win32 {
        /// Operation that failed.
        operation: &'static str,
        /// Native error code.
        code: u32,
    },
    /// Invalid caller-supplied configuration.
    #[error("invalid configuration: {0}")]
    InvalidConfig(String),
    /// The device or service did not appear before the deadline.
    #[error("timed out waiting for {0}")]
    Timeout(&'static str),
    /// A local I/O failure.
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Crate result type.
pub type Result<T> = std::result::Result<T, Error>;
