//! Read-only [`BlockDevice`] views of an opened image.

use std::io;
use std::sync::Arc;

use blockdev::BlockDevice;

use crate::image::Inner;

fn check_range(size: u64, offset: u64, len: usize) -> io::Result<()> {
    match offset.checked_add(len as u64) {
        Some(end) if end <= size => Ok(()),
        _ => Err(io::Error::new(io::ErrorKind::InvalidInput, "access beyond end of device")),
    }
}

fn write_protected() -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, "device is read-only")
}

/// A whole imaged disk.
pub(crate) struct DiskDevice {
    inner: Arc<Inner>,
    disk: usize,
}

impl DiskDevice {
    pub(crate) fn new(inner: Arc<Inner>, disk: usize) -> Self {
        Self { inner, disk }
    }
}

impl BlockDevice for DiskDevice {
    fn size(&self) -> u64 {
        self.inner.disks[self.disk].size
    }

    fn block_size(&self) -> u32 {
        self.inner.disks[self.disk].sector_size
    }

    fn read_only(&self) -> bool {
        true
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        check_range(self.size(), offset, buf.len())?;
        Ok(self.inner.read_disk(self.disk, offset, buf)?)
    }

    fn write_at(&self, _offset: u64, _buf: &[u8]) -> io::Result<()> {
        Err(write_protected())
    }
}

/// One imaged partition.
pub(crate) struct PartitionDevice {
    inner: Arc<Inner>,
    disk: usize,
    part: usize,
}

impl PartitionDevice {
    pub(crate) fn new(inner: Arc<Inner>, disk: usize, part: usize) -> Self {
        Self { inner, disk, part }
    }
}

impl BlockDevice for PartitionDevice {
    fn size(&self) -> u64 {
        self.inner.disks[self.disk].partitions[self.part].len
    }

    fn block_size(&self) -> u32 {
        self.inner.disks[self.disk].sector_size
    }

    fn read_only(&self) -> bool {
        true
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        check_range(self.size(), offset, buf.len())?;
        buf.fill(0);
        Ok(self.inner.overlay_partition(self.disk, self.part, offset, buf)?)
    }

    fn write_at(&self, _offset: u64, _buf: &[u8]) -> io::Result<()> {
        Err(write_protected())
    }
}
