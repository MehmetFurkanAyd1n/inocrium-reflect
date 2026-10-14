//! The [`BlockDevice`] trait and simple implementations.

use std::fs::File;
use std::io;
use std::path::Path;
use std::sync::Arc;

use parking_lot::RwLock;

/// A random-access device addressed in bytes.
///
/// Implementations use interior mutability so that one device can be shared between threads
/// (for example several iSCSI connections). Offsets and lengths passed by SCSI front ends are
/// always multiples of [`BlockDevice::block_size`]; other callers may use any alignment unless
/// the implementation documents otherwise.
pub trait BlockDevice: Send + Sync {
    /// Size in bytes.
    fn size(&self) -> u64;
    /// Logical sector size in bytes (512 or 4096).
    fn block_size(&self) -> u32;
    fn read_only(&self) -> bool;
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()>;
    fn write_at(&self, offset: u64, buf: &[u8]) -> io::Result<()>;
    fn flush(&self) -> io::Result<()> {
        Ok(())
    }
}

impl<T: BlockDevice + ?Sized> BlockDevice for Arc<T> {
    fn size(&self) -> u64 {
        (**self).size()
    }
    fn block_size(&self) -> u32 {
        (**self).block_size()
    }
    fn read_only(&self) -> bool {
        (**self).read_only()
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        (**self).read_at(offset, buf)
    }
    fn write_at(&self, offset: u64, buf: &[u8]) -> io::Result<()> {
        (**self).write_at(offset, buf)
    }
    fn flush(&self) -> io::Result<()> {
        (**self).flush()
    }
}

impl<T: BlockDevice + ?Sized> BlockDevice for &T {
    fn size(&self) -> u64 {
        (**self).size()
    }
    fn block_size(&self) -> u32 {
        (**self).block_size()
    }
    fn read_only(&self) -> bool {
        (**self).read_only()
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        (**self).read_at(offset, buf)
    }
    fn write_at(&self, offset: u64, buf: &[u8]) -> io::Result<()> {
        (**self).write_at(offset, buf)
    }
    fn flush(&self) -> io::Result<()> {
        (**self).flush()
    }
}

impl<T: BlockDevice + ?Sized> BlockDevice for Box<T> {
    fn size(&self) -> u64 {
        (**self).size()
    }
    fn block_size(&self) -> u32 {
        (**self).block_size()
    }
    fn read_only(&self) -> bool {
        (**self).read_only()
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        (**self).read_at(offset, buf)
    }
    fn write_at(&self, offset: u64, buf: &[u8]) -> io::Result<()> {
        (**self).write_at(offset, buf)
    }
    fn flush(&self) -> io::Result<()> {
        (**self).flush()
    }
}

pub(crate) fn check_range(dev: &(impl BlockDevice + ?Sized), offset: u64, len: usize) -> io::Result<()> {
    match offset.checked_add(len as u64) {
        Some(end) if end <= dev.size() => Ok(()),
        _ => Err(io::Error::new(io::ErrorKind::InvalidInput, "access beyond end of device")),
    }
}

pub(crate) fn write_protected() -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, "device is read-only")
}

/// In-memory device, mainly for tests.
pub struct MemDevice {
    data: RwLock<Vec<u8>>,
    block_size: u32,
    read_only: bool,
}

impl MemDevice {
    pub fn new(size: u64, block_size: u32) -> Self {
        Self::from_vec(vec![0; size as usize], block_size)
    }
    pub fn from_vec(data: Vec<u8>, block_size: u32) -> Self {
        Self { data: RwLock::new(data), block_size, read_only: false }
    }
    pub fn with_read_only(mut self, ro: bool) -> Self {
        self.read_only = ro;
        self
    }
    pub fn to_vec(&self) -> Vec<u8> {
        self.data.read().clone()
    }
}

impl BlockDevice for MemDevice {
    fn size(&self) -> u64 {
        self.data.read().len() as u64
    }
    fn block_size(&self) -> u32 {
        self.block_size
    }
    fn read_only(&self) -> bool {
        self.read_only
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        check_range(self, offset, buf.len())?;
        let o = offset as usize;
        buf.copy_from_slice(&self.data.read()[o..o + buf.len()]);
        Ok(())
    }
    fn write_at(&self, offset: u64, buf: &[u8]) -> io::Result<()> {
        if self.read_only {
            return Err(write_protected());
        }
        check_range(self, offset, buf.len())?;
        let o = offset as usize;
        self.data.write()[o..o + buf.len()].copy_from_slice(buf);
        Ok(())
    }
}

/// A regular file used as a device (fixed size).
pub struct FileDevice {
    file: File,
    size: u64,
    block_size: u32,
    read_only: bool,
}

impl FileDevice {
    pub fn open(path: &Path, writable: bool, block_size: u32) -> io::Result<Self> {
        let file = std::fs::OpenOptions::new().read(true).write(writable).open(path)?;
        let size = file.metadata()?.len();
        Ok(Self { file, size, block_size, read_only: !writable })
    }
}

impl BlockDevice for FileDevice {
    fn size(&self) -> u64 {
        self.size
    }
    fn block_size(&self) -> u32 {
        self.block_size
    }
    fn read_only(&self) -> bool {
        self.read_only
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        check_range(self, offset, buf.len())?;
        read_exact_at(&self.file, offset, buf)
    }
    fn write_at(&self, offset: u64, buf: &[u8]) -> io::Result<()> {
        if self.read_only {
            return Err(write_protected());
        }
        check_range(self, offset, buf.len())?;
        write_all_at(&self.file, offset, buf)
    }
    fn flush(&self) -> io::Result<()> {
        self.file.sync_data()
    }
}

/// Positional read that does not move a shared file cursor (safe for concurrent use).
pub fn read_exact_at(f: &File, mut offset: u64, mut buf: &mut [u8]) -> io::Result<()> {
    while !buf.is_empty() {
        #[cfg(windows)]
        let n = std::os::windows::fs::FileExt::seek_read(f, buf, offset)?;
        #[cfg(unix)]
        let n = std::os::unix::fs::FileExt::read_at(f, buf, offset)?;
        if n == 0 {
            // Reading past the end of a sparse/short file yields zeros.
            buf.fill(0);
            return Ok(());
        }
        offset += n as u64;
        buf = &mut buf[n..];
    }
    Ok(())
}

/// Positional write that does not move a shared file cursor.
pub fn write_all_at(f: &File, mut offset: u64, mut buf: &[u8]) -> io::Result<()> {
    while !buf.is_empty() {
        #[cfg(windows)]
        let n = std::os::windows::fs::FileExt::seek_write(f, buf, offset)?;
        #[cfg(unix)]
        let n = std::os::unix::fs::FileExt::write_at(f, buf, offset)?;
        if n == 0 {
            return Err(io::ErrorKind::WriteZero.into());
        }
        offset += n as u64;
        buf = &buf[n..];
    }
    Ok(())
}
