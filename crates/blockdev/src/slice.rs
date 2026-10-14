//! A byte range of another device presented as a device of its own.

use std::io;

use crate::device::{check_range, BlockDevice};

pub struct Slice<D: BlockDevice> {
    inner: D,
    offset: u64,
    len: u64,
}

impl<D: BlockDevice> Slice<D> {
    pub fn new(inner: D, offset: u64, len: u64) -> io::Result<Self> {
        match offset.checked_add(len) {
            Some(end) if end <= inner.size() => Ok(Self { inner, offset, len }),
            _ => Err(io::Error::new(io::ErrorKind::InvalidInput, "slice beyond end of device")),
        }
    }

    pub fn inner(&self) -> &D {
        &self.inner
    }

    pub fn offset(&self) -> u64 {
        self.offset
    }
}

impl<D: BlockDevice> BlockDevice for Slice<D> {
    fn size(&self) -> u64 {
        self.len
    }
    fn block_size(&self) -> u32 {
        self.inner.block_size()
    }
    fn read_only(&self) -> bool {
        self.inner.read_only()
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        check_range(self, offset, buf.len())?;
        self.inner.read_at(self.offset + offset, buf)
    }
    fn write_at(&self, offset: u64, buf: &[u8]) -> io::Result<()> {
        check_range(self, offset, buf.len())?;
        self.inner.write_at(self.offset + offset, buf)
    }
    fn flush(&self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::MemDevice;

    #[test]
    fn slice_bounds() {
        let dev = MemDevice::new(4096, 512);
        assert!(Slice::new(&dev, 4000, 200).is_err());
        let s = Slice::new(&dev, 1024, 2048).unwrap();
        s.write_at(0, &[7; 512]).unwrap();
        assert!(s.write_at(2000, &[0; 100]).is_err());
        let mut b = [0u8; 4];
        dev.read_at(1022, &mut b).unwrap();
        assert_eq!(b, [0, 0, 7, 7]);
    }
}
