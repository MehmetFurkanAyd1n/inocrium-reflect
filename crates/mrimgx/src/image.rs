//! Opening an image (or backup set), building the block layout and decoding blocks.

use std::collections::{BTreeSet, HashMap};
use std::fs::File;
use std::io::Read;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use blockdev::{read_exact_at, BlockDevice};
use lru::LruCache;
use md5::{Digest, Md5};
use parking_lot::Mutex;
use serde::Serialize;

use crate::crypto::Crypto;
use crate::device::{DiskDevice, PartitionDevice};
use crate::error::{Error, Result};
use crate::format::{
    self, names, parse_ept, parse_index, BlockHeader, DiskJson, ImageJson, IndexElem, PartitionIndex, FOOTER_LEN,
    HEADER_LEN, MAGIC, MAX_BLOCK_SIZE,
};

/// Upper bound for a decoded metadata block (JSON, track 0, block index).
const MAX_METADATA_LEN: u64 = 1 << 31;
/// Upper bound for the buffered FAT reserved sectors area of one partition.
const MAX_RESERVED_LEN: u64 = 512 << 20;
/// Number of decoded data blocks kept in memory.
const CACHE_BLOCKS: usize = 64;

/// Summary of an image, suitable for display and JSON export.
#[derive(Debug, Clone, Serialize)]
pub struct ImageInfo {
    /// 16 hex digit image id shared by all files of a backup set.
    pub image_id: String,
    /// Backup time (RFC 3339, UTC).
    pub created: Option<String>,
    /// User comment of the backup definition.
    pub comment: String,
    /// NetBIOS name of the imaged machine.
    pub hostname: String,
    /// Whether the image is password protected.
    pub encrypted: bool,
    /// `"full"`, `"incremental"` or `"differential"`.
    pub kind: String,
    /// Imaged disks.
    pub disks: Vec<DiskInfo>,
}

/// One imaged disk.
#[derive(Debug, Clone, Serialize)]
pub struct DiskInfo {
    /// Size of the original disk in bytes.
    pub size: u64,
    /// Logical sector size.
    pub sector_size: u32,
    /// `"mbr"`, `"gpt"` or `"raw"`.
    pub style: String,
    /// MBR disk signature or GPT disk GUID.
    pub signature_or_guid: String,
    /// Imaged partitions.
    pub partitions: Vec<PartitionInfo>,
}

/// One imaged partition.
#[derive(Debug, Clone, Serialize)]
pub struct PartitionInfo {
    /// Position in [`DiskInfo::partitions`] (the `partition_index` of the API).
    pub index: usize,
    /// Partition number as recorded by Reflect.
    pub number: u32,
    /// Byte offset on the disk.
    pub offset: u64,
    /// Length in bytes.
    pub length: u64,
    /// File system name (`NTFS`, `FAT32`, ...).
    pub filesystem: String,
    /// Volume label.
    pub label: String,
    /// Drive letter at backup time.
    pub drive_letter: Option<char>,
    /// GPT partition type GUID (GPT disks).
    pub gpt_type: Option<String>,
    /// MBR partition type (MBR disks).
    pub mbr_type: Option<u8>,
    /// Bytes in use by the file system, if known.
    pub used_bytes: Option<u64>,
}

/// Outcome of [`MrimgxImage::verify`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct VerifyResult {
    /// `true` if every block decoded and matched its MD5 and the run was not cancelled.
    pub ok: bool,
    /// Blocks that failed to decode or whose MD5 did not match.
    pub bad_blocks: u64,
    /// Blocks checked.
    pub checked_blocks: u64,
    /// Human readable summary.
    pub message: String,
}

// ---------------------------------------------------------------------------------------------
// Files
// ---------------------------------------------------------------------------------------------

/// One `.mrimgx` file with its parsed `$JSON`.
pub(crate) struct ImageFile {
    path: PathBuf,
    file: Arc<File>,
    len: u64,
    json: ImageJson,
}

fn read_at(file: &File, len: u64, off: u64, n: u64) -> Result<Vec<u8>> {
    // read_exact_at zero-fills past EOF, so the bounds must be checked here.
    if off.checked_add(n).is_none_or(|end| end > len) {
        return Err(Error::corrupt(format!("structure at offset {off} (+{n} bytes) extends past the end of the file")));
    }
    let mut buf = vec![0; usize::try_from(n).map_err(|_| Error::corrupt("block too large"))?];
    read_exact_at(file, off, &mut buf)?;
    Ok(buf)
}

/// Decompresses the first zstd frame of `data`, producing at most `cap` bytes (trailing bytes
/// after the frame, e.g. AES padding, are ignored like `ZSTD_findFrameCompressedSize` does).
fn zstd_decode(data: &[u8], cap: u64) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    zstd::stream::read::Decoder::with_buffer(data)
        .and_then(|d| d.single_frame().take(cap).read_to_end(&mut out))
        .map_err(|e| Error::corrupt(format!("zstd decompression failed: {e}")))?;
    Ok(out)
}

impl ImageFile {
    /// Opens a file and parses the footer and root `$JSON` block.
    fn open(path: &Path) -> Result<Self> {
        let file = File::open(path)?;
        let len = file.metadata()?.len();
        let first = footer_offset(&file, len)?.ok_or_else(|| Error::BadFormat("missing MACRIUM_FILE footer".into()))?;
        let mut json = None;
        for block in MetaList::new(&file, len, first) {
            let (hdr, off) = block?;
            if &hdr.name == names::JSON {
                if hdr.encrypted() {
                    // The key derivation needs the image id, which is stored in this very block.
                    return Err(Error::Unsupported("encrypted $JSON block".into()));
                }
                let data = read_meta_payload(&file, len, off, &hdr, None)?;
                json = Some(ImageJson::parse(&data)?);
            }
        }
        let json = json.ok_or_else(|| Error::BadFormat("no $JSON block".into()))?;
        Ok(Self { path: path.to_path_buf(), file: Arc::new(file), len, json })
    }

    /// Reads the disk and partition metadata starting at `index_file_position`.
    fn load_index(&self, crypto: Option<&Crypto>) -> Result<Vec<DiskMeta>> {
        let (file, len, json) = (&*self.file, self.len, &self.json);
        let mut pos = json.index_file_position;
        let mut disks = Vec::with_capacity(json.disks.len());
        for disk in &json.disks {
            let mut meta = DiskMeta::default();
            let mut list = MetaList::new(file, len, pos);
            for block in list.by_ref() {
                let (hdr, off) = block?;
                if &hdr.name == names::TRACK0 && hdr.len > 0 {
                    meta.track0 = read_meta_payload(file, len, off, &hdr, crypto)?;
                } else if &hdr.name == names::EPT && hdr.len > 0 {
                    meta.ept = parse_ept(&read_meta_payload(file, len, off, &hdr, crypto)?)?;
                }
            }
            if meta.track0.is_empty() {
                return Err(Error::corrupt("disk metadata has no $TRACK0 block"));
            }
            pos = list.pos;
            for _ in &disk.partitions {
                let mut index = None;
                let mut list = MetaList::new(file, len, pos);
                for block in list.by_ref() {
                    let (hdr, off) = block?;
                    if &hdr.name == names::INDEX {
                        let data = read_meta_payload(file, len, off, &hdr, crypto)?;
                        index = Some(parse_index(&data, json.delta_index)?);
                    }
                }
                meta.partitions.push(index.ok_or_else(|| Error::corrupt("partition metadata has no $INDEX block"))?);
                pos = list.pos;
            }
            disks.push(meta);
        }
        Ok(disks)
    }

    /// File numbers this file serves (its own and those merged into it by consolidation).
    fn numbers(&self) -> impl Iterator<Item = u16> + '_ {
        std::iter::once(self.json.file_number).chain(self.json.merged_files.iter().copied())
    }
}

/// Returns the first metadata block offset if the file ends with a valid footer.
fn footer_offset(file: &File, len: u64) -> Result<Option<u64>> {
    if len < FOOTER_LEN {
        return Ok(None);
    }
    let mut footer = [0; FOOTER_LEN as usize];
    read_exact_at(file, len - FOOTER_LEN, &mut footer)?;
    let off = format::u64_at(&footer, 0);
    Ok((&footer[8..] == MAGIC && off < len - FOOTER_LEN).then_some(off))
}

/// Iterator over a metadata block list, yielding `(header, header offset)`.
struct MetaList<'a> {
    file: &'a File,
    len: u64,
    /// Offset just past the last block read.
    pos: u64,
    done: bool,
}

impl<'a> MetaList<'a> {
    fn new(file: &'a File, len: u64, pos: u64) -> Self {
        Self { file, len, pos, done: false }
    }
}

impl Iterator for MetaList<'_> {
    type Item = Result<(BlockHeader, u64)>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        self.done = true;
        let off = self.pos;
        let raw = match read_at(self.file, self.len, off, HEADER_LEN) {
            Ok(raw) => raw,
            Err(e) => return Some(Err(e)),
        };
        let hdr = BlockHeader::parse(raw[..].try_into().unwrap());
        // Every header is followed by its payload; the list always moves forward.
        match (off + HEADER_LEN).checked_add(u64::from(hdr.len)).filter(|&end| end <= self.len) {
            Some(end) => self.pos = end,
            None => return Some(Err(Error::corrupt(format!("metadata block {} is truncated", hdr.name_str())))),
        }
        self.done = hdr.last();
        Some(Ok((hdr, off)))
    }
}

/// Reads, checks and decodes the payload of the metadata block whose header is at `off`.
/// The MD5 covers the stored bytes; decryption (AES-ECB) precedes decompression.
fn read_meta_payload(file: &File, len: u64, off: u64, hdr: &BlockHeader, crypto: Option<&Crypto>) -> Result<Vec<u8>> {
    let mut data = read_at(file, len, off + HEADER_LEN, u64::from(hdr.len))?;
    if <[u8; 16]>::from(Md5::digest(&data)) != hdr.md5 {
        return Err(Error::corrupt(format!("MD5 mismatch in metadata block {}", hdr.name_str())));
    }
    if hdr.encrypted() {
        let crypto = crypto.ok_or_else(|| {
            Error::Unsupported(format!("encrypted metadata block {} in an unencrypted image", hdr.name_str()))
        })?;
        crypto.decrypt_ecb(&mut data)?;
    }
    if hdr.compressed() {
        data = zstd_decode(&data, MAX_METADATA_LEN)?;
    }
    Ok(data)
}

/// Disk-level metadata plus the block indexes of its partitions.
#[derive(Default)]
struct DiskMeta {
    track0: Vec<u8>,
    ept: Vec<(u64, [u8; 512])>,
    partitions: Vec<PartitionIndex>,
}

// ---------------------------------------------------------------------------------------------
// Layout
// ---------------------------------------------------------------------------------------------

/// Decoded data blocks keyed by `(disk, partition, block index)`.
type BlockCache = LruCache<(usize, usize, usize), Arc<Vec<u8>>>;

/// Shared state behind an image and its devices.
pub(crate) struct Inner {
    files: HashMap<u16, Arc<File>>,
    file_lens: HashMap<u16, u64>,
    crypto: Option<Crypto>,
    compressed: bool,
    pub(crate) disks: Vec<DiskLayout>,
    cache: Mutex<BlockCache>,
}

pub(crate) struct DiskLayout {
    pub(crate) size: u64,
    pub(crate) sector_size: u32,
    track0: Vec<u8>,
    ept: Vec<(u64, [u8; 512])>,
    pub(crate) partitions: Vec<PartLayout>,
}

pub(crate) struct PartLayout {
    /// Offset on the disk.
    pub(crate) start: u64,
    pub(crate) len: u64,
    /// Partition-relative offset of data block 0 (`lcn0_offset - file system start`).
    data_base: u64,
    block_size: u32,
    blocks: Vec<IndexElem>,
    reserved: Vec<IndexElem>,
    reserved_len: u64,
    boot_sector_offset: u64,
    disk_number: u16,
    number: u16,
    reserved_data: Mutex<Option<Arc<Vec<u8>>>>,
}

impl PartLayout {
    fn new(disk: &DiskJson, p: &format::PartitionJson, index: &mut PartitionIndex) -> Result<Self> {
        let data_end = (index.blocks.len() as u64)
            .checked_mul(u64::from(p.block_size))
            .and_then(|n| n.checked_add(p.data_base()))
            .ok_or_else(|| Error::corrupt("partition block index too large"))?;
        let len = match p.len() {
            0 => data_end,
            n => n,
        };
        p.start.checked_add(len).ok_or_else(|| Error::corrupt("partition extends past 2^64"))?;
        Ok(Self {
            start: p.start,
            len,
            data_base: p.data_base(),
            block_size: p.block_size,
            blocks: std::mem::take(&mut index.blocks),
            reserved: std::mem::take(&mut index.reserved),
            reserved_len: p.reserved_len,
            boot_sector_offset: p.boot_sector_offset,
            disk_number: disk.number,
            number: p.number,
            reserved_data: Mutex::new(None),
        })
    }
}

impl Inner {
    /// Reads and decodes one stored block. `iv_index` is the position in its index (holes
    /// included), `cap` the largest plausible decoded size.
    fn decode(&self, p: &PartLayout, e: &IndexElem, iv_index: usize, cap: u32) -> Result<Vec<u8>> {
        let file = self.files.get(&e.file).ok_or_else(|| Error::MissingFile(format!("file number {}", e.file)))?;
        let mut data = read_at(file, self.file_lens[&e.file], e.pos, u64::from(e.len))?;
        if let Some(c) = &self.crypto {
            let iv_index = u32::try_from(iv_index).map_err(|_| Error::corrupt("block index too large"))?;
            c.decrypt_cbc(&c.iv(p.disk_number, p.number, iv_index), &mut data)?;
        }
        if self.compressed {
            data = zstd_decode(&data, u64::from(cap))?;
        } else {
            data.truncate(cap as usize);
        }
        // restore.cpp only checks the MD5 on the compressed path, FILE_LAYOUT.md documents it as
        // the hash of the decoded data; an all-zero hash is treated as "not recorded".
        if e.md5 != [0; 16] && <[u8; 16]>::from(Md5::digest(&data)) != e.md5 {
            return Err(Error::corrupt(format!(
                "MD5 mismatch in data block at offset {} of file number {}",
                e.pos, e.file
            )));
        }
        Ok(data)
    }

    /// Decoded data block `i` of a partition, through the LRU cache.
    fn block(&self, disk: usize, part: usize, i: usize) -> Result<Arc<Vec<u8>>> {
        let key = (disk, part, i);
        if let Some(b) = self.cache.lock().get(&key) {
            return Ok(b.clone());
        }
        let p = &self.disks[disk].partitions[part];
        let data = Arc::new(self.decode(p, &p.blocks[i], i, p.block_size)?);
        self.cache.lock().put(key, data.clone());
        Ok(data)
    }

    /// The FAT reserved sectors area of a partition, assembled as restore.cpp does: decoded
    /// blocks written back to back (holes skipped), up to `reserved_sectors_byte_length`.
    fn reserved(&self, p: &PartLayout) -> Result<Arc<Vec<u8>>> {
        let mut slot = p.reserved_data.lock();
        if let Some(r) = &*slot {
            return Ok(r.clone());
        }
        if p.reserved_len > MAX_RESERVED_LEN {
            return Err(Error::Unsupported(format!("reserved sectors area of {} bytes", p.reserved_len)));
        }
        let mut buf = vec![0; p.reserved_len as usize];
        let mut at = 0;
        for (i, e) in p.reserved.iter().enumerate() {
            if e.is_hole() {
                continue;
            }
            let data = self.decode(p, e, i, MAX_BLOCK_SIZE)?;
            let n = data.len().min(buf.len() - at);
            buf[at..at + n].copy_from_slice(&data[..n]);
            at += n;
        }
        let buf = Arc::new(buf);
        *slot = Some(buf.clone());
        Ok(buf)
    }

    /// Copies stored partition content overlapping `[off, off + buf.len())` into `buf`
    /// (partition-relative). Bytes without stored data are left untouched.
    pub(crate) fn overlay_partition(&self, disk: usize, part: usize, off: u64, buf: &mut [u8]) -> Result<()> {
        let p = &self.disks[disk].partitions[part];
        let end = off + buf.len() as u64;
        if p.reserved_len > 0 && !p.reserved.is_empty() {
            let r_start = p.boot_sector_offset;
            let r_end = r_start.saturating_add(p.reserved_len);
            if off < r_end && r_start < end {
                let reserved = self.reserved(p)?;
                copy_overlap(&reserved, r_start, off, buf);
            }
        }
        let bs = u64::from(p.block_size);
        if end <= p.data_base {
            return Ok(());
        }
        let first = (off.saturating_sub(p.data_base) / bs) as usize;
        let last = ((end - 1 - p.data_base) / bs) as usize;
        for i in first..=last.min(p.blocks.len().saturating_sub(1)) {
            if i >= p.blocks.len() || p.blocks[i].is_hole() {
                continue;
            }
            let data = self.block(disk, part, i)?;
            copy_overlap(&data, p.data_base + i as u64 * bs, off, buf);
        }
        Ok(())
    }

    /// Copies disk content overlapping `[off, off + buf.len())` into a zeroed `buf`, in the
    /// order restore.cpp writes it: track 0, extended boot records, partitions.
    pub(crate) fn read_disk(&self, disk: usize, off: u64, buf: &mut [u8]) -> Result<()> {
        let d = &self.disks[disk];
        buf.fill(0);
        copy_overlap(&d.track0, 0, off, buf);
        for (at, ebr) in &d.ept {
            copy_overlap(ebr, *at, off, buf);
        }
        let end = off + buf.len() as u64;
        for (i, p) in d.partitions.iter().enumerate() {
            let (s, e) = (off.max(p.start), end.min(p.start + p.len));
            if s < e {
                let sub = &mut buf[(s - off) as usize..(e - off) as usize];
                self.overlay_partition(disk, i, s - p.start, sub)?;
            }
        }
        Ok(())
    }
}

/// Copies the part of `src` (located at `src_off`) that overlaps `dst` (located at `dst_off`).
fn copy_overlap(src: &[u8], src_off: u64, dst_off: u64, dst: &mut [u8]) {
    let start = src_off.max(dst_off);
    let end = (src_off.saturating_add(src.len() as u64)).min(dst_off + dst.len() as u64);
    if start < end {
        let n = (end - start) as usize;
        let s = (start - src_off) as usize;
        let d = (start - dst_off) as usize;
        dst[d..d + n].copy_from_slice(&src[s..s + n]);
    }
}

// ---------------------------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------------------------

/// A read-only, opened Reflect X image (a single file or a whole backup set).
///
/// Data is read with positional I/O; decoded blocks are kept in a small LRU cache, so the
/// image and its devices can be shared between threads.
pub struct MrimgxImage {
    inner: Arc<Inner>,
    info: ImageInfo,
}

impl std::fmt::Debug for MrimgxImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MrimgxImage").field("info", &self.info).finish_non_exhaustive()
    }
}

/// Derives and checks the key when the image is encrypted.
fn unlock(json: &ImageJson, password: Option<&str>) -> Result<Option<Crypto>> {
    let Some(enc) = &json.encryption else { return Ok(None) };
    let password = password.ok_or(Error::PasswordRequired)?;
    if enc.key_iterations == 0 {
        return Err(Error::corrupt("encryption enabled with 0 key iterations"));
    }
    let key = Crypto::derive_key(password, &json.image_id_bin, enc.key_iterations);
    if !Crypto::check_hmac(&key, &enc.hmac)? {
        return Err(Error::WrongPassword);
    }
    if matches!(enc.aes_type.as_str(), "" | "none") {
        return Ok(None);
    }
    Crypto::new(&enc.aes_type, &key, json.image_id_bin).map(Some)
}

/// Finds the files of the backup set of `target`: same directory and extension, same image id
/// and an increment number not above the target's (backup_set.cpp). Sorted newest first.
fn scan_set(target: ImageFile) -> Result<Vec<ImageFile>> {
    let dir = target.path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let ext = target.path.extension().map(|e| e.to_ascii_lowercase());
    let own = std::fs::canonicalize(&target.path).ok();
    let mut members = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.extension().map(|e| e.to_ascii_lowercase()) != ext || !path.is_file() {
            continue;
        }
        if own.is_some() && std::fs::canonicalize(&path).ok() == own {
            continue;
        }
        // Unrelated or unreadable files in the folder are skipped, as in the reference.
        if let Ok(f) = ImageFile::open(&path) {
            if f.json.image_id.eq_ignore_ascii_case(&target.json.image_id)
                && f.json.increment_number <= target.json.increment_number
            {
                members.push(f);
            }
        }
    }
    members.push(target);
    members.sort_by_key(|m| std::cmp::Reverse(m.json.file_number));
    Ok(members)
}

impl MrimgxImage {
    /// `true` if `path` ends with a Reflect X footer (`MACRIUM_FILE`).
    pub fn is_mrimgx(path: &Path) -> bool {
        (|| -> Result<bool> {
            let file = File::open(path)?;
            let len = file.metadata()?.len();
            Ok(footer_offset(&file, len)?.is_some())
        })()
        .unwrap_or(false)
    }

    /// Whether opening the image requires a password.
    pub fn needs_password(path: &Path) -> Result<bool> {
        Ok(ImageFile::open(path)?.json.encryption.is_some())
    }

    /// Opens an image. Split files and earlier files of incremental/differential chains are
    /// looked up next to `path`.
    pub fn open(path: &Path, password: Option<&str>) -> Result<Self> {
        let target = ImageFile::open(path)?;
        let mut crypto = unlock(&target.json, password)?;
        let header = target.json.clone();

        // Fast path: a full, unsplit image that only references itself.
        if !target.json.split_file && !target.json.delta_index {
            let meta = target.load_index(crypto.as_ref())?;
            let own: BTreeSet<u16> = target.numbers().collect();
            if referenced(&meta).is_subset(&own) {
                let files = own.iter().map(|&n| (n, (target.file.clone(), target.len))).collect();
                let layout_json = target.json.clone();
                return Self::build(&header, &layout_json, meta, files, crypto);
            }
        }

        let members = scan_set(target)?;
        // The newest file carrying a block index describes the restore (split parts carry data only).
        let restore_at = members
            .iter()
            .position(|m| !m.json.split_file)
            .ok_or_else(|| Error::MissingFile(format!("file with the block index of image {}", header.image_id)))?;
        let restoration = &members[restore_at];
        if restoration.json.encryption.as_ref().map(|e| (&e.aes_type, e.key_iterations, &e.hmac))
            != header.encryption.as_ref().map(|e| (&e.aes_type, e.key_iterations, &e.hmac))
        {
            crypto = unlock(&restoration.json, password)?;
        }
        let mut meta = restoration.load_index(crypto.as_ref())?;
        if restoration.json.delta_index {
            // Start from the newest full (non-delta) index and replay the delta indexes from
            // oldest to newest (backup_set.cpp buildIndex).
            let base_at = members.iter().position(|m| !m.json.delta_index && !m.json.split_file).ok_or_else(|| {
                Error::MissingFile(format!("full image (non-delta index) of backup set {}", header.image_id))
            })?;
            let mut merged = members[base_at].load_index(crypto.as_ref())?;
            check_shape(&merged, &meta)?;
            for m in members[..base_at].iter().rev().filter(|m| !m.json.split_file) {
                let is_restoration = std::ptr::eq(m, restoration);
                let delta = if is_restoration { std::mem::take(&mut meta) } else { m.load_index(crypto.as_ref())? };
                check_shape(&merged, &delta)?;
                for ((md, dd), dj) in merged.iter_mut().zip(delta).zip(&restoration.json.disks) {
                    for ((mp, dp), pj) in md.partitions.iter_mut().zip(dd.partitions).zip(&dj.partitions) {
                        // Bound the index so a garbled delta cannot make us allocate gigabytes.
                        let limit = (mp.blocks.len() as u64)
                            .max(pj.block_count)
                            .max(pj.len().div_ceil(u64::from(pj.block_size)));
                        for (i, e) in dp.deltas {
                            if u64::from(i) >= limit {
                                return Err(Error::corrupt(format!("delta block index {i} out of range")));
                            }
                            let i = i as usize;
                            if i >= mp.blocks.len() {
                                mp.blocks.resize(i + 1, IndexElem::default());
                            }
                            mp.blocks[i] = e;
                        }
                        if is_restoration {
                            // restore.cpp takes the reserved sectors index from the restoration file.
                            mp.reserved = dp.reserved;
                        }
                    }
                    if is_restoration {
                        md.track0 = dd.track0;
                        md.ept = dd.ept;
                    }
                }
            }
            meta = merged;
        }

        let mut files = HashMap::new();
        for m in members.iter().rev() {
            for n in m.numbers() {
                files.insert(n, (m.file.clone(), m.len));
            }
        }
        if let Some(n) = referenced(&meta).into_iter().find(|n| !files.contains_key(n)) {
            return Err(Error::MissingFile(format!("file number {n} of backup set {}", header.image_id)));
        }
        let layout_json = restoration.json.clone();
        Self::build(&header, &layout_json, meta, files, crypto)
    }

    fn build(
        header: &ImageJson,
        layout: &ImageJson,
        meta: Vec<DiskMeta>,
        files: HashMap<u16, (Arc<File>, u64)>,
        crypto: Option<Crypto>,
    ) -> Result<Self> {
        let mut disks = Vec::new();
        let mut infos = Vec::new();
        for (dj, mut dm) in layout.disks.iter().zip(meta) {
            let mut parts = Vec::new();
            for (pj, index) in dj.partitions.iter().zip(dm.partitions.iter_mut()) {
                parts.push(PartLayout::new(dj, pj, index)?);
            }
            let extent = parts
                .iter()
                .map(|p| p.start + p.len)
                .chain(dm.ept.iter().map(|(o, _)| o.saturating_add(512)))
                .chain([dm.track0.len() as u64])
                .max()
                .unwrap_or(0);
            let layout = DiskLayout {
                size: dj.size.max(extent),
                sector_size: dj.sector_size,
                track0: std::mem::take(&mut dm.track0),
                ept: std::mem::take(&mut dm.ept),
                partitions: parts,
            };
            infos.push(crate::info::disk_info(dj, &layout.track0, &layout));
            disks.push(layout);
        }
        let info = ImageInfo {
            image_id: header.image_id.clone(),
            created: (header.backup_time > 0)
                .then(|| chrono::DateTime::from_timestamp(header.backup_time, 0))
                .flatten()
                .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
            comment: header.comment.clone(),
            hostname: header.netbios_name.clone(),
            encrypted: header.encryption.is_some(),
            kind: match header.backup_type.as_str() {
                "" | "full" => "full".into(),
                "inc" | "incremental" => "incremental".into(),
                "diff" | "differential" => "differential".into(),
                other => other.into(),
            },
            disks: infos,
        };
        let (file_lens, files) = files.into_iter().map(|(n, (f, l))| ((n, l), (n, f))).unzip();
        let inner = Inner {
            files,
            file_lens,
            crypto,
            compressed: layout.compressed,
            disks,
            cache: Mutex::new(LruCache::new(NonZeroUsize::new(CACHE_BLOCKS).unwrap())),
        };
        Ok(Self { inner: Arc::new(inner), info })
    }

    /// Image summary.
    pub fn info(&self) -> &ImageInfo {
        &self.info
    }

    /// Read-only view of a whole disk: track 0, extended boot records and partitions at their
    /// original offsets, zeros elsewhere (including the backup GPT, which is not imaged).
    pub fn disk_device(&self, disk_index: usize) -> Result<Arc<dyn BlockDevice>> {
        self.check(disk_index, None)?;
        Ok(Arc::new(DiskDevice::new(self.inner.clone(), disk_index)))
    }

    /// Read-only view of one partition.
    pub fn partition_device(&self, disk_index: usize, partition_index: usize) -> Result<Arc<dyn BlockDevice>> {
        self.check(disk_index, Some(partition_index))?;
        Ok(Arc::new(PartitionDevice::new(self.inner.clone(), disk_index, partition_index)))
    }

    fn check(&self, disk: usize, part: Option<usize>) -> Result<()> {
        let d = self.inner.disks.get(disk).ok_or_else(|| Error::NotFound(format!("disk {disk}")))?;
        match part {
            Some(p) if p >= d.partitions.len() => Err(Error::NotFound(format!("partition {p} of disk {disk}"))),
            _ => Ok(()),
        }
    }

    /// Decodes every stored block and checks its MD5. `progress(done, total)` is called after
    /// each block (counted in blocks); returning `false` cancels.
    pub fn verify(&self, progress: &mut dyn FnMut(u64, u64) -> bool) -> Result<VerifyResult> {
        let inner = &self.inner;
        let stored = |v: &[IndexElem]| v.iter().filter(|e| !e.is_hole()).count() as u64;
        let total: u64 =
            inner.disks.iter().flat_map(|d| &d.partitions).map(|p| stored(&p.reserved) + stored(&p.blocks)).sum();
        let mut res = VerifyResult::default();
        for p in inner.disks.iter().flat_map(|d| &d.partitions) {
            let lists = [(&p.reserved, MAX_BLOCK_SIZE), (&p.blocks, p.block_size)];
            for (list, cap) in lists {
                for (i, e) in list.iter().enumerate().filter(|(_, e)| !e.is_hole()) {
                    match inner.decode(p, e, i, cap) {
                        Ok(_) => {}
                        Err(Error::Corrupt(_)) => res.bad_blocks += 1,
                        Err(e) => return Err(e),
                    }
                    res.checked_blocks += 1;
                    if !progress(res.checked_blocks, total) {
                        res.message = format!(
                            "cancelled after {} of {total} blocks ({} bad)",
                            res.checked_blocks, res.bad_blocks
                        );
                        return Ok(res);
                    }
                }
            }
        }
        res.ok = res.bad_blocks == 0;
        res.message = if res.ok {
            format!("all {} blocks verified", res.checked_blocks)
        } else {
            format!("{} of {} blocks are corrupt", res.bad_blocks, res.checked_blocks)
        };
        Ok(res)
    }
}

/// All file numbers referenced by stored blocks.
fn referenced(meta: &[DiskMeta]) -> BTreeSet<u16> {
    meta.iter()
        .flat_map(|d| &d.partitions)
        .flat_map(|p| p.reserved.iter().chain(&p.blocks).chain(p.deltas.iter().map(|(_, e)| e)))
        .filter(|e| !e.is_hole())
        .map(|e| e.file)
        .collect()
}

/// Files of one chain must describe the same disks and partitions.
fn check_shape(a: &[DiskMeta], b: &[DiskMeta]) -> Result<()> {
    let same = a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.partitions.len() == y.partitions.len());
    if same {
        Ok(())
    } else {
        Err(Error::Unsupported("backup set files describe different disk/partition layouts".into()))
    }
}
