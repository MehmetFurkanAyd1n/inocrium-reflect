use crate::{disk::system_partition, handle::*, win, Error, PartitionKind, Result};
use serde::{Deserialize, Serialize};
use std::{
    mem::{offset_of, size_of},
    path::Path,
};
use windows::{
    core::PCWSTR,
    Win32::{Foundation::*, Storage::FileSystem::*, System::Ioctl::*},
};

/// A Windows volume and its mount paths.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VolumeInfo {
    /// GUID volume path including the trailing backslash.
    pub guid_path: String,
    /// Assigned drive letters without colons.
    pub drive_letters: Vec<char>,
    /// Drive roots and mounted directories.
    pub mount_points: Vec<String>,
    /// Filesystem name, or empty for an unknown/unavailable filesystem.
    pub filesystem: String,
    /// Filesystem label.
    pub label: String,
    /// Filesystem capacity in bytes, or zero when unavailable.
    pub total_bytes: u64,
    /// Free bytes available to the caller, or zero when unavailable.
    pub free_bytes: u64,
    /// Holds the running Windows directory.
    pub is_windows: bool,
    /// Holds an EFI or active boot partition.
    pub is_system: bool,
}
/// Cluster allocation map (LSB-first; 1 denotes an allocated cluster).
#[derive(Debug, Clone)]
pub struct VolumeBitmap {
    /// Bytes per allocation cluster.
    pub cluster_size: u32,
    /// Number of valid bits/clusters.
    pub total_clusters: u64,
    /// Byte offset of bitmap LCN zero: NTFS/ReFS zero, FAT data area, exFAT cluster heap.
    /// Callers must preserve all bytes before this offset as used.
    pub cluster0_offset: u64,
    /// Packed allocation bits, least-significant bit first.
    pub bits: Vec<u8>,
}
/// File allocation run; `u64::MAX` denotes sparse or virtual clusters.
#[derive(Debug, Clone, Copy)]
pub struct Extent {
    /// Starting virtual cluster number in the file.
    pub vcn: u64,
    /// Starting logical cluster number on the volume; `u64::MAX` for sparse runs.
    pub lcn: u64,
    /// Number of clusters in this run.
    pub clusters: u64,
}
/// Raw volume or VSS shadow-device handle. Closing the handle releases volume locks.
pub struct Volume {
    pub(crate) device: Device,
}
impl Volume {
    /// Open a volume device; a trailing backslash is accepted.
    pub fn open(path: &str, writable: bool) -> Result<Self> {
        let path = device_path(path);
        Ok(Self { device: Device::open(&path, writable)? })
    }
    /// Query volume length in bytes.
    pub fn length(&self) -> Result<u64> {
        let l: GET_LENGTH_INFORMATION = output(self.device.handle.0, IOCTL_DISK_GET_LENGTH_INFO)?;
        u64::try_from(l.Length).map_err(|_| Error::Other("negative volume length".into()))
    }
    /// Filesystem name, or an empty string for an unrecognized/raw filesystem.
    pub fn filesystem(&self) -> Result<String> {
        let mut b = [0u16; 64];
        match unsafe { GetVolumeInformationByHandleW(self.device.handle.0, None, None, None, None, Some(&mut b)) } {
            Ok(()) => Ok(string(&b)),
            Err(e)
                if [ERROR_UNRECOGNIZED_VOLUME, ERROR_NOT_SUPPORTED, ERROR_INVALID_FUNCTION]
                    .iter()
                    .any(|x| x.to_hresult() == e.code()) =>
            {
                Ok(String::new())
            }
            Err(e) => Err(win("volume filesystem", e)),
        }
    }
    /// Read the full allocation map using paged FSCTL requests and boot-sector geometry.
    pub fn bitmap(&self) -> Result<VolumeBitmap> {
        let mut boot = vec![0; self.device.sector as usize];
        self.device.read(0, &mut boot)?;
        let (cluster_size, cluster0_offset) = boot_geometry(&boot, &self.filesystem()?)?;
        let mut next = 0u64;
        let mut total = None;
        let mut bits = Vec::new();
        loop {
            let mut b = Buffer::new(1024 * 1024 + 16, 8)?;
            let (r, n) =
                ioctl(self.device.handle.0, FSCTL_GET_VOLUME_BITMAP, &(next as i64).to_ne_bytes(), b.bytes_mut());
            let more = r.as_ref().is_err_and(|e| e.code() == ERROR_MORE_DATA.to_hresult());
            if !more {
                r.map_err(|e| win("volume allocation bitmap", e))?;
            }
            if n < 16 || n > b.bytes().len() {
                return Err(Error::Other("truncated volume bitmap".into()));
            }
            let start: i64 = b.get(0)?;
            let remaining: i64 = b.get(8)?;
            if start < 0 || remaining < 0 {
                return Err(Error::Other("invalid bitmap range".into()));
            }
            let start = start as u64;
            let remaining = remaining as u64;
            let end = start.checked_add(remaining).ok_or_else(|| Error::Other("bitmap range overflow".into()))?;
            if let Some(t) = total {
                if t != end {
                    return Err(Error::Other("volume bitmap size changed during query".into()));
                }
            } else {
                total = Some(end);
                let bytes = usize::try_from(end.div_ceil(8)).map_err(|_| Error::Other("bitmap too large".into()))?;
                bits.try_reserve_exact(bytes).map_err(|e| Error::Other(e.to_string()))?;
                bits.resize(bytes, 0);
            }
            let count = remaining.min((n as u64 - 16) * 8);
            if start > next || start + count <= next && next < end {
                return Err(Error::Other("bitmap query made no progress".into()));
            }
            copy_bitmap_page(&mut bits, start, count, &b.bytes()[16..n])?;
            next = start + count;
            if next >= end {
                break;
            }
            if !more {
                return Err(Error::Other("incomplete volume bitmap".into()));
            }
        }
        Ok(VolumeBitmap { cluster_size, total_clusters: total.unwrap_or(0), cluster0_offset, bits })
    }
    /// Exclusively lock the volume until unlocked or closed.
    pub fn lock(&self) -> Result<()> {
        control(self.device.handle.0, FSCTL_LOCK_VOLUME)
    }
    /// Release an exclusive volume lock.
    pub fn unlock(&self) -> Result<()> {
        control(self.device.handle.0, FSCTL_UNLOCK_VOLUME)
    }
    /// Dismount the filesystem; normally used after locking it.
    pub fn dismount(&self) -> Result<()> {
        control(self.device.handle.0, FSCTL_DISMOUNT_VOLUME)
    }
    /// Extend a filesystem to the specified total sector count.
    pub fn extend(&self, new_total_sectors: u64) -> Result<()> {
        let sectors = i64::try_from(new_total_sectors).map_err(|_| Error::Other("sector count overflow".into()))?;
        input(self.device.handle.0, FSCTL_EXTEND_VOLUME, &sectors)
    }
}
block_device!(Volume);
pub(crate) fn device_path(path: &str) -> String {
    let path = path.trim_end_matches('\\');
    if path.len() == 2 && path.as_bytes()[1] == b':' {
        format!(r"\\.\{path}")
    } else {
        path.to_owned()
    }
}
fn le16(b: &[u8], o: usize) -> u64 {
    u16::from_le_bytes(b[o..o + 2].try_into().unwrap()) as u64
}
fn copy_bitmap_page(bits: &mut [u8], start: u64, count: u64, page: &[u8]) -> Result<()> {
    // Windows rounds StartingLcn down to a byte boundary. Preserve that returned base.
    if start & 7 != 0 {
        return Err(Error::Other("bitmap start is not byte aligned".into()));
    }
    let base = usize::try_from(start / 8).map_err(|_| Error::Other("bitmap index overflow".into()))?;
    let bytes = usize::try_from(count.div_ceil(8)).map_err(|_| Error::Other("bitmap page overflow".into()))?;
    let end = base.checked_add(bytes).ok_or_else(|| Error::Other("bitmap page overflow".into()))?;
    let target = bits.get_mut(base..end).ok_or_else(|| Error::Other("bitmap page outside allocation".into()))?;
    let source = page.get(..bytes).ok_or_else(|| Error::Other("truncated bitmap page".into()))?;
    target.copy_from_slice(source);
    if count & 7 != 0 && bytes != 0 {
        target[bytes - 1] &= (1u8 << (count & 7)) - 1;
    }
    Ok(())
}
fn le32(b: &[u8], o: usize) -> u64 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap()) as u64
}
fn boot_geometry(b: &[u8], fs: &str) -> Result<(u32, u64)> {
    if b.len() < 512 || b[510..512] != [0x55, 0xaa] {
        return Err(Error::Other("invalid filesystem boot sector".into()));
    }
    let (bytes, sectors, offset) = match fs.to_ascii_uppercase().as_str() {
        "EXFAT" => {
            if &b[3..11] != b"EXFAT   " || !(9..=12).contains(&b[108]) || b[109] > 25 - b[108] {
                return Err(Error::Other("invalid exFAT geometry".into()));
            }
            let bytes = 1u64 << b[108];
            (bytes, 1u64 << b[109], le32(b, 88) * bytes)
        }
        "FAT" | "FAT12" | "FAT16" | "FAT32" => {
            let bytes = le16(b, 11);
            let sectors = b[13] as u64;
            let fat_size = if le16(b, 22) == 0 { le32(b, 36) } else { le16(b, 22) };
            let root_sectors = (le16(b, 17) * 32).div_ceil(bytes.max(1));
            let offset = (le16(b, 14) + b[16] as u64 * fat_size + root_sectors) * bytes;
            if fat_size == 0 || b[16] == 0 || le16(b, 14) == 0 {
                return Err(Error::Other("invalid FAT data area".into()));
            }
            (bytes, sectors, offset)
        }
        "NTFS" => (le16(b, 11), b[13] as u64, 0),
        // ReFS has a BPB with 32-bit sector/cluster fields at offsets 32 and 36.
        "REFS" => (le32(b, 32), le32(b, 36), 0),
        _ => return Err(Error::Other(format!("unsupported bitmap filesystem {fs}"))),
    };
    if !(512..=65536).contains(&bytes) || !bytes.is_power_of_two() || sectors == 0 || !sectors.is_power_of_two() {
        return Err(Error::Other("invalid cluster geometry".into()));
    }
    let cluster = bytes
        .checked_mul(sectors)
        .and_then(|n| u32::try_from(n).ok())
        .ok_or_else(|| Error::Other("cluster size overflow".into()))?;
    Ok((cluster, offset))
}

struct VolumeSearch(HANDLE);
impl Drop for VolumeSearch {
    fn drop(&mut self) {
        unsafe {
            let _ = FindVolumeClose(self.0);
        }
    }
}
pub(crate) fn volume_names() -> Result<Vec<String>> {
    let mut b = [0u16; 1024];
    let h = unsafe { FindFirstVolumeW(&mut b) }.map_err(|e| win("enumerate volumes", e))?;
    let h = VolumeSearch(h);
    let mut names = Vec::new();
    loop {
        names.push(string(&b));
        match unsafe { FindNextVolumeW(h.0, &mut b) } {
            Ok(()) => {}
            Err(e) if e.code() == ERROR_NO_MORE_FILES.to_hresult() => break,
            Err(e) => return Err(win("enumerate volumes", e)),
        }
    }
    Ok(names)
}
fn mount_points(path: &str) -> Result<Vec<String>> {
    let p = wide(path);
    let mut n = 0;
    let mut b = vec![0u16; 1024];
    loop {
        match unsafe { GetVolumePathNamesForVolumeNameW(PCWSTR(p.as_ptr()), Some(&mut b), &mut n) } {
            Ok(()) => {
                return Ok(b[..(n as usize).min(b.len())]
                    .split(|&c| c == 0)
                    .filter(|s| !s.is_empty())
                    .map(string)
                    .collect())
            }
            Err(e) if e.code() == ERROR_MORE_DATA.to_hresult() && n as usize > b.len() => b.resize(n as usize, 0),
            Err(e) => return Err(win("volume mount paths", e)),
        }
    }
}
fn windows_volume() -> Option<String> {
    let p = wide(crate::windows_dir());
    let mut root = vec![0u16; 32768];
    unsafe { GetVolumePathNameW(PCWSTR(p.as_ptr()), &mut root) }.ok()?;
    let mut guid = [0u16; 1024];
    unsafe { GetVolumeNameForVolumeMountPointW(PCWSTR(root.as_ptr()), &mut guid) }.ok()?;
    Some(string(&guid))
}
/// Enumerate GUID volumes without requiring raw read access.
pub fn list_volumes() -> Result<Vec<VolumeInfo>> {
    let windows = windows_volume();
    let mut volumes = Vec::new();
    for guid_path in volume_names()? {
        let mount_points = mount_points(&guid_path)?;
        let drive_letters = mount_points
            .iter()
            .filter_map(|p| if p.len() == 3 && p.as_bytes()[1] == b':' { p.chars().next() } else { None })
            .collect();
        let p = wide(&guid_path);
        let mut fs = [0u16; 64];
        let mut label = [0u16; 256];
        let r = unsafe { GetVolumeInformationW(PCWSTR(p.as_ptr()), Some(&mut label), None, None, None, Some(&mut fs)) };
        if let Err(e) = r {
            if ![ERROR_UNRECOGNIZED_VOLUME, ERROR_NOT_READY, ERROR_ACCESS_DENIED, ERROR_INVALID_FUNCTION]
                .iter()
                .any(|x| x.to_hresult() == e.code())
            {
                return Err(win("volume metadata", e));
            }
        }
        let mut total_bytes = 0;
        let mut free_bytes = 0;
        unsafe {
            let _ = GetDiskFreeSpaceExW(PCWSTR(p.as_ptr()), Some(&mut free_bytes), Some(&mut total_bytes), None);
        }
        let is_system = open(&device_path(&guid_path), 0, FILE_FLAGS_AND_ATTRIBUTES(0))
            .ok()
            .and_then(|h| output::<PARTITION_INFORMATION_EX>(h.0, IOCTL_DISK_GET_PARTITION_INFO_EX).ok())
            .is_some_and(|p| unsafe {
                if p.PartitionStyle == PARTITION_STYLE_MBR {
                    p.Anonymous.Mbr.BootIndicator.as_bool()
                } else if p.PartitionStyle == PARTITION_STYLE_GPT {
                    let g = p.Anonymous.Gpt;
                    system_partition(&PartitionKind::Gpt {
                        type_id: crate::disk::uuid(g.PartitionType),
                        id: crate::disk::uuid(g.PartitionId),
                        name: string(&g.Name),
                        attributes: g.Attributes.0,
                    })
                } else {
                    false
                }
            });
        let is_windows = windows.as_ref().is_some_and(|w| w.eq_ignore_ascii_case(&guid_path));
        volumes.push(VolumeInfo {
            guid_path,
            drive_letters,
            mount_points,
            filesystem: string(&fs),
            label: string(&label),
            total_bytes,
            free_bytes,
            is_windows,
            is_system,
        });
    }
    Ok(volumes)
}
/// Physical disk ranges occupied by a volume.
pub fn volume_disk_extents(volume_guid_path: &str) -> Result<Vec<(u32, u64, u64)>> {
    let h = open(&device_path(volume_guid_path), 0, FILE_FLAGS_AND_ATTRIBUTES(0))?;
    let b = variable(h.0, IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS, &[])?;
    let count: u32 = b.get(0)?;
    let mut result = Vec::new();
    for i in 0..count as usize {
        let e: DISK_EXTENT = b.get(offset_of!(VOLUME_DISK_EXTENTS, Extents) + i * size_of::<DISK_EXTENT>())?;
        if e.StartingOffset < 0 || e.ExtentLength < 0 {
            return Err(Error::Other("negative disk extent".into()));
        }
        result.push((e.DiskNumber, e.StartingOffset as u64, e.ExtentLength as u64));
    }
    Ok(result)
}
pub(crate) fn mapped_extents(path: &str) -> Result<Option<Vec<(u32, u64, u64)>>> {
    match volume_disk_extents(path) {
        Ok(e) => Ok(Some(e)),
        Err(Error::Win32 { source, .. })
            if [
                ERROR_INVALID_FUNCTION,
                ERROR_NOT_SUPPORTED,
                ERROR_NOT_READY,
                ERROR_NO_MEDIA_IN_DRIVE,
                ERROR_FILE_NOT_FOUND,
                ERROR_PATH_NOT_FOUND,
            ]
            .iter()
            .any(|e| e.to_hresult() == source.code()) =>
        {
            Ok(None)
        }
        Err(e) => Err(e),
    }
}
/// Find a GUID volume whose extent starts at the supplied partition offset.
pub fn volume_for_partition(disk: u32, offset: u64) -> Result<Option<String>> {
    for v in volume_names()? {
        if mapped_extents(&v)?.is_some_and(|e| e.iter().any(|x| x.0 == disk && x.1 == offset)) {
            return Ok(Some(v));
        }
    }
    Ok(None)
}
/// Query allocated file extents, including sparse runs; resident files return an empty vector.
pub fn file_extents(path: &Path) -> Result<Vec<Extent>> {
    let p = wide(path);
    let h = unsafe {
        CreateFileW(
            PCWSTR(p.as_ptr()),
            FILE_READ_ATTRIBUTES.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            None,
        )
    }
    .map(Handle)
    .map_err(|e| win("open file for retrieval pointers", e))?;
    let mut result = Vec::new();
    let mut next = 0i64;
    loop {
        let mut b = Buffer::new(65536, 8)?;
        let (r, n) = ioctl(h.0, FSCTL_GET_RETRIEVAL_POINTERS, &next.to_ne_bytes(), b.bytes_mut());
        let more = r.as_ref().is_err_and(|e| e.code() == ERROR_MORE_DATA.to_hresult());
        if r.as_ref().is_err_and(|e| e.code() == ERROR_HANDLE_EOF.to_hresult()) {
            break;
        }
        if !more {
            r.map_err(|e| win("file retrieval pointers", e))?;
        }
        if n < 16 || n > b.bytes().len() {
            return Err(Error::Other("truncated retrieval pointers".into()));
        }
        let count: u32 = b.get(0)?;
        let mut vcn: i64 = b.get(8)?;
        if vcn < 0 || vcn > next || count as usize > (n - 16) / 16 {
            return Err(Error::Other("invalid retrieval pointers".into()));
        }
        for i in 0..count as usize {
            let e: RETRIEVAL_POINTERS_BUFFER_0 = b.get(16 + i * 16)?;
            if e.NextVcn <= vcn || e.Lcn < -1 {
                return Err(Error::Other("invalid file extent".into()));
            }
            result.push(Extent {
                vcn: vcn as u64,
                lcn: if e.Lcn == -1 { u64::MAX } else { e.Lcn as u64 },
                clusters: (e.NextVcn - vcn) as u64,
            });
            vcn = e.NextVcn;
        }
        if !more {
            break;
        }
        if vcn <= next {
            return Err(Error::Other("retrieval pointers made no progress".into()));
        }
        next = vcn;
    }
    Ok(result)
}
/// Mount a GUID volume at a drive root or an empty directory.
pub fn set_mount_point(mount: &str, volume_guid_path: &str) -> Result<()> {
    let m = wide(format!("{}\\", mount.trim_end_matches('\\')));
    let v = wide(format!("{}\\", volume_guid_path.trim_end_matches('\\')));
    unsafe { SetVolumeMountPointW(PCWSTR(m.as_ptr()), PCWSTR(v.as_ptr())) }.map_err(|e| win("set mount point", e))
}
/// Remove a drive-root or directory mount point.
pub fn remove_mount_point(mount: &str) -> Result<()> {
    let m = wide(format!("{}\\", mount.trim_end_matches('\\')));
    unsafe { DeleteVolumeMountPointW(PCWSTR(m.as_ptr())) }.map_err(|e| win("remove mount point", e))
}
/// First currently unused drive letter from D through Z.
pub fn free_drive_letter() -> Option<char> {
    let mask = unsafe { GetLogicalDrives() };
    if mask == 0 {
        return None;
    }
    ('D'..='Z').find(|&c| mask & (1 << (c as u32 - 'A' as u32)) == 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn boot() -> Vec<u8> {
        let mut b = vec![0; 512];
        b[510..].copy_from_slice(&[0x55, 0xaa]);
        b[11..13].copy_from_slice(&512u16.to_le_bytes());
        b[13] = 8;
        b
    }
    #[test]
    fn boot_sector_geometry() {
        let mut b = boot();
        assert_eq!(boot_geometry(&b, "NTFS").unwrap(), (4096, 0));
        b[14..16].copy_from_slice(&1u16.to_le_bytes());
        b[16] = 2;
        b[17..19].copy_from_slice(&512u16.to_le_bytes());
        b[22..24].copy_from_slice(&128u16.to_le_bytes());
        assert_eq!(boot_geometry(&b, "FAT16").unwrap(), (4096, (1 + 256 + 32) * 512));
        b[17..19].fill(0);
        b[22..24].fill(0);
        b[14..16].copy_from_slice(&32u16.to_le_bytes());
        b[36..40].copy_from_slice(&100u32.to_le_bytes());
        assert_eq!(boot_geometry(&b, "FAT32").unwrap(), (4096, 232 * 512));
        b[3..11].copy_from_slice(b"EXFAT   ");
        b[108] = 9;
        b[109] = 7;
        b[88..92].copy_from_slice(&2048u32.to_le_bytes());
        assert_eq!(boot_geometry(&b, "exFAT").unwrap(), (65536, 1048576));
        b[32..36].copy_from_slice(&4096u32.to_le_bytes());
        b[36..40].copy_from_slice(&16u32.to_le_bytes());
        assert_eq!(boot_geometry(&b, "ReFS").unwrap(), (65536, 0));
        b[510] = 0;
        assert!(boot_geometry(&b, "NTFS").is_err());
    }
    #[test]
    fn enumerate_volumes() {
        let v = list_volumes().unwrap();
        assert!(v.iter().any(|v| v.is_windows));
        assert!(v.iter().all(|v| v.guid_path.ends_with('\\')));
    }
    #[test]
    fn resident_file_extents() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("resident");
        std::fs::write(&p, b"small").unwrap();
        assert!(file_extents(&p).unwrap().is_empty());
    }
    #[test]
    fn bitmap_pages_and_partial_byte() {
        let mut bits = [0; 4];
        copy_bitmap_page(&mut bits, 0, 16, &[0xaa, 0xff]).unwrap();
        copy_bitmap_page(&mut bits, 8, 19, &[0x33, 0x80, 0xff]).unwrap();
        assert_eq!(bits, [0xaa, 0x33, 0x80, 7]);
        assert!(copy_bitmap_page(&mut bits, 1, 8, &[0xff]).is_err());
        assert!(copy_bitmap_page(&mut bits, 24, 16, &[0xff; 2]).is_err());
    }
}
