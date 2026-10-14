//! Block index: maps (disk, stream, block number) to the location of the stored block.

use std::collections::BTreeMap;

use crate::format::{get_u32, get_u64, put_u32, put_u64};
use crate::{Error, Result};

/// The stored block is zstd-compressed (otherwise it is stored raw).
pub const ENTRY_COMPRESSED: u8 = 1 << 0;
/// The stored block is sealed with AES-256-GCM.
pub const ENTRY_ENCRYPTED: u8 = 1 << 1;

const ENTRY_SIZE: usize = 56;
const STREAM_HEADER_SIZE: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexEntry {
    /// Block number within the stream.
    pub block: u64,
    /// Split file number holding the block.
    pub file_no: u32,
    pub flags: u8,
    /// Offset of the stored block within its file.
    pub offset: u64,
    /// Stored length (after compression / encryption).
    pub stored_len: u32,
    /// Length of the plain block.
    pub plain_len: u32,
    /// Nonce counter used to seal the block.
    pub nonce: u64,
    /// First 16 bytes of BLAKE3 over the plain block.
    pub hash: [u8; 16],
}

impl IndexEntry {
    fn encode_into(&self, out: &mut Vec<u8>) {
        let mut b = [0u8; ENTRY_SIZE];
        put_u64(&mut b, 0, self.block);
        put_u32(&mut b, 8, self.file_no);
        b[12] = self.flags;
        put_u64(&mut b, 16, self.offset);
        put_u32(&mut b, 24, self.stored_len);
        put_u32(&mut b, 28, self.plain_len);
        put_u64(&mut b, 32, self.nonce);
        b[40..56].copy_from_slice(&self.hash);
        out.extend_from_slice(&b);
    }

    fn decode(b: &[u8]) -> Self {
        Self {
            block: get_u64(b, 0),
            file_no: get_u32(b, 8),
            flags: b[12],
            offset: get_u64(b, 16),
            stored_len: get_u32(b, 24),
            plain_len: get_u32(b, 28),
            nonce: get_u64(b, 32),
            hash: b[40..56].try_into().unwrap(),
        }
    }
}

/// Index of all blocks, keyed by (disk, stream); entries sorted by block number.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BlockIndex {
    pub streams: BTreeMap<(u32, u32), Vec<IndexEntry>>,
}

impl BlockIndex {
    pub fn push(&mut self, disk: u32, stream: u32, entry: IndexEntry) {
        let v = self.streams.entry((disk, stream)).or_default();
        // Blocks almost always arrive in order; keep the vector sorted regardless.
        match v.last() {
            Some(last) if last.block >= entry.block => match v.binary_search_by_key(&entry.block, |e| e.block) {
                Ok(i) => v[i] = entry,
                Err(i) => v.insert(i, entry),
            },
            _ => v.push(entry),
        }
    }

    pub fn lookup(&self, disk: u32, stream: u32, block: u64) -> Option<&IndexEntry> {
        let v = self.streams.get(&(disk, stream))?;
        v.binary_search_by_key(&block, |e| e.block).ok().map(|i| &v[i])
    }

    pub fn total_blocks(&self) -> u64 {
        self.streams.values().map(|v| v.len() as u64).sum()
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.total_blocks() as usize * ENTRY_SIZE + 64);
        out.extend_from_slice(&(self.streams.len() as u32).to_le_bytes());
        for ((disk, stream), entries) in &self.streams {
            let mut h = [0u8; STREAM_HEADER_SIZE];
            put_u32(&mut h, 0, *disk);
            put_u32(&mut h, 4, *stream);
            put_u64(&mut h, 8, entries.len() as u64);
            out.extend_from_slice(&h);
            for e in entries {
                e.encode_into(&mut out);
            }
        }
        out
    }

    pub fn decode(b: &[u8]) -> Result<Self> {
        if b.len() < 4 {
            return Err(Error::CorruptMetadata);
        }
        let count = get_u32(b, 0);
        let mut pos = 4;
        let mut idx = BlockIndex::default();
        for _ in 0..count {
            if b.len() < pos + STREAM_HEADER_SIZE {
                return Err(Error::CorruptMetadata);
            }
            let disk = get_u32(b, pos);
            let stream = get_u32(b, pos + 4);
            let n = get_u64(b, pos + 8) as usize;
            pos += STREAM_HEADER_SIZE;
            let end = n.checked_mul(ENTRY_SIZE).and_then(|l| l.checked_add(pos)).ok_or(Error::CorruptMetadata)?;
            if b.len() < end {
                return Err(Error::CorruptMetadata);
            }
            let mut v = Vec::with_capacity(n);
            for chunk in b[pos..end].as_chunks::<ENTRY_SIZE>().0 {
                v.push(IndexEntry::decode(chunk));
            }
            if v.windows(2).any(|w| w[0].block >= w[1].block) {
                return Err(Error::CorruptMetadata);
            }
            pos = end;
            idx.streams.insert((disk, stream), v);
        }
        Ok(idx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(block: u64) -> IndexEntry {
        IndexEntry {
            block,
            file_no: 1,
            flags: 3,
            offset: block * 10,
            stored_len: 9,
            plain_len: 10,
            nonce: block,
            hash: [block as u8; 16],
        }
    }

    #[test]
    fn roundtrip_and_ordering() {
        let mut idx = BlockIndex::default();
        idx.push(0, 1, e(0));
        idx.push(0, 1, e(5));
        idx.push(0, 1, e(2));
        idx.push(0, 2, e(1));
        idx.push(0, 1, e(5));
        assert_eq!(idx.streams[&(0, 1)].iter().map(|x| x.block).collect::<Vec<_>>(), vec![0, 2, 5]);
        let dec = BlockIndex::decode(&idx.encode()).unwrap();
        assert_eq!(dec, idx);
        assert_eq!(dec.lookup(0, 1, 2).unwrap().offset, 20);
        assert!(dec.lookup(0, 1, 3).is_none());
    }
}
