//! Copy-on-write overlay used for writable mounts. Writes go to a scratch file; the base
//! device is never modified, and the scratch file disappears when the overlay is dropped.

use std::collections::HashMap;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::{Mutex, RwLock};

use crate::device::{check_range, read_exact_at, write_all_at, BlockDevice};

/// Granularity of copied-up data.
pub const CHUNK_SIZE: u64 = 64 << 10;

pub struct CowOverlay {
    base: Arc<dyn BlockDevice>,
    file: File,
    path: PathBuf,
    /// Chunk index → offset in `file`.
    map: RwLock<HashMap<u64, u64>>,
    /// End of the append log; held for the duration of each write so copy-ups don't race.
    next: Mutex<u64>,
}

impl CowOverlay {
    /// Creates the overlay file at `path` (which must not exist).
    pub fn create(base: Arc<dyn BlockDevice>, path: &Path) -> io::Result<Self> {
        let mut opts = std::fs::OpenOptions::new();
        opts.read(true).write(true).create_new(true);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            const FILE_FLAG_DELETE_ON_CLOSE: u32 = 0x0400_0000;
            opts.custom_flags(FILE_FLAG_DELETE_ON_CLOSE);
        }
        let file = opts.open(path)?;
        #[cfg(not(windows))]
        std::fs::remove_file(path)?;
        Ok(Self { base, file, path: path.to_owned(), map: RwLock::new(HashMap::new()), next: Mutex::new(0) })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Bytes held in the overlay file.
    pub fn dirty_bytes(&self) -> u64 {
        *self.next.lock()
    }

    fn chunk_len(&self, chunk: u64) -> u64 {
        CHUNK_SIZE.min(self.base.size() - chunk * CHUNK_SIZE)
    }
}

/// Splits `[offset, offset + len)` into `(chunk, offset within chunk, position in buffer, length)`.
fn pieces(offset: u64, len: usize) -> impl Iterator<Item = (u64, u64, usize, usize)> {
    let mut pos = 0usize;
    std::iter::from_fn(move || {
        if pos == len {
            return None;
        }
        let abs = offset + pos as u64;
        let within = abs % CHUNK_SIZE;
        let n = ((CHUNK_SIZE - within) as usize).min(len - pos);
        let item = (abs / CHUNK_SIZE, within, pos, n);
        pos += n;
        Some(item)
    })
}

impl BlockDevice for CowOverlay {
    fn size(&self) -> u64 {
        self.base.size()
    }
    fn block_size(&self) -> u32 {
        self.base.block_size()
    }
    fn read_only(&self) -> bool {
        false
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        check_range(self, offset, buf.len())?;
        // Coalesce runs of unmodified chunks into single base reads.
        let mut base_run: Option<(u64, usize, usize)> = None;
        let map = self.map.read();
        for (chunk, within, pos, n) in pieces(offset, buf.len()) {
            match map.get(&chunk) {
                Some(&file_off) => {
                    if let Some((o, p, l)) = base_run.take() {
                        self.base.read_at(o, &mut buf[p..p + l])?;
                    }
                    read_exact_at(&self.file, file_off + within, &mut buf[pos..pos + n])?;
                }
                None => match &mut base_run {
                    Some((_, _, l)) => *l += n,
                    None => base_run = Some((chunk * CHUNK_SIZE + within, pos, n)),
                },
            }
        }
        if let Some((o, p, l)) = base_run {
            self.base.read_at(o, &mut buf[p..p + l])?;
        }
        Ok(())
    }
    fn write_at(&self, offset: u64, buf: &[u8]) -> io::Result<()> {
        check_range(self, offset, buf.len())?;
        let mut next = self.next.lock();
        for (chunk, within, pos, n) in pieces(offset, buf.len()) {
            let existing = self.map.read().get(&chunk).copied();
            match existing {
                Some(file_off) => write_all_at(&self.file, file_off + within, &buf[pos..pos + n])?,
                None => {
                    let len = self.chunk_len(chunk) as usize;
                    let mut data = vec![0u8; len];
                    if n < len {
                        self.base.read_at(chunk * CHUNK_SIZE, &mut data)?;
                    }
                    data[within as usize..within as usize + n].copy_from_slice(&buf[pos..pos + n]);
                    write_all_at(&self.file, *next, &data)?;
                    self.map.write().insert(chunk, *next);
                    *next += CHUNK_SIZE;
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::MemDevice;

    #[test]
    fn overlay_preserves_base() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.cow");
        let size = 3 * CHUNK_SIZE + 4096; // short last chunk
        let init: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
        let base = Arc::new(MemDevice::from_vec(init.clone(), 512).with_read_only(true));
        let cow = CowOverlay::create(base.clone(), &path).unwrap();
        assert!(CowOverlay::create(base.clone(), &path).is_err(), "must not reuse an existing file");

        let mut expect = init.clone();
        let mut write = |off: u64, data: &[u8]| {
            cow.write_at(off, data).unwrap();
            expect[off as usize..off as usize + data.len()].copy_from_slice(data);
        };
        write(100, &[1; 10]);
        write(CHUNK_SIZE - 512, &[2; 1024]); // spans chunks 0 and 1
        write(3 * CHUNK_SIZE, &[3; 4096]); // whole short chunk
        write(150, &[4; 5]); // rewrite of a copied chunk

        let mut all = vec![0u8; size as usize];
        cow.read_at(0, &mut all).unwrap();
        assert_eq!(all, expect);
        let mut part = vec![0u8; 3000];
        cow.read_at(CHUNK_SIZE - 1000, &mut part).unwrap();
        assert_eq!(part, expect[(CHUNK_SIZE - 1000) as usize..(CHUNK_SIZE + 2000) as usize]);
        assert_eq!(base.to_vec(), init);
        assert_eq!(cow.dirty_bytes(), 3 * CHUNK_SIZE);
        assert!(cow.write_at(size - 1, &[0; 2]).is_err());

        drop(cow);
        assert!(!path.exists());
    }
}
