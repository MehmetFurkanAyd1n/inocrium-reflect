//! Fixed-size on-disk structures: the per-file header and the trailing footer.
//!
//! All integers are little-endian.

use uuid::Uuid;

use crate::crypto::{KdfParams, SALT_LEN, WRAPPED_KEY_LEN, WRAP_NONCE_LEN};
use crate::{Error, Result};

pub const FORMAT_VERSION: u32 = 1;
pub const HEADER_SIZE: usize = 4096;
pub const FOOTER_SIZE: usize = 128;

const HEADER_MAGIC: &[u8; 8] = b"INOCRIUM";
const FOOTER_MAGIC: &[u8; 8] = b"INOCFOOT";
const FOOTER_END_MAGIC: &[u8; 8] = b"INOCEND\0";
const HEADER_HASH_OFFSET: usize = HEADER_SIZE - 32;

pub const FLAG_ENCRYPTED: u32 = 1 << 0;
pub const FLAG_COMPRESSED: u32 = 1 << 1;

const KDF_ARGON2ID: u32 = 1;

/// Key material stored in the header of every file of an encrypted image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeySlot {
    pub kdf: KdfParams,
    pub salt: [u8; SALT_LEN],
    pub wrap_nonce: [u8; WRAP_NONCE_LEN],
    pub wrapped_key: [u8; WRAPPED_KEY_LEN],
}

/// Header at the start of every image file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileHeader {
    pub version: u32,
    pub flags: u32,
    /// 0 for `name.irimg`, n for `name.iNN`.
    pub file_no: u32,
    pub image_id: Uuid,
    pub block_size: u32,
    pub created_unix: i64,
    pub key_slot: Option<KeySlot>,
}

impl FileHeader {
    pub fn encrypted(&self) -> bool {
        self.flags & FLAG_ENCRYPTED != 0
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut b = vec![0u8; HEADER_SIZE];
        b[0..8].copy_from_slice(HEADER_MAGIC);
        put_u32(&mut b, 8, self.version);
        put_u32(&mut b, 12, HEADER_SIZE as u32);
        put_u32(&mut b, 16, self.flags);
        put_u32(&mut b, 20, self.file_no);
        b[24..40].copy_from_slice(self.image_id.as_bytes());
        put_u32(&mut b, 40, self.block_size);
        b[152..160].copy_from_slice(&self.created_unix.to_le_bytes());
        if let Some(slot) = &self.key_slot {
            put_u32(&mut b, 44, KDF_ARGON2ID);
            put_u32(&mut b, 48, slot.kdf.memory_kib);
            put_u32(&mut b, 52, slot.kdf.iterations);
            put_u32(&mut b, 56, slot.kdf.parallelism);
            b[60..92].copy_from_slice(&slot.salt);
            b[92..104].copy_from_slice(&slot.wrap_nonce);
            b[104..152].copy_from_slice(&slot.wrapped_key);
        }
        let hash = blake3::hash(&b[..HEADER_HASH_OFFSET]);
        b[HEADER_HASH_OFFSET..].copy_from_slice(hash.as_bytes());
        b
    }

    pub fn decode(b: &[u8]) -> Result<Self> {
        if b.len() < HEADER_SIZE || &b[0..8] != HEADER_MAGIC {
            return Err(Error::BadMagic);
        }
        let version = get_u32(b, 8);
        if version != FORMAT_VERSION {
            return Err(Error::UnsupportedVersion(version));
        }
        if get_u32(b, 12) as usize != HEADER_SIZE {
            return Err(Error::CorruptHeader);
        }
        if blake3::hash(&b[..HEADER_HASH_OFFSET]).as_bytes() != &b[HEADER_HASH_OFFSET..HEADER_SIZE] {
            return Err(Error::CorruptHeader);
        }
        let flags = get_u32(b, 16);
        let key_slot = if flags & FLAG_ENCRYPTED != 0 {
            if get_u32(b, 44) != KDF_ARGON2ID {
                return Err(Error::CorruptHeader);
            }
            Some(KeySlot {
                kdf: KdfParams { memory_kib: get_u32(b, 48), iterations: get_u32(b, 52), parallelism: get_u32(b, 56) },
                salt: b[60..92].try_into().unwrap(),
                wrap_nonce: b[92..104].try_into().unwrap(),
                wrapped_key: b[104..152].try_into().unwrap(),
            })
        } else {
            None
        };
        Ok(Self {
            version,
            flags,
            file_no: get_u32(b, 20),
            image_id: Uuid::from_bytes(b[24..40].try_into().unwrap()),
            block_size: get_u32(b, 40),
            created_unix: i64::from_le_bytes(b[152..160].try_into().unwrap()),
            key_slot,
        })
    }
}

/// Trailer at the very end of the last image file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Footer {
    pub total_files: u32,
    pub image_id: Uuid,
    /// Offset of the metadata blob within the last file.
    pub metadata_offset: u64,
    /// Stored (compressed, possibly encrypted) length of the metadata blob.
    pub metadata_len: u64,
    /// Length of the metadata after decryption and decompression.
    pub metadata_plain_len: u64,
    /// Nonce counter used to seal the metadata.
    pub metadata_nonce: u64,
    /// BLAKE3 of the stored metadata bytes.
    pub metadata_hash: [u8; 32],
}

impl Footer {
    pub fn encode(&self) -> Vec<u8> {
        let mut b = vec![0u8; FOOTER_SIZE];
        b[0..8].copy_from_slice(FOOTER_MAGIC);
        put_u32(&mut b, 8, FORMAT_VERSION);
        put_u32(&mut b, 12, self.total_files);
        b[16..32].copy_from_slice(self.image_id.as_bytes());
        put_u64(&mut b, 32, self.metadata_offset);
        put_u64(&mut b, 40, self.metadata_len);
        put_u64(&mut b, 48, self.metadata_plain_len);
        put_u64(&mut b, 56, self.metadata_nonce);
        b[64..96].copy_from_slice(&self.metadata_hash);
        b[120..128].copy_from_slice(FOOTER_END_MAGIC);
        b
    }

    pub fn decode(b: &[u8]) -> Result<Self> {
        if b.len() < FOOTER_SIZE || &b[0..8] != FOOTER_MAGIC || &b[120..128] != FOOTER_END_MAGIC {
            return Err(Error::CorruptFooter);
        }
        let version = get_u32(b, 8);
        if version != FORMAT_VERSION {
            return Err(Error::UnsupportedVersion(version));
        }
        Ok(Self {
            total_files: get_u32(b, 12),
            image_id: Uuid::from_bytes(b[16..32].try_into().unwrap()),
            metadata_offset: get_u64(b, 32),
            metadata_len: get_u64(b, 40),
            metadata_plain_len: get_u64(b, 48),
            metadata_nonce: get_u64(b, 56),
            metadata_hash: b[64..96].try_into().unwrap(),
        })
    }
}

pub(crate) fn put_u32(b: &mut [u8], off: usize, v: u32) {
    b[off..off + 4].copy_from_slice(&v.to_le_bytes());
}
pub(crate) fn put_u64(b: &mut [u8], off: usize, v: u64) {
    b[off..off + 8].copy_from_slice(&v.to_le_bytes());
}
pub(crate) fn get_u32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(b[off..off + 4].try_into().unwrap())
}
pub(crate) fn get_u64(b: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(b[off..off + 8].try_into().unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_roundtrip_and_tamper() {
        let h = FileHeader {
            version: FORMAT_VERSION,
            flags: FLAG_ENCRYPTED | FLAG_COMPRESSED,
            file_no: 3,
            image_id: Uuid::new_v4(),
            block_size: 1 << 20,
            created_unix: 1_700_000_000,
            key_slot: Some(KeySlot {
                kdf: KdfParams::default(),
                salt: [7; SALT_LEN],
                wrap_nonce: [9; WRAP_NONCE_LEN],
                wrapped_key: [1; WRAPPED_KEY_LEN],
            }),
        };
        let mut enc = h.encode();
        assert_eq!(FileHeader::decode(&enc).unwrap(), h);
        enc[20] ^= 1;
        assert!(matches!(FileHeader::decode(&enc), Err(Error::CorruptHeader)));
    }

    #[test]
    fn footer_roundtrip() {
        let f = Footer {
            total_files: 2,
            image_id: Uuid::new_v4(),
            metadata_offset: 12345,
            metadata_len: 99,
            metadata_plain_len: 400,
            metadata_nonce: 77,
            metadata_hash: [5; 32],
        };
        assert_eq!(Footer::decode(&f.encode()).unwrap(), f);
    }
}
