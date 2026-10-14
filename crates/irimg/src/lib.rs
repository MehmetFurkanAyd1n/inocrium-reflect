//! The `.irimg` disk image format.
//!
//! An image is a sequence of one or more files (`name.irimg`, `name.i01`, `name.i02`, ...).
//! Every file starts with a fixed [`FileHeader`]. Data blocks follow back to back; each block
//! is independently compressed (zstd) and, when the image is encrypted, sealed with
//! AES-256-GCM. The last file ends with an encoded metadata blob (manifest JSON, raw disk
//! regions such as TRACK0 and the GPT backup area, and the block index) followed by a fixed
//! [`Footer`] that points at it. Because the metadata is written last the image can be
//! produced as a stream and read with random access afterwards.
//!
//! Data is grouped into *streams*: one per captured partition plus, in forensic mode, one per
//! unpartitioned gap. Block `n` of a stream covers stream bytes `[n * block_size, (n + 1) *
//! block_size)`. Blocks that are unused or entirely zero are not stored at all.

mod codec;
mod crypto;
mod error;
mod format;
mod index;
mod manifest;
mod reader;
mod split;
mod writer;

pub use codec::{BlockEncoder, BlockRef, EncodedBlock};
pub use crypto::KdfParams;
pub use error::{Error, Result};
pub use format::{FileHeader, Footer, FOOTER_SIZE, FORMAT_VERSION, HEADER_SIZE};
pub use index::IndexEntry;
pub use manifest::*;
pub use reader::{ImageReader, VerifyProgress, VerifyReport};
pub use split::{image_base_path, image_file_path, image_files};
pub use writer::{Checkpoint, FinishSummary, ImageWriter, RawRegion, WriterOptions};

/// Default block size: 1 MiB.
pub const DEFAULT_BLOCK_SIZE: u32 = 1024 * 1024;
/// Smallest accepted block size: 256 KiB.
pub const MIN_BLOCK_SIZE: u32 = 256 * 1024;
/// Largest accepted block size: 4 MiB.
pub const MAX_BLOCK_SIZE: u32 = 4 * 1024 * 1024;

/// File extension of the first file of an image.
pub const EXTENSION: &str = "irimg";
