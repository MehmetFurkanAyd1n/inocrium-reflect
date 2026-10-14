//! Per-block encoding: hash → zstd → AES-256-GCM, and the inverse.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use uuid::Uuid;

use crate::crypto::{Sealer, DOMAIN_DATA};
use crate::index::{IndexEntry, ENTRY_COMPRESSED, ENTRY_ENCRYPTED};
use crate::{Error, Result};

/// Identifies one block of one stream of one disk inside an image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BlockRef {
    pub disk: u32,
    pub stream: u32,
    pub block: u64,
}

impl BlockRef {
    /// Additional authenticated data binding a sealed block to its position in the image,
    /// so that swapping or moving blocks is detected.
    pub fn aad(&self, image_id: &Uuid) -> [u8; 32] {
        let mut a = [0u8; 32];
        a[..16].copy_from_slice(image_id.as_bytes());
        a[16..20].copy_from_slice(&self.disk.to_le_bytes());
        a[20..24].copy_from_slice(&self.stream.to_le_bytes());
        a[24..32].copy_from_slice(&self.block.to_le_bytes());
        a
    }
}

/// A block ready to be appended to the image.
#[derive(Debug, Clone)]
pub struct EncodedBlock {
    pub at: BlockRef,
    pub flags: u8,
    pub plain_len: u32,
    pub nonce: u64,
    pub hash: [u8; 16],
    pub data: Vec<u8>,
}

pub(crate) fn block_hash(plain: &[u8]) -> [u8; 16] {
    let mut h = [0u8; 16];
    h.copy_from_slice(&blake3::hash(plain).as_bytes()[..16]);
    h
}

/// Thread-safe, cheaply clonable block encoder. Created by [`crate::ImageWriter::encoder`] and
/// meant to be shared by a pool of worker threads.
#[derive(Clone, Debug)]
pub struct BlockEncoder {
    pub(crate) image_id: Uuid,
    pub(crate) level: i32,
    pub(crate) sealer: Option<Sealer>,
    pub(crate) nonce: Arc<AtomicU64>,
}

impl BlockEncoder {
    pub fn encode(&self, at: BlockRef, plain: &[u8]) -> EncodedBlock {
        let hash = block_hash(plain);
        let mut flags = 0u8;
        let mut data = None;
        if self.level > 0 {
            if let Ok(c) = zstd::bulk::compress(plain, self.level) {
                if c.len() < plain.len() {
                    flags |= ENTRY_COMPRESSED;
                    data = Some(c);
                }
            }
        }
        let mut data = data.unwrap_or_else(|| plain.to_vec());
        let nonce = self.nonce.fetch_add(1, Ordering::Relaxed);
        if let Some(s) = &self.sealer {
            flags |= ENTRY_ENCRYPTED;
            data = s.seal(DOMAIN_DATA, nonce, &data, &at.aad(&self.image_id));
        }
        EncodedBlock { at, flags, plain_len: plain.len() as u32, nonce, hash, data }
    }
}

/// Decodes a stored block and checks it against its index entry.
pub(crate) fn decode_block(
    image_id: &Uuid,
    sealer: Option<&Sealer>,
    at: BlockRef,
    entry: &IndexEntry,
    stored: &[u8],
) -> Result<Vec<u8>> {
    let corrupt = || Error::CorruptBlock { disk: at.disk, stream: at.stream, block: at.block };
    let opened;
    let mut data: &[u8] = stored;
    if entry.flags & ENTRY_ENCRYPTED != 0 {
        let s = sealer.ok_or(Error::PasswordRequired)?;
        opened = s.open(DOMAIN_DATA, entry.nonce, stored, &at.aad(image_id)).ok_or_else(corrupt)?;
        data = &opened;
    }
    let plain = if entry.flags & ENTRY_COMPRESSED != 0 {
        zstd::bulk::decompress(data, entry.plain_len as usize).map_err(|_| corrupt())?
    } else {
        data.to_vec()
    };
    if plain.len() != entry.plain_len as usize || block_hash(&plain) != entry.hash {
        return Err(corrupt());
    }
    Ok(plain)
}
