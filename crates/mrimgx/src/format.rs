//! Binary structures and the `$JSON` model of a Reflect X image, as defined by
//! `docs/FILE_LAYOUT.md` and `src/libs/file_reader/*.h` of the Macrium sample code.

use serde_json::Value;

use crate::error::{Error, Result};

/// Magic string at the very end of every image file.
pub(crate) const MAGIC: &[u8; 12] = b"MACRIUM_FILE";
/// Footer: `u64 first_metadata_block_header` followed by [`MAGIC`].
pub(crate) const FOOTER_LEN: u64 = 20;
/// Size of a packed `MetadataBlockHeader`.
pub(crate) const HEADER_LEN: u64 = 32;
/// Size of a packed `DataBlockIndexElement`.
pub(crate) const ELEM_LEN: usize = 30;
/// Size of a packed `DeltaDataBlock` (element + `u32 block_index`).
pub(crate) const DELTA_ELEM_LEN: usize = 34;
/// `ExtendedPartitionTable` is not declared packed in `disk_structs.h`, so MSVC pads it
/// from 522 to 528 bytes; both strides are accepted.
const EPT_STRIDES: [usize; 2] = [528, 522];

/// Metadata block names.
pub(crate) mod names {
    pub const JSON: &[u8; 8] = b"$JSON   ";
    pub const TRACK0: &[u8; 8] = b"$TRACK0 ";
    pub const EPT: &[u8; 8] = b"$EPT    ";
    pub const INDEX: &[u8; 8] = b"$INDEX  ";
}

const FLAG_LAST: u8 = 1;
const FLAG_COMPRESSED: u8 = 2;
const FLAG_ENCRYPTED: u8 = 4;

fn u16_at(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes(b[o..o + 2].try_into().unwrap())
}
fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
pub(crate) fn u64_at(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

/// A `MetadataBlockHeader`.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct BlockHeader {
    pub name: [u8; 8],
    pub len: u32,
    pub md5: [u8; 16],
    pub flags: u8,
}

impl BlockHeader {
    pub fn parse(b: &[u8; HEADER_LEN as usize]) -> Self {
        Self { name: b[..8].try_into().unwrap(), len: u32_at(b, 8), md5: b[12..28].try_into().unwrap(), flags: b[28] }
    }

    pub fn last(&self) -> bool {
        self.flags & FLAG_LAST != 0
    }
    pub fn compressed(&self) -> bool {
        self.flags & FLAG_COMPRESSED != 0
    }
    pub fn encrypted(&self) -> bool {
        self.flags & FLAG_ENCRYPTED != 0
    }

    /// Readable block name for error messages.
    pub fn name_str(&self) -> String {
        String::from_utf8_lossy(&self.name).trim_end().to_string()
    }

    #[cfg(test)]
    pub fn new(name: &[u8; 8], len: u32, md5: [u8; 16], last: bool, compressed: bool, encrypted: bool) -> Self {
        let flags = (u8::from(last) * FLAG_LAST)
            | (u8::from(compressed) * FLAG_COMPRESSED)
            | (u8::from(encrypted) * FLAG_ENCRYPTED);
        Self { name: *name, len, md5, flags }
    }

    #[cfg(test)]
    pub fn to_bytes(self) -> [u8; HEADER_LEN as usize] {
        let mut b = [0; HEADER_LEN as usize];
        b[..8].copy_from_slice(&self.name);
        b[8..12].copy_from_slice(&self.len.to_le_bytes());
        b[12..28].copy_from_slice(&self.md5);
        b[28] = self.flags;
        b
    }
}

/// A `DataBlockIndexElement`: where a data block lives and the MD5 of its decoded content.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct IndexElem {
    pub pos: u64,
    pub md5: [u8; 16],
    /// Stored (compressed/encrypted) length; 0 marks a hole.
    pub len: u32,
    pub file: u16,
}

impl IndexElem {
    fn parse(b: &[u8]) -> Result<Self> {
        let pos = i64::from_le_bytes(b[..8].try_into().unwrap());
        let len = u32_at(b, 24);
        let pos = match u64::try_from(pos) {
            Ok(p) => p,
            Err(_) if len == 0 => 0,
            Err(_) => return Err(Error::corrupt(format!("negative data block position {pos}"))),
        };
        Ok(Self { pos, md5: b[8..24].try_into().unwrap(), len, file: u16_at(b, 28) })
    }

    /// `true` if the block was not stored (unused space, reads as zeros).
    pub fn is_hole(&self) -> bool {
        self.len == 0
    }

    #[cfg(test)]
    pub fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.pos.to_le_bytes());
        out.extend_from_slice(&self.md5);
        out.extend_from_slice(&self.len.to_le_bytes());
        out.extend_from_slice(&self.file.to_le_bytes());
    }
}

/// Decoded `$INDEX` payload of one partition.
#[derive(Debug, Default)]
pub(crate) struct PartitionIndex {
    /// `ReservedSectorsIndex` (FAT only).
    pub reserved: Vec<IndexElem>,
    /// `DataBlockIndex` (empty for delta files).
    pub blocks: Vec<IndexElem>,
    /// `DeltaDataBlockIndex` as `(block_index, element)`.
    pub deltas: Vec<(u32, IndexElem)>,
}

/// Reads an `i32` element count followed by that many records; negative counts mean 0
/// (as in `file_reader.cpp`).
fn read_array<'a>(data: &'a [u8], at: &mut usize, stride: usize) -> Result<std::slice::ChunksExact<'a, u8>> {
    let raw = data.get(*at..*at + 4).ok_or_else(|| Error::corrupt("truncated block index"))?;
    let count = usize::try_from(i32::from_le_bytes(raw.try_into().unwrap())).unwrap_or(0);
    *at += 4;
    let bytes = count
        .checked_mul(stride)
        .filter(|&n| n <= data.len() - *at)
        .ok_or_else(|| Error::corrupt(format!("block index count {count} exceeds the $INDEX block")))?;
    let chunk = &data[*at..*at + bytes];
    *at += bytes;
    Ok(chunk.chunks_exact(stride))
}

/// Parses an `$INDEX` payload: reserved sectors index, then the (delta) data block index.
pub(crate) fn parse_index(data: &[u8], delta: bool) -> Result<PartitionIndex> {
    let mut at = 0;
    let reserved = read_array(data, &mut at, ELEM_LEN)?.map(IndexElem::parse).collect::<Result<_>>()?;
    let mut index = PartitionIndex { reserved, ..Default::default() };
    if delta {
        for c in read_array(data, &mut at, DELTA_ELEM_LEN)? {
            index.deltas.push((u32_at(c, ELEM_LEN), IndexElem::parse(c)?));
        }
    } else {
        index.blocks = read_array(data, &mut at, ELEM_LEN)?.map(IndexElem::parse).collect::<Result<_>>()?;
    }
    Ok(index)
}

/// Parses an `$EPT` payload into `(disk byte offset, 512-byte extended boot record)` pairs.
pub(crate) fn parse_ept(data: &[u8]) -> Result<Vec<(u64, [u8; 512])>> {
    let count = data.get(..4).map(|c| u32_at(c, 0) as usize).ok_or_else(|| Error::corrupt("truncated $EPT"))?;
    let rest = &data[4..];
    let stride = EPT_STRIDES
        .into_iter()
        .find(|&s| count.checked_mul(s).is_some_and(|n| n <= rest.len()))
        .ok_or_else(|| Error::corrupt(format!("$EPT count {count} exceeds the block")))?;
    Ok(rest.chunks_exact(stride).take(count).map(|c| (u64_at(c, 512), c[..512].try_into().unwrap())).collect())
}

// ---------------------------------------------------------------------------------------------
// JSON model
// ---------------------------------------------------------------------------------------------

/// Lenient unsigned integer: JSON number (non-negative) or a decimal string.
fn num(v: &Value) -> Option<u64> {
    match v {
        Value::Number(n) => n.as_u64().or_else(|| n.as_f64().filter(|f| *f >= 0.0 && f.is_finite()).map(|f| f as u64)),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn num_or(v: &Value, default: u64) -> u64 {
    num(v).unwrap_or(default)
}

fn int(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn flag(v: &Value) -> Option<bool> {
    match v {
        Value::Bool(b) => Some(*b),
        Value::Number(n) => n.as_u64().map(|n| n != 0),
        _ => None,
    }
}

fn small<T: TryFrom<u64>>(v: &Value, what: &str) -> Result<T> {
    num(v).and_then(|n| T::try_from(n).ok()).ok_or_else(|| Error::BadFormat(format!("missing or invalid {what}")))
}

/// `_encryption` node (only kept when encryption is enabled).
#[derive(Debug, Clone)]
pub(crate) struct EncryptionJson {
    pub aes_type: String,
    pub key_iterations: u32,
    pub hmac: String,
}

/// The parts of the `$JSON` block this reader uses.
#[derive(Debug, Clone)]
pub(crate) struct ImageJson {
    pub image_id: String,
    pub image_id_bin: [u8; 8],
    pub file_number: u16,
    pub increment_number: i64,
    pub merged_files: Vec<u16>,
    pub split_file: bool,
    pub netbios_name: String,
    pub backup_time: i64,
    pub index_file_position: u64,
    pub delta_index: bool,
    pub backup_type: String,
    pub encryption: Option<EncryptionJson>,
    pub compressed: bool,
    pub comment: String,
    pub disks: Vec<DiskJson>,
}

/// One entry of `disks`.
#[derive(Debug, Clone)]
pub(crate) struct DiskJson {
    pub format: String,
    pub number: u16,
    pub signature: String,
    pub sector_size: u32,
    pub size: u64,
    pub partitions: Vec<PartitionJson>,
}

/// One entry of `disks[].partitions`.
#[derive(Debug, Clone)]
pub(crate) struct PartitionJson {
    pub number: u16,
    pub block_size: u32,
    pub block_count: u64,
    pub start: u64,
    pub end: u64,
    pub length: u64,
    pub boot_sector_offset: u64,
    pub mbr_type: Option<u8>,
    pub fs_type: String,
    pub fs_start: u64,
    pub lcn0_offset: u64,
    pub reserved_len: u64,
    pub label: String,
    pub drive_letter: Option<char>,
    pub total_clusters: u64,
    pub free_clusters: u64,
    pub sectors_per_cluster: u64,
}

/// Largest accepted data block size (the sample uses 64 KiB).
pub(crate) const MAX_BLOCK_SIZE: u32 = 64 << 20;

impl ImageJson {
    /// Parses and validates the `$JSON` text.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        // The JSON may be NUL terminated.
        let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
        let j: Value = serde_json::from_slice(&bytes[..end]).map_err(|e| Error::BadFormat(format!("$JSON: {e}")))?;
        if !j.is_object() {
            return Err(Error::BadFormat("$JSON is not an object".into()));
        }
        let h = &j["_header"];
        let image_id = text(&h["imageid"]);
        let image_id_bin = crate::crypto::decode_hex(&image_id)
            .and_then(|b| <[u8; 8]>::try_from(b).ok())
            .ok_or_else(|| Error::BadFormat(format!("invalid image id {image_id:?}")))?;
        let backup_type = text(&h["backup_type"]);
        let e = &j["_encryption"];
        // Data blocks are decrypted whenever aes_type != "none" (restore.cpp); `enable` gates the
        // password check (file_reader.cpp). Treat either as "encrypted".
        let aes_type = text(&e["aes_type"]);
        let enabled = flag(&e["enable"]).unwrap_or(false) || aes_type.starts_with("aes-");
        let encryption = enabled.then(|| EncryptionJson {
            aes_type,
            key_iterations: num(&e["key_iterations"]).and_then(|n| u32::try_from(n).ok()).unwrap_or(0),
            hmac: text(&e["hmac"]),
        });
        // compression_level "none"/"medium"/"high"; unknown values are treated as "none" like
        // the reference enum conversion does.
        let compressed = matches!(text(&j["_compression"]["compression_level"]).as_str(), "medium" | "high");
        let disks = match &j["disks"] {
            Value::Array(a) => a.iter().map(DiskJson::parse).collect::<Result<_>>()?,
            Value::Null => Vec::new(),
            _ => return Err(Error::BadFormat("`disks` is not an array".into())),
        };
        Ok(Self {
            image_id,
            image_id_bin,
            file_number: small(&h["file_number"], "file_number").unwrap_or(0),
            increment_number: int(&h["increment_number"]).unwrap_or(0),
            merged_files: match &h["merged_files"] {
                Value::Array(a) => a.iter().filter_map(|v| num(v).and_then(|n| u16::try_from(n).ok())).collect(),
                _ => Vec::new(),
            },
            split_file: flag(&h["split_file"]).unwrap_or(false),
            netbios_name: text(&h["netbios_name"]),
            backup_time: int(&h["backup_time"]).unwrap_or(0),
            index_file_position: num_or(&h["index_file_position"], 0),
            // Marked [REQUIRED] in file_structs.h (C++ default `true`); when absent we infer
            // it from the backup type since a full image cannot hold a delta index.
            delta_index: flag(&h["delta_index"]).unwrap_or(backup_type != "full"),
            backup_type,
            encryption,
            compressed,
            comment: text(&j["_auxiliary_data"]["backup_definition"]["comment"]),
            disks,
        })
    }
}

impl DiskJson {
    fn parse(d: &Value) -> Result<Self> {
        let h = &d["_header"];
        let g = &d["_geometry"];
        let sector_size = num(&g["bytes_per_sector"]).and_then(|n| u32::try_from(n).ok()).unwrap_or(512);
        if !(512..=65536).contains(&sector_size) || !sector_size.is_power_of_two() {
            return Err(Error::BadFormat(format!("invalid sector size {sector_size}")));
        }
        let partitions = match &d["partitions"] {
            Value::Array(a) => a.iter().map(PartitionJson::parse).collect::<Result<_>>()?,
            Value::Null => Vec::new(),
            _ => return Err(Error::BadFormat("`partitions` is not an array".into())),
        };
        Ok(Self {
            format: text(&h["disk_format"]),
            number: small(&h["disk_number"], "disk_number").unwrap_or(0),
            signature: text(&h["disk_signature"]),
            sector_size,
            size: num_or(&g["disk_size"], 0),
            partitions,
        })
    }
}

impl PartitionJson {
    fn parse(p: &Value) -> Result<Self> {
        let h = &p["_header"];
        let g = &p["_geometry"];
        let fs = &p["_file_system"];
        let block_size: u32 = small(&h["block_size"], "partition block_size")?;
        if block_size == 0 || block_size > MAX_BLOCK_SIZE {
            return Err(Error::BadFormat(format!("invalid partition block size {block_size}")));
        }
        let fs_start = num_or(&fs["start"], 0);
        let lcn0_offset = num(&fs["lcn0_offset"]).unwrap_or(fs_start);
        if lcn0_offset < fs_start {
            return Err(Error::BadFormat("lcn0_offset precedes the file system start".into()));
        }
        // drive_letter is an integer character code in the sample (72 = 'H'); accept a string too.
        let drive_letter = match &fs["drive_letter"] {
            Value::String(s) => s.chars().next(),
            v => num(v).and_then(|n| u32::try_from(n).ok()).and_then(char::from_u32),
        }
        .filter(char::is_ascii_alphabetic)
        .map(|c| c.to_ascii_uppercase());
        Ok(Self {
            number: small(&h["partition_number"], "partition_number").unwrap_or(0),
            block_size,
            block_count: num_or(&h["block_count"], 0),
            start: num_or(&g["start"], 0),
            end: num_or(&g["end"], 0),
            length: num_or(&g["length"], 0),
            boot_sector_offset: num_or(&g["boot_sector_offset"], 0),
            mbr_type: num(&p["_partition_table_entry"]["type"]).and_then(|n| u8::try_from(n).ok()),
            fs_type: text(&fs["type"]),
            fs_start,
            lcn0_offset,
            reserved_len: num_or(&fs["reserved_sectors_byte_length"], 0),
            label: text(&fs["volume_label"]),
            drive_letter,
            total_clusters: num_or(&fs["total_clusters"], 0),
            free_clusters: num_or(&fs["free_clusters"], 0),
            sectors_per_cluster: num_or(&fs["sectors_per_cluster"], 0),
        })
    }

    /// Partition length: `_geometry.length`, else the inclusive `start..=end` range.
    pub fn len(&self) -> u64 {
        if self.length > 0 {
            self.length
        } else if self.end >= self.start && self.end > 0 {
            self.end - self.start + 1
        } else {
            0
        }
    }

    /// Offset of data block 0 relative to the partition start.
    pub fn data_base(&self) -> u64 {
        self.lcn0_offset - self.fs_start
    }
}
