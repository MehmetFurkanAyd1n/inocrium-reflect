//! Master boot record parsing and construction.

/// Offset of the 32-bit disk signature.
pub const SIGNATURE_OFFSET: usize = 440;
/// Offset of the first of the four primary partition entries.
pub const TABLE_OFFSET: usize = 446;
pub const ENTRY_SIZE: usize = 16;
/// Partition type of the single entry of a protective MBR.
pub const GPT_PROTECTIVE: u8 = 0xEE;

/// One primary partition table entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MbrEntry {
    pub bootable: bool,
    pub partition_type: u8,
    pub first_lba: u32,
    pub sectors: u32,
}

impl MbrEntry {
    fn encode(&self) -> [u8; ENTRY_SIZE] {
        let mut b = [0u8; ENTRY_SIZE];
        b[0] = if self.bootable { 0x80 } else { 0 };
        // CHS fields use the "LBA only" marker that Windows writes for large disks.
        b[1..4].copy_from_slice(&[0xFE, 0xFF, 0xFF]);
        b[4] = self.partition_type;
        b[5..8].copy_from_slice(&[0xFE, 0xFF, 0xFF]);
        b[8..12].copy_from_slice(&self.first_lba.to_le_bytes());
        b[12..16].copy_from_slice(&self.sectors.to_le_bytes());
        b
    }

    fn decode(b: &[u8]) -> Option<Self> {
        (b[4] != 0).then(|| Self {
            bootable: b[0] & 0x80 != 0,
            partition_type: b[4],
            first_lba: u32::from_le_bytes(b[8..12].try_into().unwrap()),
            sectors: u32::from_le_bytes(b[12..16].try_into().unwrap()),
        })
    }
}

/// Extended partition container types (CHS, LBA and Linux).
pub fn is_extended(partition_type: u8) -> bool {
    matches!(partition_type, 0x05 | 0x0F | 0x85)
}

pub fn has_boot_signature(sector: &[u8]) -> bool {
    sector.len() >= 512 && sector[510] == 0x55 && sector[511] == 0xAA
}

pub fn signature(sector: &[u8]) -> u32 {
    u32::from_le_bytes(sector[SIGNATURE_OFFSET..SIGNATURE_OFFSET + 4].try_into().unwrap())
}

pub fn set_signature(sector: &mut [u8], signature: u32) {
    sector[SIGNATURE_OFFSET..SIGNATURE_OFFSET + 4].copy_from_slice(&signature.to_le_bytes());
}

/// The four primary slots; `None` for empty ones.
pub fn entries(sector: &[u8]) -> [Option<MbrEntry>; 4] {
    std::array::from_fn(|i| MbrEntry::decode(&sector[TABLE_OFFSET + i * ENTRY_SIZE..][..ENTRY_SIZE]))
}

/// Writes (or clears, with `None`) primary slot `index`.
pub fn set_entry(sector: &mut [u8], index: usize, entry: Option<&MbrEntry>) {
    let slot = &mut sector[TABLE_OFFSET + index * ENTRY_SIZE..][..ENTRY_SIZE];
    match entry {
        Some(e) => slot.copy_from_slice(&e.encode()),
        None => slot.fill(0),
    }
}

/// Builds an MBR sector. `boot_code` (up to 440 bytes) is copied in front of the signature.
pub fn build(signature: u32, entries: &[MbrEntry], boot_code: Option<&[u8]>) -> [u8; 512] {
    let mut b = [0u8; 512];
    if let Some(code) = boot_code {
        let n = code.len().min(SIGNATURE_OFFSET);
        b[..n].copy_from_slice(&code[..n]);
    }
    set_signature(&mut b, signature);
    for (i, e) in entries.iter().take(4).enumerate() {
        set_entry(&mut b, i, Some(e));
    }
    b[510] = 0x55;
    b[511] = 0xAA;
    b
}

/// The protective MBR placed in LBA 0 of a GPT disk with `total_sectors` sectors.
pub fn protective_mbr(total_sectors: u64) -> [u8; 512] {
    let mut b = build(
        0,
        &[MbrEntry {
            bootable: false,
            partition_type: GPT_PROTECTIVE,
            first_lba: 1,
            sectors: (total_sectors - 1).min(u32::MAX as u64) as u32,
        }],
        None,
    );
    // UEFI specifies a starting CHS of 0/0/2 for the protective entry.
    b[TABLE_OFFSET + 1..TABLE_OFFSET + 4].copy_from_slice(&[0x00, 0x02, 0x00]);
    b
}

/// A random, non-zero disk signature.
pub fn random_signature() -> u32 {
    loop {
        let mut b = [0u8; 4];
        getrandom::getrandom(&mut b).expect("system RNG unavailable");
        let s = u32::from_le_bytes(b);
        if s != 0 {
            return s;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let e = [
            MbrEntry { bootable: true, partition_type: 0x07, first_lba: 2048, sectors: 1_026_048 },
            MbrEntry { bootable: false, partition_type: 0x0F, first_lba: 1_028_096, sectors: 4096 },
        ];
        let mut s = build(0xDEAD_BEEF, &e, Some(&[0x33; 500]));
        assert!(has_boot_signature(&s));
        assert_eq!(signature(&s), 0xDEAD_BEEF);
        assert_eq!(s[439], 0x33);
        assert_eq!(entries(&s), [Some(e[0]), Some(e[1]), None, None]);
        assert!(is_extended(e[1].partition_type));
        set_entry(&mut s, 0, None);
        set_signature(&mut s, 7);
        assert_eq!(entries(&s), [None, Some(e[1]), None, None]);
        assert_eq!(signature(&s), 7);
    }

    #[test]
    fn protective() {
        let s = protective_mbr(1 << 40);
        let [Some(e), None, None, None] = entries(&s) else { panic!() };
        assert_eq!((e.partition_type, e.first_lba, e.sectors), (GPT_PROTECTIVE, 1, u32::MAX));
        assert_eq!(entries(&protective_mbr(1000))[0].unwrap().sectors, 999);
        assert_ne!(random_signature(), 0);
    }
}
