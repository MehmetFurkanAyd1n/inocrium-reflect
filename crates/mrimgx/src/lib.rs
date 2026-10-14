//! Read-only access to Macrium Reflect X (`.mrimgx`) disk images.
//!
//! Implements the layout published by Macrium at
//! <https://github.com/macrium/mrimgx_file_layout> (MIT): a `MACRIUM_FILE` footer pointing to
//! a list of metadata blocks (`$JSON` describing the backup), per-disk `$TRACK0`/`$EPT` blocks
//! and per-partition `$INDEX` blocks locating fixed-size data blocks, which may be zstd
//! compressed and AES encrypted. Split files and incremental/differential chains of a backup
//! set are resolved from the image's directory.
//!
//! ```no_run
//! use blockdev::BlockDevice;
//! use mrimgx::MrimgxImage;
//! # fn main() -> mrimgx::Result<()> {
//! let image = MrimgxImage::open("backup-00-00.mrimgx".as_ref(), None)?;
//! let disk = image.disk_device(0)?;
//! let mut mbr = [0; 512];
//! disk.read_at(0, &mut mbr)?;
//! # Ok(())
//! # }
//! ```

mod crypto;
mod device;
mod error;
mod format;
mod image;
mod info;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod testutil;

pub use error::{Error, Result};
pub use image::{DiskInfo, ImageInfo, MrimgxImage, PartitionInfo, VerifyResult};
