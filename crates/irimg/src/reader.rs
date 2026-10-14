//! Random-access image reader.

use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use uuid::Uuid;

use crate::codec::{decode_block, BlockRef};
use crate::crypto::{self, Sealer, DOMAIN_METADATA};
use crate::format::{get_u32, get_u64, FileHeader, Footer, FOOTER_SIZE, HEADER_SIZE};
use crate::index::{BlockIndex, IndexEntry};
use crate::manifest::Manifest;
use crate::split::{image_base_path, image_file_path};
use crate::writer::{RawRegion, META_MAGIC, SECTION_INDEX, SECTION_MANIFEST, SECTION_RAW_REGIONS};
use crate::{Error, Result};

pub struct ImageReader {
    files: Vec<File>,
    paths: Vec<PathBuf>,
    header: FileHeader,
    sealer: Option<Sealer>,
    manifest: Manifest,
    raw_regions: Vec<RawRegion>,
    index: BlockIndex,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct VerifyProgress {
    pub blocks_done: u64,
    pub blocks_total: u64,
    pub bytes_done: u64,
}

#[derive(Debug, Clone, Default)]
pub struct VerifyReport {
    pub blocks_checked: u64,
    pub bytes_checked: u64,
    pub bad_blocks: Vec<BlockRef>,
}

impl VerifyReport {
    pub fn ok(&self) -> bool {
        self.bad_blocks.is_empty()
    }
}

impl ImageReader {
    /// Reads only the first header; tells whether a password is needed to open the image.
    pub fn peek_header(path: &Path) -> Result<FileHeader> {
        let first = image_file_path(&image_base_path(path), 0);
        let mut f = File::open(&first).map_err(|_| Error::MissingFile(first.display().to_string()))?;
        let mut b = vec![0u8; HEADER_SIZE];
        f.read_exact(&mut b).map_err(|_| Error::BadMagic)?;
        FileHeader::decode(&b)
    }

    pub fn open(path: &Path, password: Option<&str>) -> Result<Self> {
        let base = image_base_path(path);
        let mut files = Vec::new();
        let mut paths = Vec::new();
        let mut header0: Option<FileHeader> = None;
        // Open parts until the one carrying the footer.
        let footer = loop {
            let n = files.len() as u32;
            let p = image_file_path(&base, n);
            let f = File::open(&p).map_err(|_| Error::MissingFile(p.display().to_string()))?;
            let mut hb = vec![0u8; HEADER_SIZE];
            read_exact_at(&f, 0, &mut hb)?;
            let h = FileHeader::decode(&hb)?;
            if h.file_no != n {
                return Err(Error::ForeignFile(p.display().to_string()));
            }
            match &header0 {
                Some(h0) if h0.image_id != h.image_id => return Err(Error::ForeignFile(p.display().to_string())),
                None => header0 = Some(h.clone()),
                _ => {}
            }
            let len = f.metadata()?.len();
            let footer = if len >= (HEADER_SIZE + FOOTER_SIZE) as u64 {
                let mut fb = vec![0u8; FOOTER_SIZE];
                read_exact_at(&f, len - FOOTER_SIZE as u64, &mut fb)?;
                Footer::decode(&fb).ok().filter(|ft| ft.image_id == h.image_id && ft.total_files == n + 1)
            } else {
                None
            };
            files.push(f);
            paths.push(p);
            if let Some(ft) = footer {
                break ft;
            }
            if !image_file_path(&base, n + 1).is_file() {
                return Err(Error::CorruptFooter);
            }
        };
        let header = header0.unwrap();

        let sealer = match &header.key_slot {
            Some(slot) => {
                let pw = password.ok_or(Error::PasswordRequired)?;
                let kek = slot.kdf.derive(pw, &slot.salt)?;
                let dek = crypto::unwrap_key(&kek, &slot.wrap_nonce, &slot.wrapped_key, header.image_id.as_bytes())?;
                Some(Sealer::new(&dek))
            }
            None => None,
        };

        let last = files.last().unwrap();
        let mut stored = vec![0u8; usize::try_from(footer.metadata_len).map_err(|_| Error::CorruptFooter)?];
        read_exact_at(last, footer.metadata_offset, &mut stored)?;
        if blake3::hash(&stored).as_bytes() != &footer.metadata_hash {
            return Err(Error::CorruptMetadata);
        }
        if let Some(s) = &sealer {
            stored = s
                .open(DOMAIN_METADATA, footer.metadata_nonce, &stored, header.image_id.as_bytes())
                .ok_or(Error::CorruptMetadata)?;
        }
        let plain =
            zstd::bulk::decompress(&stored, footer.metadata_plain_len as usize).map_err(|_| Error::CorruptMetadata)?;
        let (manifest, raw_regions, index) = parse_metadata(&plain)?;
        if manifest.image_id != header.image_id {
            return Err(Error::CorruptMetadata);
        }
        Ok(Self { files, paths, header, sealer, manifest, raw_regions, index })
    }

    pub fn image_id(&self) -> Uuid {
        self.header.image_id
    }

    pub fn header(&self) -> &FileHeader {
        &self.header
    }

    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    pub fn paths(&self) -> &[PathBuf] {
        &self.paths
    }

    pub fn block_size(&self) -> u32 {
        self.header.block_size
    }

    pub fn raw_regions(&self, disk: u32) -> impl Iterator<Item = &RawRegion> {
        self.raw_regions.iter().filter(move |r| r.disk == disk)
    }

    pub fn stream_entries(&self, disk: u32, stream: u32) -> &[IndexEntry] {
        self.index.streams.get(&(disk, stream)).map(|v| v.as_slice()).unwrap_or(&[])
    }

    pub fn total_blocks(&self) -> u64 {
        self.index.total_blocks()
    }

    /// Reads one block. Returns `None` for blocks that were not stored (unused or zero).
    pub fn read_block(&self, disk: u32, stream: u32, block: u64) -> Result<Option<Vec<u8>>> {
        let Some(e) = self.index.lookup(disk, stream, block) else { return Ok(None) };
        self.read_entry(BlockRef { disk, stream, block }, e).map(Some)
    }

    fn read_entry(&self, at: BlockRef, e: &IndexEntry) -> Result<Vec<u8>> {
        let f = self.files.get(e.file_no as usize).ok_or(Error::CorruptMetadata)?;
        let mut stored = vec![0u8; e.stored_len as usize];
        read_exact_at(f, e.offset, &mut stored)?;
        decode_block(&self.header.image_id, self.sealer.as_ref(), at, e, &stored)
    }

    /// Reads stream bytes at `offset`; unstored ranges read as zeros.
    pub fn read_stream_at(&self, disk: u32, stream: u32, offset: u64, buf: &mut [u8]) -> Result<()> {
        let bs = self.header.block_size as u64;
        let mut done = 0usize;
        while done < buf.len() {
            let pos = offset + done as u64;
            let block = pos / bs;
            let within = (pos % bs) as usize;
            let n = (bs as usize - within).min(buf.len() - done);
            let dst = &mut buf[done..done + n];
            match self.read_block(disk, stream, block)? {
                Some(data) => {
                    let avail = data.len().saturating_sub(within).min(n);
                    dst[..avail].copy_from_slice(&data[within..within + avail]);
                    dst[avail..].fill(0);
                }
                None => dst.fill(0),
            }
            done += n;
        }
        Ok(())
    }

    /// Decodes every block and checks its hash / authentication tag.
    pub fn verify(&self, mut progress: impl FnMut(VerifyProgress) -> bool) -> Result<VerifyReport> {
        let mut report = VerifyReport::default();
        let total = self.index.total_blocks();
        for (&(disk, stream), entries) in &self.index.streams {
            for e in entries {
                let at = BlockRef { disk, stream, block: e.block };
                match self.read_entry(at, e) {
                    Ok(_) => {}
                    Err(Error::CorruptBlock { .. }) => report.bad_blocks.push(at),
                    Err(Error::Io(io)) if io.kind() == io::ErrorKind::UnexpectedEof => report.bad_blocks.push(at),
                    Err(other) => return Err(other),
                }
                report.blocks_checked += 1;
                report.bytes_checked += e.plain_len as u64;
                let keep_going = progress(VerifyProgress {
                    blocks_done: report.blocks_checked,
                    blocks_total: total,
                    bytes_done: report.bytes_checked,
                });
                if !keep_going {
                    return Err(Error::Io(io::Error::new(io::ErrorKind::Interrupted, "verification cancelled")));
                }
            }
        }
        Ok(report)
    }
}

fn parse_metadata(plain: &[u8]) -> Result<(Manifest, Vec<RawRegion>, BlockIndex)> {
    if plain.len() < 8 || &plain[..8] != META_MAGIC {
        return Err(Error::CorruptMetadata);
    }
    let mut pos = 8;
    let mut manifest = None;
    let mut raw = Vec::new();
    let mut index = None;
    while pos < plain.len() {
        if plain.len() < pos + 12 {
            return Err(Error::CorruptMetadata);
        }
        let tag = get_u32(plain, pos);
        let len = get_u64(plain, pos + 4) as usize;
        pos += 12;
        let end = pos.checked_add(len).filter(|&e| e <= plain.len()).ok_or(Error::CorruptMetadata)?;
        let data = &plain[pos..end];
        match tag {
            SECTION_MANIFEST => manifest = Some(serde_json::from_slice::<Manifest>(data)?),
            SECTION_RAW_REGIONS => raw = parse_raw_regions(data)?,
            SECTION_INDEX => index = Some(BlockIndex::decode(data)?),
            _ => {} // Unknown sections are skipped for forward compatibility.
        }
        pos = end;
    }
    Ok((manifest.ok_or(Error::CorruptMetadata)?, raw, index.ok_or(Error::CorruptMetadata)?))
}

fn parse_raw_regions(b: &[u8]) -> Result<Vec<RawRegion>> {
    if b.len() < 4 {
        return Err(Error::CorruptMetadata);
    }
    let n = get_u32(b, 0);
    let mut pos = 4;
    let mut out = Vec::new();
    for _ in 0..n {
        if b.len() < pos + 20 {
            return Err(Error::CorruptMetadata);
        }
        let disk = get_u32(b, pos);
        let offset = get_u64(b, pos + 4);
        let len = get_u64(b, pos + 12) as usize;
        pos += 20;
        let end = pos.checked_add(len).filter(|&e| e <= b.len()).ok_or(Error::CorruptMetadata)?;
        out.push(RawRegion { disk, offset, data: b[pos..end].to_vec() });
        pos = end;
    }
    Ok(out)
}

pub(crate) fn read_exact_at(f: &File, mut offset: u64, mut buf: &mut [u8]) -> io::Result<()> {
    while !buf.is_empty() {
        let n = read_at(f, offset, buf)?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "unexpected end of image file"));
        }
        offset += n as u64;
        buf = &mut buf[n..];
    }
    Ok(())
}

#[cfg(windows)]
fn read_at(f: &File, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
    std::os::windows::fs::FileExt::seek_read(f, buf, offset)
}

#[cfg(unix)]
fn read_at(f: &File, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
    std::os::unix::fs::FileExt::read_at(f, buf, offset)
}
