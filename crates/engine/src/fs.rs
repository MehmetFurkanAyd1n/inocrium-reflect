//! File system detection from a volume's boot sector.

/// File system name as Windows reports it, or `None` if unrecognized.
pub fn detect(boot: &[u8]) -> Option<&'static str> {
    if boot.len() < 512 {
        return None;
    }
    let oem = &boot[3..11];
    match oem {
        b"NTFS    " => Some("NTFS"),
        b"EXFAT   " => Some("exFAT"),
        b"-FVE-FS-" => Some("BitLocker"),
        _ if &boot[3..7] == b"ReFS" => Some("ReFS"),
        _ if &boot[82..87] == b"FAT32" => Some("FAT32"),
        _ if &boot[54..57] == b"FAT" => Some("FAT"),
        _ => None,
    }
}

/// Whether smart (used blocks only) capture is supported for a file system. The volume
/// bitmap gives cluster numbers relative to the start of the volume only for NTFS and ReFS;
/// FAT variants number clusters from the data area, so those fall back to forensic capture.
pub fn smart_capable(filesystem: &str) -> bool {
    filesystem.eq_ignore_ascii_case("NTFS") || filesystem.eq_ignore_ascii_case("ReFS")
}

/// Bytes covered by the NTFS cluster bitmap (`total sectors × bytes per sector`, rounded down
/// to whole clusters). Everything after this (the backup boot sector) is outside the bitmap
/// and must always be captured.
pub fn ntfs_volume_bytes(boot: &[u8]) -> Option<u64> {
    if detect(boot) != Some("NTFS") {
        return None;
    }
    let bps = u16::from_le_bytes([boot[0x0B], boot[0x0C]]) as u64;
    let spc_raw = boot[0x0D];
    // Values above 0x80 encode the cluster size as a negative power of two.
    let spc = if spc_raw > 0x80 { 1u64 << (256 - spc_raw as u32) } else { spc_raw as u64 };
    let total = u64::from_le_bytes(boot[0x28..0x30].try_into().unwrap());
    if bps == 0 || spc == 0 {
        return None;
    }
    let cluster = bps * spc;
    Some(total * bps / cluster * cluster)
}

#[cfg(test)]
pub(crate) fn fake_ntfs_boot(total_sectors: u64, sectors_per_cluster: u8) -> Vec<u8> {
    let mut b = vec![0u8; 512];
    b[0..3].copy_from_slice(&[0xEB, 0x52, 0x90]);
    b[3..11].copy_from_slice(b"NTFS    ");
    b[0x0B..0x0D].copy_from_slice(&512u16.to_le_bytes());
    b[0x0D] = sectors_per_cluster;
    b[0x28..0x30].copy_from_slice(&total_sectors.to_le_bytes());
    b[510] = 0x55;
    b[511] = 0xAA;
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects() {
        let b = fake_ntfs_boot(2047, 8);
        assert_eq!(detect(&b), Some("NTFS"));
        assert_eq!(ntfs_volume_bytes(&b), Some(2040 * 512));
        let mut f = vec![0u8; 512];
        f[82..90].copy_from_slice(b"FAT32   ");
        assert_eq!(detect(&f), Some("FAT32"));
        assert!(!smart_capable("FAT32") && smart_capable("ntfs"));
        assert_eq!(detect(&[0u8; 512]), None);
    }
}
