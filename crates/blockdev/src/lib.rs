//! Block devices: the image-backed disk view, the copy-on-write overlay used for writable
//! mounts, and partition table helpers shared with restore and VHDX export.

mod cow;
mod device;
pub mod gpt;
pub mod mbr;
mod slice;
mod table;
mod view;

pub use cow::{CowOverlay, CHUNK_SIZE as COW_CHUNK_SIZE};
pub use device::{read_exact_at, write_all_at, BlockDevice, FileDevice, MemDevice};
pub use slice::Slice;
pub use table::disk_table;
pub use view::{ImageDiskView, ViewOptions};
