use crate::{win, Error, Result};
use parking_lot::Mutex;
use std::{
    alloc::{alloc_zeroed, dealloc, Layout},
    ffi::OsStr,
    io,
    mem::size_of,
    os::windows::ffi::OsStrExt,
    ptr::NonNull,
};
use windows::{
    core::PCWSTR,
    Win32::{
        Foundation::*,
        Storage::FileSystem::*,
        System::{Ioctl::*, IO::DeviceIoControl},
    },
};

pub(crate) fn wide(s: impl AsRef<OsStr>) -> Vec<u16> {
    s.as_ref().encode_wide().chain(Some(0)).collect()
}
pub(crate) fn string(s: &[u16]) -> String {
    String::from_utf16_lossy(&s[..s.iter().position(|&c| c == 0).unwrap_or(s.len())])
}

pub(crate) struct Handle(pub HANDLE);
// Kernel handles may be used on any thread. Cursor-based I/O is separately serialized below.
unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}
pub(crate) fn open(path: &str, access: u32, flags: FILE_FLAGS_AND_ATTRIBUTES) -> Result<Handle> {
    let p = wide(path);
    unsafe {
        CreateFileW(
            PCWSTR(p.as_ptr()),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            flags,
            None,
        )
    }
    .map(Handle)
    .map_err(|e| win(format!("open {path}"), e))
}

/// Owns an allocation with an alignment suitable for unbuffered device I/O.
pub(crate) struct Buffer {
    ptr: NonNull<u8>,
    layout: Layout,
    len: usize,
}
impl Buffer {
    pub fn new(len: usize, alignment: usize) -> Result<Self> {
        let layout = Layout::from_size_align(len.max(1), alignment).map_err(|e| Error::Other(e.to_string()))?;
        let ptr = NonNull::new(unsafe { alloc_zeroed(layout) })
            .ok_or_else(|| Error::Other("buffer allocation failed".into()))?;
        Ok(Self { ptr, layout, len: layout.size() })
    }
    pub fn bytes(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }
    pub fn bytes_mut(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }
    pub fn get<T: Copy>(&self, offset: usize) -> Result<T> {
        if offset.checked_add(size_of::<T>()).is_none_or(|n| n > self.len) {
            return Err(Error::Other("truncated IOCTL result".into()));
        }
        Ok(unsafe { self.ptr.as_ptr().add(offset).cast::<T>().read_unaligned() })
    }
    pub fn truncate(&mut self, len: usize) -> Result<()> {
        if len > self.len {
            return Err(Error::Other("invalid IOCTL result length".into()));
        }
        self.len = len;
        Ok(())
    }
    pub fn put<T: Copy>(&mut self, offset: usize, value: T) -> Result<()> {
        if offset.checked_add(size_of::<T>()).is_none_or(|n| n > self.layout.size()) {
            return Err(Error::Other("IOCTL buffer too small".into()));
        }
        unsafe {
            self.ptr.as_ptr().add(offset).cast::<T>().write_unaligned(value);
        }
        Ok(())
    }
}
impl Drop for Buffer {
    fn drop(&mut self) {
        unsafe {
            dealloc(self.ptr.as_ptr(), self.layout);
        }
    }
}

pub(crate) fn ioctl(h: HANDLE, code: u32, input: &[u8], output: &mut [u8]) -> (windows::core::Result<()>, usize) {
    let mut count = 0;
    let r = unsafe {
        DeviceIoControl(
            h,
            code,
            if input.is_empty() { None } else { Some(input.as_ptr().cast()) },
            input.len() as u32,
            if output.is_empty() { None } else { Some(output.as_mut_ptr().cast()) },
            output.len() as u32,
            Some(&mut count),
            None,
        )
    };
    (r, count as usize)
}
pub(crate) fn input<T: Copy>(h: HANDLE, code: u32, v: &T) -> Result<()> {
    // T is a fully initialized SDK POD structure and the buffer is used only during this call.
    let mut n = 0;
    unsafe {
        DeviceIoControl(h, code, Some((v as *const T).cast()), size_of::<T>() as u32, None, 0, Some(&mut n), None)
    }
    .map_err(|e| win(format!("IOCTL {code:#x}"), e))
}
pub(crate) fn output<T: Copy + Default>(h: HANDLE, code: u32) -> Result<T> {
    let mut v = T::default();
    let mut n = 0;
    unsafe {
        DeviceIoControl(h, code, None, 0, Some((&mut v as *mut T).cast()), size_of::<T>() as u32, Some(&mut n), None)
    }
    .map_err(|e| win(format!("IOCTL {code:#x}"), e))?;
    if n < size_of::<T>() as u32 {
        return Err(Error::Other("truncated IOCTL result".into()));
    }
    Ok(v)
}
pub(crate) fn control(h: HANDLE, code: u32) -> Result<()> {
    ioctl(h, code, &[], &mut []).0.map_err(|e| win(format!("IOCTL {code:#x}"), e))
}
pub(crate) fn variable(h: HANDLE, code: u32, input: &[u8]) -> Result<Buffer> {
    let mut len = 4096;
    loop {
        let mut b = Buffer::new(len, 8)?;
        let (r, n) = ioctl(h, code, input, b.bytes_mut());
        match r {
            Ok(()) => {
                if n == 0 {
                    return Err(Error::Other("empty IOCTL result".into()));
                }
                if code == IOCTL_STORAGE_QUERY_PROPERTY && n >= 8 {
                    let required: u32 = b.get(4)?;
                    if required as usize > n {
                        if required as usize > 16 * 1024 * 1024 || required as usize <= len {
                            return Err(Error::Other("truncated storage descriptor".into()));
                        }
                        len = required as usize;
                        continue;
                    }
                }
                b.truncate(n)?;
                return Ok(b);
            }
            Err(e)
                if (e.code() == ERROR_MORE_DATA.to_hresult() || e.code() == ERROR_INSUFFICIENT_BUFFER.to_hresult())
                    && len < 16 * 1024 * 1024 =>
            {
                len *= 2
            }
            Err(e) => return Err(win(format!("IOCTL {code:#x}"), e)),
        }
    }
}

pub(crate) fn alignment(h: HANDLE) -> Result<(u32, u32)> {
    let q = STORAGE_PROPERTY_QUERY {
        PropertyId: StorageAccessAlignmentProperty,
        QueryType: PropertyStandardQuery,
        ..Default::default()
    };
    let mut d = STORAGE_ACCESS_ALIGNMENT_DESCRIPTOR::default();
    let mut n = 0;
    let r = unsafe {
        DeviceIoControl(
            h,
            IOCTL_STORAGE_QUERY_PROPERTY,
            Some((&q as *const STORAGE_PROPERTY_QUERY).cast()),
            size_of::<STORAGE_PROPERTY_QUERY>() as u32,
            Some((&mut d as *mut STORAGE_ACCESS_ALIGNMENT_DESCRIPTOR).cast()),
            size_of::<STORAGE_ACCESS_ALIGNMENT_DESCRIPTOR>() as u32,
            Some(&mut n),
            None,
        )
    };
    if r.is_ok()
        && n >= size_of::<STORAGE_ACCESS_ALIGNMENT_DESCRIPTOR>() as u32
        && valid_sector(d.BytesPerLogicalSector)
        && valid_sector(d.BytesPerPhysicalSector)
    {
        return Ok((d.BytesPerLogicalSector, d.BytesPerPhysicalSector.max(d.BytesPerLogicalSector)));
    }
    let g: DISK_GEOMETRY = output(h, IOCTL_DISK_GET_DRIVE_GEOMETRY)?;
    if !valid_sector(g.BytesPerSector) {
        return Err(Error::Other("invalid sector size".into()));
    }
    Ok((g.BytesPerSector, g.BytesPerSector.max(4096)))
}
fn valid_sector(n: u32) -> bool {
    n >= 512 && n.is_power_of_two() && n <= 65536
}

pub(crate) struct Device {
    pub handle: Handle,
    pub size: u64,
    pub sector: u32,
    pub physical: u32,
    pub writable: bool,
    cursor: Mutex<()>,
}
impl Device {
    pub fn open(path: &str, writable: bool) -> Result<Self> {
        let handle = open(
            path,
            GENERIC_READ.0 | if writable { GENERIC_WRITE.0 } else { 0 },
            FILE_FLAG_NO_BUFFERING | FILE_FLAG_WRITE_THROUGH,
        )?;
        let len: GET_LENGTH_INFORMATION = output(handle.0, IOCTL_DISK_GET_LENGTH_INFO)?;
        let size = u64::try_from(len.Length).map_err(|_| Error::Other("negative device size".into()))?;
        let (sector, physical) = alignment(handle.0)?;
        Ok(Self { handle, size, sector, physical, writable, cursor: Mutex::new(()) })
    }
    fn transfer(&self, offset: u64, b: &mut [u8], writing: bool) -> Result<()> {
        unsafe { SetFilePointerEx(self.handle.0, offset as i64, None, FILE_BEGIN) }
            .map_err(|e| win("seek device", e))?;
        let mut n = 0;
        let r = unsafe {
            if writing {
                WriteFile(self.handle.0, Some(b), Some(&mut n), None)
            } else {
                ReadFile(self.handle.0, Some(b), Some(&mut n), None)
            }
        };
        r.map_err(|e| win("device I/O", e))?;
        if n as usize != b.len() {
            return Err(io::Error::new(
                if writing { io::ErrorKind::WriteZero } else { io::ErrorKind::UnexpectedEof },
                "short device I/O",
            )
            .into());
        }
        Ok(())
    }
    fn range(&self, offset: u64, len: usize) -> io::Result<()> {
        if offset.checked_add(len as u64).is_none_or(|e| e > self.size || e > i64::MAX as u64) {
            Err(io::Error::new(io::ErrorKind::InvalidInput, "access beyond device"))
        } else {
            Ok(())
        }
    }
    pub fn read(&self, mut offset: u64, mut out: &mut [u8]) -> io::Result<()> {
        self.range(offset, out.len())?;
        let _guard = self.cursor.lock();
        let s = self.sector as u64;
        while !out.is_empty() {
            let start = offset / s * s;
            let prefix = (offset - start) as usize;
            let count = out.len().min(4 * 1024 * 1024 - prefix);
            let len = (prefix + count).div_ceil(s as usize) * s as usize;
            let mut b = Buffer::new(len, self.physical as usize)?;
            self.transfer(start, b.bytes_mut(), false)?;
            out[..count].copy_from_slice(&b.bytes()[prefix..prefix + count]);
            offset += count as u64;
            out = &mut out[count..];
        }
        Ok(())
    }
    pub fn write(&self, mut offset: u64, mut data: &[u8]) -> io::Result<()> {
        if !self.writable {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "device is read-only"));
        }
        self.range(offset, data.len())?;
        let _guard = self.cursor.lock();
        let s = self.sector as u64;
        while !data.is_empty() {
            let start = offset / s * s;
            let prefix = (offset - start) as usize;
            let count = data.len().min(4 * 1024 * 1024 - prefix);
            let len = (prefix + count).div_ceil(s as usize) * s as usize;
            let mut b = Buffer::new(len, self.physical as usize)?;
            if prefix != 0 || count != len {
                self.transfer(start, b.bytes_mut(), false)?;
            }
            b.bytes_mut()[prefix..prefix + count].copy_from_slice(&data[..count]);
            self.transfer(start, b.bytes_mut(), true)?;
            offset += count as u64;
            data = &data[count..];
        }
        Ok(())
    }
    pub fn flush(&self) -> io::Result<()> {
        let _guard = self.cursor.lock();
        unsafe { FlushFileBuffers(self.handle.0) }.map_err(|e| io::Error::from(win("flush device", e)))
    }
}
macro_rules! block_device {
    ($t:ty) => {
        impl blockdev::BlockDevice for $t {
            fn size(&self) -> u64 {
                self.device.size
            }
            fn block_size(&self) -> u32 {
                self.device.sector
            }
            fn read_only(&self) -> bool {
                !self.device.writable
            }
            fn read_at(&self, offset: u64, buf: &mut [u8]) -> std::io::Result<()> {
                self.device.read(offset, buf)
            }
            fn write_at(&self, offset: u64, buf: &[u8]) -> std::io::Result<()> {
                self.device.write(offset, buf)
            }
            fn flush(&self) -> std::io::Result<()> {
                self.device.flush()
            }
        }
    };
}
pub(crate) use block_device;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn file_device(path: &std::path::Path, size: u64, writable: bool) -> Device {
        Device {
            handle: open(
                &path.to_string_lossy(),
                GENERIC_READ.0 | if writable { GENERIC_WRITE.0 } else { 0 },
                FILE_FLAG_NO_BUFFERING | FILE_FLAG_WRITE_THROUGH,
            )
            .unwrap(),
            size,
            sector: 512,
            physical: 4096,
            writable,
            cursor: Mutex::new(()),
        }
    }

    #[test]
    fn unbuffered_bounce_io_and_concurrent_rmw() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("aligned-io");
        let size = 8 * 1024 * 1024;
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(size).unwrap();
        drop(file);
        let device = Arc::new(file_device(&path, size, true));
        let data: Vec<u8> = (0..4 * 1024 * 1024 + 123).map(|i| (i % 251) as u8).collect();
        device.write(17, &data).unwrap();
        let mut read = vec![0; data.len()];
        device.read(17, &mut read).unwrap();
        assert_eq!(read, data);
        let mut prefix = [1; 17];
        device.read(0, &mut prefix).unwrap();
        assert_eq!(prefix, [0; 17]);
        let mut suffix = [1; 23];
        device.read(17 + data.len() as u64, &mut suffix).unwrap();
        assert_eq!(suffix, [0; 23]);
        let mut workers = Vec::new();
        for i in 0..8 {
            let device = device.clone();
            workers.push(std::thread::spawn(move || {
                for _ in 0..10 {
                    device.write(7 * 1024 * 1024 + i * 32 + 1, &[i as u8 + 1; 31]).unwrap();
                }
            }));
        }
        for worker in workers {
            worker.join().unwrap();
        }
        let mut sector = [0; 512];
        device.read(7 * 1024 * 1024, &mut sector).unwrap();
        for i in 0..8 {
            assert_eq!(&sector[i * 32 + 1..i * 32 + 32], &[i as u8 + 1; 31]);
            assert_eq!(sector[i * 32], 0);
        }
        assert!(device.read(size, &mut [0; 1]).is_err());
        assert!(device.read(u64::MAX, &mut [0; 1]).is_err());
        device.flush().unwrap();
        drop(device);
        let read_only = file_device(&path, size, false);
        assert_eq!(read_only.write(0, b"x").unwrap_err().kind(), io::ErrorKind::PermissionDenied);
    }
}
