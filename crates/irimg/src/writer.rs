//! Sequential image writer with split files, checkpoints and resume.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::codec::{BlockEncoder, EncodedBlock};
use crate::crypto::{self, KdfParams, Sealer, DOMAIN_METADATA, KEY_LEN, SALT_LEN, WRAP_NONCE_LEN};
use crate::format::{FileHeader, Footer, KeySlot, FLAG_COMPRESSED, FLAG_ENCRYPTED, FORMAT_VERSION, HEADER_SIZE};
use crate::index::{BlockIndex, IndexEntry};
use crate::manifest::Manifest;
use crate::split::{image_base_path, image_file_path};
use crate::{Error, Result, MAX_BLOCK_SIZE, MIN_BLOCK_SIZE};

/// Gap added to the nonce counter on resume so that nonces consumed by blocks written after
/// the last checkpoint (and then truncated away) are never reused.
const RESUME_NONCE_GAP: u64 = 1 << 40;
/// Bits of randomness added on top of [`RESUME_NONCE_GAP`]. Resuming twice from the same
/// checkpoint (the first resumed run died before writing a new one) must not start both runs
/// at the same counter, or blocks of the two runs would be sealed under identical nonces.
const RESUME_NONCE_RANDOM_BITS: u32 = 56;
/// Nonce counters above this are refused so the counter can never wrap around.
const MAX_NONCE_START: u64 = u64::MAX - (1 << 48);

pub(crate) const META_MAGIC: &[u8; 8] = b"IRMETA01";
pub(crate) const SECTION_MANIFEST: u32 = 1;
pub(crate) const SECTION_RAW_REGIONS: u32 = 2;
pub(crate) const SECTION_INDEX: u32 = 3;

#[derive(Clone)]
pub struct WriterOptions {
    pub block_size: u32,
    /// zstd level; 0 disables compression.
    pub compression_level: i32,
    pub password: Option<String>,
    pub kdf: KdfParams,
    /// Maximum size of each file; `None` writes a single file.
    pub split_size: Option<u64>,
}

impl std::fmt::Debug for WriterOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print the password.
        f.debug_struct("WriterOptions")
            .field("block_size", &self.block_size)
            .field("compression_level", &self.compression_level)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("kdf", &self.kdf)
            .field("split_size", &self.split_size)
            .finish()
    }
}

impl Default for WriterOptions {
    fn default() -> Self {
        Self {
            block_size: crate::DEFAULT_BLOCK_SIZE,
            compression_level: 3,
            password: None,
            kdf: KdfParams::default(),
            split_size: None,
        }
    }
}

/// Bytes of the source disk stored verbatim in the metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawRegion {
    pub disk: u32,
    pub offset: u64,
    pub data: Vec<u8>,
}

/// State needed to continue an interrupted backup. Serialized next to the image as
/// `name.irimg.ckpt` by the engine. It never contains key material.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    pub image_id: Uuid,
    pub file_no: u32,
    pub file_offset: u64,
    pub nonce_counter: u64,
    pub block_size: u32,
    pub compression_level: i32,
    pub split_size: Option<u64>,
    /// Binary encoded block index (hex in JSON).
    #[serde(with = "serde_bytes_compat")]
    pub index: Vec<u8>,
    /// Opaque state owned by the caller (e.g. which stream/block to continue from, VSS ids).
    pub user: serde_json::Value,
}

#[derive(Debug, Clone)]
pub struct FinishSummary {
    pub image_id: Uuid,
    pub files: Vec<PathBuf>,
    pub total_bytes: u64,
    pub blocks: u64,
}

pub struct ImageWriter {
    base: PathBuf,
    header: FileHeader,
    split_size: Option<u64>,
    file: File,
    file_offset: u64,
    files: Vec<PathBuf>,
    index: BlockIndex,
    encoder: BlockEncoder,
}

impl ImageWriter {
    /// Creates a new image. `path` may be given with or without the `.irimg` extension.
    pub fn create(path: &Path, opts: WriterOptions) -> Result<Self> {
        if !(MIN_BLOCK_SIZE..=MAX_BLOCK_SIZE).contains(&opts.block_size) || !opts.block_size.is_power_of_two() {
            return Err(Error::InvalidArgument(format!("block size {} not supported", opts.block_size)));
        }
        if let Some(s) = opts.split_size {
            if s < (HEADER_SIZE as u64) + opts.block_size as u64 * 2 {
                return Err(Error::InvalidArgument("split size too small".into()));
            }
        }
        let image_id = Uuid::new_v4();
        let mut flags = 0;
        if opts.compression_level > 0 {
            flags |= FLAG_COMPRESSED;
        }
        let (key_slot, sealer) = match &opts.password {
            Some(pw) => {
                flags |= FLAG_ENCRYPTED;
                let salt = crypto::random_bytes::<SALT_LEN>();
                let kek = opts.kdf.derive(pw, &salt)?;
                let dek = crypto::random_bytes::<KEY_LEN>();
                let wrap_nonce = crypto::random_bytes::<WRAP_NONCE_LEN>();
                let wrapped_key = crypto::wrap_key(&kek, &wrap_nonce, &dek, image_id.as_bytes());
                (Some(KeySlot { kdf: opts.kdf, salt, wrap_nonce, wrapped_key }), Some(Sealer::new(&dek)))
            }
            None => (None, None),
        };
        let header = FileHeader {
            version: FORMAT_VERSION,
            flags,
            file_no: 0,
            image_id,
            block_size: opts.block_size,
            created_unix: now_unix(),
            key_slot,
        };
        let base = image_base_path(path);
        // Remove stale split parts from an earlier image with the same name.
        remove_parts_from(&base, 1);
        let first = image_file_path(&base, 0);
        let mut file = OpenOptions::new().write(true).create(true).truncate(true).open(&first)?;
        file.write_all(&header.encode())?;
        let encoder =
            BlockEncoder { image_id, level: opts.compression_level, sealer, nonce: Arc::new(AtomicU64::new(0)) };
        Ok(Self {
            base,
            header,
            split_size: opts.split_size,
            file,
            file_offset: HEADER_SIZE as u64,
            files: vec![first],
            index: BlockIndex::default(),
            encoder,
        })
    }

    /// Reopens an interrupted image at `checkpoint`. Anything written after the checkpoint is
    /// discarded. The password must match the one the image was created with. Callers should
    /// durably replace their stored checkpoint with [`Self::checkpoint`] before appending, so a
    /// second resume never reuses this run's nonces.
    pub fn resume(path: &Path, checkpoint: &Checkpoint, password: Option<&str>) -> Result<Self> {
        let base = image_base_path(path);
        let first = image_file_path(&base, 0);
        let mut hbuf = vec![0u8; HEADER_SIZE];
        File::open(&first)?.read_exact(&mut hbuf)?;
        let header0 = FileHeader::decode(&hbuf)?;
        if header0.image_id != checkpoint.image_id {
            return Err(Error::ForeignFile(first.display().to_string()));
        }
        let sealer = match &header0.key_slot {
            Some(slot) => {
                let pw = password.ok_or(Error::PasswordRequired)?;
                let kek = slot.kdf.derive(pw, &slot.salt)?;
                let dek = crypto::unwrap_key(&kek, &slot.wrap_nonce, &slot.wrapped_key, header0.image_id.as_bytes())?;
                Some(Sealer::new(&dek))
            }
            None => None,
        };
        if checkpoint.block_size != header0.block_size {
            return Err(Error::InvalidArgument("checkpoint block size does not match the image".into()));
        }
        let index = BlockIndex::decode(&checkpoint.index)?;
        let nonce_start = resume_nonce_start(checkpoint.nonce_counter)?;
        let mut files = Vec::new();
        for n in 0..=checkpoint.file_no {
            let p = image_file_path(&base, n);
            if !p.is_file() {
                return Err(Error::MissingFile(p.display().to_string()));
            }
            if n > 0 {
                File::open(&p)?.read_exact(&mut hbuf)?;
                let h = FileHeader::decode(&hbuf)?;
                if h.image_id != header0.image_id || h.file_no != n {
                    return Err(Error::ForeignFile(p.display().to_string()));
                }
            }
            files.push(p);
        }
        let current = files.last().unwrap().clone();
        let mut file = OpenOptions::new().write(true).read(true).open(&current)?;
        // Never truncate into the header, and never extend the file with zeros the index
        // would then point into.
        if checkpoint.file_offset < HEADER_SIZE as u64 || checkpoint.file_offset > file.metadata()?.len() {
            return Err(Error::InvalidArgument("checkpoint does not match the image file".into()));
        }
        remove_parts_from(&base, checkpoint.file_no.saturating_add(1));
        file.set_len(checkpoint.file_offset)?;
        file.seek(SeekFrom::Start(checkpoint.file_offset))?;
        let mut header = header0;
        header.file_no = checkpoint.file_no;
        let encoder = BlockEncoder {
            image_id: header.image_id,
            level: checkpoint.compression_level,
            sealer,
            nonce: Arc::new(AtomicU64::new(nonce_start)),
        };
        Ok(Self {
            base,
            header,
            split_size: checkpoint.split_size,
            file,
            file_offset: checkpoint.file_offset,
            files,
            index,
            encoder,
        })
    }

    pub fn image_id(&self) -> Uuid {
        self.header.image_id
    }

    pub fn block_size(&self) -> u32 {
        self.header.block_size
    }

    pub fn first_file(&self) -> &Path {
        &self.files[0]
    }

    /// Encoder for worker threads.
    pub fn encoder(&self) -> BlockEncoder {
        self.encoder.clone()
    }

    /// Whether a block for this position has already been stored (useful after resume).
    pub fn contains(&self, disk: u32, stream: u32, block: u64) -> bool {
        self.index.lookup(disk, stream, block).is_some()
    }

    /// Appends an encoded block. Blocks may be appended in any order.
    pub fn append(&mut self, b: EncodedBlock) -> Result<()> {
        let len = b.data.len() as u64;
        if let Some(limit) = self.split_size {
            if self.file_offset + len > limit && self.file_offset > HEADER_SIZE as u64 {
                self.next_file()?;
            }
        }
        self.file.write_all(&b.data)?;
        self.index.push(
            b.at.disk,
            b.at.stream,
            IndexEntry {
                block: b.at.block,
                file_no: self.header.file_no,
                flags: b.flags,
                offset: self.file_offset,
                stored_len: len as u32,
                plain_len: b.plain_len,
                nonce: b.nonce,
                hash: b.hash,
            },
        );
        self.file_offset += len;
        Ok(())
    }

    fn next_file(&mut self) -> Result<()> {
        self.file.sync_all()?;
        self.header.file_no += 1;
        let p = image_file_path(&self.base, self.header.file_no);
        let mut f = OpenOptions::new().write(true).create(true).truncate(true).open(&p)?;
        f.write_all(&self.header.encode())?;
        self.file = f;
        self.file_offset = HEADER_SIZE as u64;
        self.files.push(p);
        Ok(())
    }

    /// Flushes everything written so far to stable storage and returns the resume state.
    pub fn checkpoint(&mut self, user: serde_json::Value) -> Result<Checkpoint> {
        self.file.sync_all()?;
        Ok(Checkpoint {
            image_id: self.header.image_id,
            file_no: self.header.file_no,
            file_offset: self.file_offset,
            nonce_counter: self.encoder.nonce.load(Ordering::SeqCst),
            block_size: self.header.block_size,
            compression_level: self.encoder.level,
            split_size: self.split_size,
            index: self.index.encode(),
            user,
        })
    }

    /// Writes the metadata and footer and closes the image.
    pub fn finish(mut self, manifest: &Manifest, raw_regions: &[RawRegion]) -> Result<FinishSummary> {
        if manifest.image_id != self.header.image_id {
            return Err(Error::InvalidArgument("manifest image_id does not match the writer".into()));
        }
        let mut plain = Vec::new();
        plain.extend_from_slice(META_MAGIC);
        push_section(&mut plain, SECTION_MANIFEST, &serde_json::to_vec(manifest)?);
        let mut raw = Vec::new();
        raw.extend_from_slice(&(raw_regions.len() as u32).to_le_bytes());
        for r in raw_regions {
            raw.extend_from_slice(&r.disk.to_le_bytes());
            raw.extend_from_slice(&r.offset.to_le_bytes());
            raw.extend_from_slice(&(r.data.len() as u64).to_le_bytes());
            raw.extend_from_slice(&r.data);
        }
        push_section(&mut plain, SECTION_RAW_REGIONS, &raw);
        push_section(&mut plain, SECTION_INDEX, &self.index.encode());

        let mut stored = zstd::bulk::compress(&plain, 3)?;
        let nonce = self.encoder.nonce.fetch_add(1, Ordering::SeqCst);
        if let Some(s) = &self.encoder.sealer {
            stored = s.seal(DOMAIN_METADATA, nonce, &stored, self.header.image_id.as_bytes());
        }
        let footer = Footer {
            total_files: self.header.file_no + 1,
            image_id: self.header.image_id,
            metadata_offset: self.file_offset,
            metadata_len: stored.len() as u64,
            metadata_plain_len: plain.len() as u64,
            metadata_nonce: nonce,
            metadata_hash: *blake3::hash(&stored).as_bytes(),
        };
        self.file.write_all(&stored)?;
        self.file.write_all(&footer.encode())?;
        self.file.sync_all()?;
        let mut total = 0;
        for f in &self.files {
            total += fs::metadata(f)?.len();
        }
        Ok(FinishSummary {
            image_id: self.header.image_id,
            files: self.files,
            total_bytes: total,
            blocks: self.index.total_blocks(),
        })
    }
}

/// First nonce counter of a resumed run: past everything the checkpointed run may have used,
/// plus a random offset that separates runs resumed from the same checkpoint.
fn resume_nonce_start(checkpoint_counter: u64) -> Result<u64> {
    let random = u64::from_le_bytes(crypto::random_bytes::<8>()) >> (64 - RESUME_NONCE_RANDOM_BITS);
    checkpoint_counter
        .checked_add(RESUME_NONCE_GAP)
        .and_then(|n| n.checked_add(random))
        .filter(|&n| n <= MAX_NONCE_START)
        .ok_or_else(|| Error::InvalidArgument("nonce counter exhausted; start a new image".into()))
}

fn push_section(out: &mut Vec<u8>, tag: u32, data: &[u8]) {
    out.extend_from_slice(&tag.to_le_bytes());
    out.extend_from_slice(&(data.len() as u64).to_le_bytes());
    out.extend_from_slice(data);
}

fn remove_parts_from(base: &Path, from: u32) {
    let mut n = from.max(1);
    loop {
        let p = image_file_path(base, n);
        if fs::remove_file(&p).is_err() {
            break;
        }
        n += 1;
    }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Serializes `Vec<u8>` as a compact hex string inside the JSON checkpoint.
mod serde_bytes_compat {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &[u8], s: S) -> Result<S::Ok, S::Error> {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut out = String::with_capacity(v.len() * 2);
        for b in v {
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 15) as usize] as char);
        }
        s.serialize_str(&out)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(d)?;
        let nib = |c: u8| match c {
            b'0'..=b'9' => Ok(c - b'0'),
            b'a'..=b'f' => Ok(c - b'a' + 10),
            _ => Err(serde::de::Error::custom("bad hex")),
        };
        let b = s.as_bytes();
        if b.len() % 2 != 0 {
            return Err(serde::de::Error::custom("odd hex length"));
        }
        b.as_chunks::<2>().0.iter().map(|&[h, l]| Ok((nib(h)? << 4) | nib(l)?)).collect()
    }
}
