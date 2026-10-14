//! GUID partition table and protective MBR construction.

use uuid::Uuid;

pub const GPT_SIGNATURE: &[u8; 8] = b"EFI PART";
pub const GPT_REVISION: u32 = 0x0001_0000;
pub const GPT_HEADER_SIZE: u32 = 92;
pub const GPT_ENTRY_SIZE: u32 = 128;
pub const DEFAULT_ENTRY_COUNT: u32 = 128;

/// `EBD0A0A2-B9E5-4433-87C0-68B6B72699C7`
pub const BASIC_DATA: Uuid = Uuid::from_u128(0xebd0a0a2_b9e5_4433_87c0_68b6b72699c7);
/// `C12A7328-F81F-11D2-BA4B-00A0C93EC93B`
pub const EFI_SYSTEM: Uuid = Uuid::from_u128(0xc12a7328_f81f_11d2_ba4b_00a0c93ec93b);
/// `E3C9E316-0B5C-4DB8-817D-F92DF00215AE`
pub const MICROSOFT_RESERVED: Uuid = Uuid::from_u128(0xe3c9e316_0b5c_4db8_817d_f92df00215ae);
/// `DE94BBA4-06D1-4D40-A16A-BFD50179D6AC`
pub const WINDOWS_RECOVERY: Uuid = Uuid::from_u128(0xde94bba4_06d1_4d40_a16a_bfd50179d6ac);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GptEntry {
    pub type_id: Uuid,
    pub id: Uuid,
    pub first_lba: u64,
    pub last_lba: u64,
    pub attributes: u64,
    pub name: String,
}

impl GptEntry {
    pub fn encode(&self) -> [u8; GPT_ENTRY_SIZE as usize] {
        let mut b = [0u8; GPT_ENTRY_SIZE as usize];
        b[0..16].copy_from_slice(&self.type_id.to_bytes_le());
        b[16..32].copy_from_slice(&self.id.to_bytes_le());
        b[32..40].copy_from_slice(&self.first_lba.to_le_bytes());
        b[40..48].copy_from_slice(&self.last_lba.to_le_bytes());
        b[48..56].copy_from_slice(&self.attributes.to_le_bytes());
        for (i, u) in self.name.encode_utf16().take(36).enumerate() {
            b[56 + i * 2..58 + i * 2].copy_from_slice(&u.to_le_bytes());
        }
        b
    }

    /// Returns `None` for unused entries.
    pub fn decode(b: &[u8]) -> Option<Self> {
        let type_id = Uuid::from_bytes_le(b[0..16].try_into().ok()?);
        if type_id.is_nil() {
            return None;
        }
        let units: Vec<u16> =
            b[56..128].as_chunks::<2>().0.iter().map(|&c| u16::from_le_bytes(c)).take_while(|&u| u != 0).collect();
        Some(Self {
            type_id,
            id: Uuid::from_bytes_le(b[16..32].try_into().ok()?),
            first_lba: u64::from_le_bytes(b[32..40].try_into().ok()?),
            last_lba: u64::from_le_bytes(b[40..48].try_into().ok()?),
            attributes: u64::from_le_bytes(b[48..56].try_into().ok()?),
            name: String::from_utf16_lossy(&units),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GptHeader {
    pub my_lba: u64,
    pub alternate_lba: u64,
    pub first_usable_lba: u64,
    pub last_usable_lba: u64,
    pub disk_id: Uuid,
    pub entries_lba: u64,
    pub entry_count: u32,
    pub entry_size: u32,
    pub entries_crc: u32,
}

impl GptHeader {
    /// Encodes the header into a full sector, computing the header CRC.
    pub fn encode(&self, sector_size: u32) -> Vec<u8> {
        let mut b = vec![0u8; sector_size as usize];
        b[0..8].copy_from_slice(GPT_SIGNATURE);
        b[8..12].copy_from_slice(&GPT_REVISION.to_le_bytes());
        b[12..16].copy_from_slice(&GPT_HEADER_SIZE.to_le_bytes());
        b[24..32].copy_from_slice(&self.my_lba.to_le_bytes());
        b[32..40].copy_from_slice(&self.alternate_lba.to_le_bytes());
        b[40..48].copy_from_slice(&self.first_usable_lba.to_le_bytes());
        b[48..56].copy_from_slice(&self.last_usable_lba.to_le_bytes());
        b[56..72].copy_from_slice(&self.disk_id.to_bytes_le());
        b[72..80].copy_from_slice(&self.entries_lba.to_le_bytes());
        b[80..84].copy_from_slice(&self.entry_count.to_le_bytes());
        b[84..88].copy_from_slice(&self.entry_size.to_le_bytes());
        b[88..92].copy_from_slice(&self.entries_crc.to_le_bytes());
        let crc = crc32fast::hash(&b[..GPT_HEADER_SIZE as usize]);
        b[16..20].copy_from_slice(&crc.to_le_bytes());
        b
    }

    /// Parses and validates (signature and header CRC) a GPT header sector.
    pub fn decode(b: &[u8]) -> Option<Self> {
        if b.len() < GPT_HEADER_SIZE as usize || &b[0..8] != GPT_SIGNATURE {
            return None;
        }
        let u32_at = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
        let u64_at = |o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap());
        let size = u32_at(12) as usize;
        if !(GPT_HEADER_SIZE as usize..=b.len()).contains(&size) {
            return None;
        }
        let mut copy = b[..size].to_vec();
        copy[16..20].fill(0);
        if crc32fast::hash(&copy) != u32_at(16) {
            return None;
        }
        Some(Self {
            my_lba: u64_at(24),
            alternate_lba: u64_at(32),
            first_usable_lba: u64_at(40),
            last_usable_lba: u64_at(48),
            disk_id: Uuid::from_bytes_le(b[56..72].try_into().unwrap()),
            entries_lba: u64_at(72),
            entry_count: u32_at(80),
            entry_size: u32_at(84),
            entries_crc: u32_at(88),
        })
    }
}

/// Encodes a partition entry array of `count` slots.
pub fn encode_entries(entries: &[GptEntry], count: u32) -> Vec<u8> {
    let mut b = vec![0u8; (count * GPT_ENTRY_SIZE) as usize];
    for (i, e) in entries.iter().take(count as usize).enumerate() {
        b[i * GPT_ENTRY_SIZE as usize..(i + 1) * GPT_ENTRY_SIZE as usize].copy_from_slice(&e.encode());
    }
    b
}

pub fn decode_entries(b: &[u8], entry_size: u32) -> Vec<GptEntry> {
    if entry_size < GPT_ENTRY_SIZE {
        return Vec::new();
    }
    b.chunks_exact(entry_size as usize).filter_map(GptEntry::decode).collect()
}

/// A complete GPT, ready to be laid out on a disk of a given size.
#[derive(Debug, Clone)]
pub struct Gpt {
    pub disk_id: Uuid,
    pub sector_size: u32,
    pub entry_count: u32,
    pub entries: Vec<GptEntry>,
}

/// The sectors making up a GPT: the protective MBR, primary header and array at the start of
/// the disk, and the backup array and header at the end.
#[derive(Debug, Clone)]
pub struct GptSectors {
    /// Bytes for LBA 0 up to (excluding) `first_usable_lba`.
    pub primary: Vec<u8>,
    /// Byte offset of `backup` on the disk.
    pub backup_offset: u64,
    /// Backup partition entry array followed by the backup header (last sector of the disk).
    pub backup: Vec<u8>,
    pub first_usable_lba: u64,
    pub last_usable_lba: u64,
}

impl Gpt {
    pub fn entry_array_sectors(&self) -> u64 {
        ((self.entry_count * GPT_ENTRY_SIZE) as u64).div_ceil(self.sector_size as u64)
    }

    /// Lays the table out for a disk of `disk_size` bytes. `first_usable_lba` may be raised to
    /// preserve an original layout; it never goes below the minimum the array needs.
    pub fn build(&self, disk_size: u64, first_usable_lba: Option<u64>, protective_mbr: Option<&[u8]>) -> GptSectors {
        let ss = self.sector_size as u64;
        let total = disk_size / ss;
        let array = self.entry_array_sectors();
        let first_usable = first_usable_lba.unwrap_or(0).max(2 + array);
        let last_usable = total - 2 - array;
        let entries = encode_entries(&self.entries, self.entry_count);
        let entries_crc = crc32fast::hash(&entries);
        let mut primary_header = GptHeader {
            my_lba: 1,
            alternate_lba: total - 1,
            first_usable_lba: first_usable,
            last_usable_lba: last_usable,
            disk_id: self.disk_id,
            entries_lba: 2,
            entry_count: self.entry_count,
            entry_size: GPT_ENTRY_SIZE,
            entries_crc,
        };
        let mut primary = vec![0u8; (first_usable * ss) as usize];
        match protective_mbr {
            Some(m) if m.len() >= 512 => primary[..512].copy_from_slice(&m[..512]),
            _ => primary[..512].copy_from_slice(&crate::mbr::protective_mbr(total)),
        }
        primary[ss as usize..2 * ss as usize].copy_from_slice(&primary_header.encode(self.sector_size));
        primary[2 * ss as usize..2 * ss as usize + entries.len()].copy_from_slice(&entries);

        primary_header.my_lba = total - 1;
        primary_header.alternate_lba = 1;
        primary_header.entries_lba = total - 1 - array;
        let mut backup = vec![0u8; ((array + 1) * ss) as usize];
        backup[..entries.len()].copy_from_slice(&entries);
        backup[(array * ss) as usize..].copy_from_slice(&primary_header.encode(self.sector_size));
        GptSectors {
            primary,
            backup_offset: (total - 1 - array) * ss,
            backup,
            first_usable_lba: first_usable,
            last_usable_lba: last_usable,
        }
    }
}

/// Parses the primary GPT of a disk from its first sectors (`head` must contain LBA 0 through
/// the end of the entry array).
pub fn parse_primary(head: &[u8], sector_size: u32) -> Option<(GptHeader, Vec<GptEntry>)> {
    let ss = sector_size as usize;
    let header = GptHeader::decode(head.get(ss..2 * ss)?)?;
    let start = header.entries_lba as usize * ss;
    let len = (header.entry_count * header.entry_size) as usize;
    let array = head.get(start..start + len)?;
    if crc32fast::hash(array) != header.entries_crc {
        return None;
    }
    Some((header.clone(), decode_entries(array, header.entry_size)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_and_parse() {
        let gpt = Gpt {
            disk_id: Uuid::new_v4(),
            sector_size: 512,
            entry_count: DEFAULT_ENTRY_COUNT,
            entries: vec![GptEntry {
                type_id: BASIC_DATA,
                id: Uuid::new_v4(),
                first_lba: 2048,
                last_lba: 4095,
                attributes: 0,
                name: "Basic data partition".into(),
            }],
        };
        let size = 8 << 20;
        let s = gpt.build(size, Some(2048), None);
        assert_eq!(s.primary.len(), 2048 * 512);
        assert_eq!(s.backup_offset + s.backup.len() as u64, size);
        assert_eq!(s.primary[510..512], [0x55, 0xAA]);
        let (h, e) = parse_primary(&s.primary, 512).unwrap();
        assert_eq!(h.disk_id, gpt.disk_id);
        assert_eq!(h.last_usable_lba, size / 512 - 34);
        assert_eq!(e, gpt.entries);
        let backup = GptHeader::decode(&s.backup[s.backup.len() - 512..]).unwrap();
        assert_eq!(backup.my_lba, size / 512 - 1);
        assert_eq!(backup.entries_lba * 512, s.backup_offset);
    }

    #[test]
    fn corrupt_header_rejected() {
        let h = GptHeader {
            my_lba: 1,
            alternate_lba: 99,
            first_usable_lba: 34,
            last_usable_lba: 66,
            disk_id: Uuid::new_v4(),
            entries_lba: 2,
            entry_count: 128,
            entry_size: 128,
            entries_crc: 0,
        };
        let mut b = h.encode(4096);
        assert_eq!(GptHeader::decode(&b), Some(h));
        b[60] ^= 1;
        assert_eq!(GptHeader::decode(&b), None);
    }
}
