//! Platform-independent imaging engine: backup, verify, restore and retention on top of
//! [`blockdev::BlockDevice`] sources and targets. Windows specifics (VSS, volume bitmaps,
//! layout IOCTLs) live in [`win`].

mod error;
pub mod esp;
pub mod extents;
pub mod fs;
pub mod ntfs;
pub mod plan;
pub mod progress;

pub mod backup;
pub mod restore;
pub mod retention;
pub mod verify;
#[cfg(windows)]
pub mod win;

pub use backup::{
    backup, checkpoint_caller_state, checkpoint_path, load_checkpoint, BackupOptions, BackupOutcome, SourceDisk,
    SourcePartition,
};
pub use error::{Error, Result};
pub use extents::Extents;
pub use progress::{no_progress, Cancel, Progress, ProgressFn};
pub use restore::{restore_disk, restore_partition, target_layout, RestoreOptions, RestoreReport, ZeroFill};
pub use retention::{apply_retention, find_images, image_name, ImageFile, Retention};
pub use verify::verify;
