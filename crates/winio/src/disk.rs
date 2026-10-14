use crate::{handle::*, volume::*, Error, Result};
use serde::{Deserialize, Serialize};
use std::{
    mem::{offset_of, size_of},
    time::{Duration, Instant},
};
use uuid::Uuid;
use windows::{
    core::GUID,
    Win32::{Foundation::*, Storage::FileSystem::*, System::Ioctl::*},
};

/// Physical disk metadata and its partition table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiskInfo {
    /// Number in `PhysicalDriveN`.
    pub number: u32,
    /// Device length in bytes.
    pub size: u64,
    /// Logical sector size.
    pub sector_size: u32,
    /// Physical sector size used for buffer alignment.
    pub physical_sector_size: u32,
    /// Reported product/model name.
    pub model: String,
    /// Reported serial number, when available.
    pub serial: String,
    /// Storage bus name, such as `NVMe`, `SATA` or `iSCSI`.
    pub bus_type: String,
    /// Whether the device reports removable media.
    pub removable: bool,
    /// Whether this is a virtual, file-backed or iSCSI disk.
    pub is_virtual: bool,
    /// Current offline attribute.
    pub offline: bool,
    /// Current read-only attribute.
    pub read_only: bool,
    /// Holds Windows, a boot/system partition or an active pagefile.
    pub is_system: bool,
    /// Current partition table.
    pub layout: DiskLayout,
}
/// Partition table identity and entries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiskLayout {
    /// Partition table format and identity.
    pub style: LayoutStyle,
    /// Data partitions, excluding MBR extended containers and unused slots.
    pub partitions: Vec<PartitionInfo>,
}
/// On-disk partition table format.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum LayoutStyle {
    /// Master boot record and its disk signature.
    Mbr { signature: u32 },
    /// GUID partition table and its usable byte range.
    Gpt { disk_id: Uuid, starting_usable_offset: u64, usable_length: u64, max_partition_count: u32 },
    /// No recognized partition table.
    Raw,
}
/// Byte-addressed partition and optional mounted volume.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PartitionInfo {
    /// Windows partition number, starting at one.
    pub number: u32,
    /// Starting byte offset on the disk.
    pub offset: u64,
    /// Length in bytes.
    pub length: u64,
    /// Partition type and identity.
    pub kind: PartitionKind,
    /// Associated filesystem volume, when one exists.
    pub volume: Option<VolumeInfo>,
}
/// MBR type/boot flag or GPT type/identity/attributes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PartitionKind {
    /// MBR type, active flag and primary/logical status.
    Mbr { partition_type: u8, bootable: bool, logical: bool },
    /// GPT type GUID, unique GUID, UTF-16 name and attribute flags.
    Gpt { type_id: Uuid, id: Uuid, name: String, attributes: u64 },
}
pub(crate) fn guid(id: Uuid) -> GUID {
    GUID::from_u128(id.as_u128())
}
pub(crate) fn uuid(id: GUID) -> Uuid {
    Uuid::from_u128(id.to_u128())
}
pub(crate) fn system_partition(p: &PartitionKind) -> bool {
    match p {
        PartitionKind::Mbr { bootable, .. } => *bootable,
        PartitionKind::Gpt { type_id, attributes, .. } => {
            *type_id == Uuid::from_u128(0xc12a7328_f81f_11d2_ba4b_00a0c93ec93b)
                || attributes & GPT_ATTRIBUTE_LEGACY_BIOS_BOOTABLE != 0
        }
    }
}
fn parse_layout(b: &Buffer) -> Result<DiskLayout> {
    let h = DRIVE_LAYOUT_INFORMATION_EX {
        PartitionStyle: b.get(0)?,
        PartitionCount: b.get(4)?,
        Anonymous: b.get(offset_of!(DRIVE_LAYOUT_INFORMATION_EX, Anonymous))?,
        ..Default::default()
    };
    let style = unsafe {
        match h.PartitionStyle as i32 {
            0 => LayoutStyle::Mbr { signature: h.Anonymous.Mbr.Signature },
            1 => {
                let g = h.Anonymous.Gpt;
                LayoutStyle::Gpt {
                    disk_id: uuid(g.DiskId),
                    starting_usable_offset: g.StartingUsableOffset.max(0) as u64,
                    usable_length: g.UsableLength.max(0) as u64,
                    max_partition_count: g.MaxPartitionCount,
                }
            }
            _ => LayoutStyle::Raw,
        }
    };
    let mut partitions = Vec::new();
    for i in 0..h.PartitionCount as usize {
        let p: PARTITION_INFORMATION_EX =
            b.get(offset_of!(DRIVE_LAYOUT_INFORMATION_EX, PartitionEntry) + i * size_of::<PARTITION_INFORMATION_EX>())?;
        if p.PartitionNumber == 0 || p.PartitionLength <= 0 {
            continue;
        }
        if p.StartingOffset < 0 {
            return Err(Error::Other("negative partition offset".into()));
        }
        let kind = unsafe {
            if p.PartitionStyle == PARTITION_STYLE_MBR {
                let m = p.Anonymous.Mbr;
                if matches!(m.PartitionType, 0 | 0x05 | 0x0f | 0x85) {
                    continue;
                }
                PartitionKind::Mbr {
                    partition_type: m.PartitionType,
                    bootable: m.BootIndicator.as_bool(),
                    logical: i >= 4,
                }
            } else if p.PartitionStyle == PARTITION_STYLE_GPT {
                let g = p.Anonymous.Gpt;
                PartitionKind::Gpt {
                    type_id: uuid(g.PartitionType),
                    id: uuid(g.PartitionId),
                    name: string(&g.Name),
                    attributes: g.Attributes.0,
                }
            } else {
                continue;
            }
        };
        partitions.push(PartitionInfo {
            number: p.PartitionNumber,
            offset: p.StartingOffset as u64,
            length: p.PartitionLength as u64,
            kind,
            volume: None,
        });
    }
    Ok(DiskLayout { style, partitions })
}
fn build_layout(layout: &DiskLayout, sector: u32, size: u64) -> Result<(Buffer, CREATE_DISK)> {
    let mut h = DRIVE_LAYOUT_INFORMATION_EX::default();
    let mut create = CREATE_DISK::default();
    let count = match layout.style {
        LayoutStyle::Mbr { signature } => {
            if layout.partitions.len() > 4
                || layout.partitions.iter().any(|p| matches!(p.kind, PartitionKind::Mbr { logical: true, .. }))
            {
                return Err(Error::Other(
                    "writing logical MBR partitions is unsupported; only four primary slots are supported".into(),
                ));
            }
            h.PartitionStyle = PARTITION_STYLE_MBR.0 as u32;
            h.Anonymous.Mbr = DRIVE_LAYOUT_INFORMATION_MBR { Signature: signature, ..Default::default() };
            create.PartitionStyle = PARTITION_STYLE_MBR;
            create.Anonymous.Mbr = CREATE_DISK_MBR { Signature: signature };
            4
        }
        LayoutStyle::Gpt { disk_id, starting_usable_offset, usable_length, max_partition_count } => {
            if max_partition_count == 0
                || layout.partitions.len() > max_partition_count as usize
                || starting_usable_offset.checked_add(usable_length).is_none_or(|end| end > size)
            {
                return Err(Error::Other("invalid GPT usable range/count".into()));
            }
            h.PartitionStyle = PARTITION_STYLE_GPT.0 as u32;
            h.Anonymous.Gpt = DRIVE_LAYOUT_INFORMATION_GPT {
                DiskId: guid(disk_id),
                StartingUsableOffset: starting_usable_offset
                    .try_into()
                    .map_err(|_| Error::Other("GPT offset overflow".into()))?,
                UsableLength: usable_length.try_into().map_err(|_| Error::Other("GPT length overflow".into()))?,
                MaxPartitionCount: max_partition_count,
            };
            create.PartitionStyle = PARTITION_STYLE_GPT;
            create.Anonymous.Gpt = CREATE_DISK_GPT { DiskId: guid(disk_id), MaxPartitionCount: max_partition_count };
            layout.partitions.len()
        }
        LayoutStyle::Raw => return Err(Error::Other("use clear_layout to remove a partition table".into())),
    };
    h.PartitionCount = count as u32;
    let mut b = Buffer::new(
        size_of::<DRIVE_LAYOUT_INFORMATION_EX>().max(
            offset_of!(DRIVE_LAYOUT_INFORMATION_EX, PartitionEntry) + count * size_of::<PARTITION_INFORMATION_EX>(),
        ),
        8,
    )?;
    b.put(0, h)?;
    // Empty primary slots must also be rewritten to remove entries from the previous MBR.
    if matches!(layout.style, LayoutStyle::Mbr { .. }) {
        for slot in 0..4 {
            b.put(
                offset_of!(DRIVE_LAYOUT_INFORMATION_EX, PartitionEntry) + slot * size_of::<PARTITION_INFORMATION_EX>(),
                PARTITION_INFORMATION_EX {
                    PartitionStyle: PARTITION_STYLE_MBR,
                    RewritePartition: BOOLEAN(1),
                    ..Default::default()
                },
            )?;
        }
    }
    let mut ranges = Vec::new();
    let mut numbers = std::collections::HashSet::new();
    for (i, p) in layout.partitions.iter().enumerate() {
        let end = p.offset.checked_add(p.length).ok_or_else(|| Error::Other("partition range overflow".into()))?;
        if p.number == 0
            || !numbers.insert(p.number)
            || p.offset == 0
            || p.length == 0
            || end > size
            || end > i64::MAX as u64
            || p.offset % sector as u64 != 0
            || p.length % sector as u64 != 0
            || ranges.iter().any(|&(a, z)| p.offset < z && end > a)
        {
            return Err(Error::Other("invalid or overlapping partition range/number".into()));
        }
        ranges.push((p.offset, end));
        if let LayoutStyle::Gpt { starting_usable_offset, usable_length, .. } = layout.style {
            if p.offset < starting_usable_offset || end > starting_usable_offset + usable_length {
                return Err(Error::Other("partition outside GPT usable range".into()));
            }
        }
        let mut entry = PARTITION_INFORMATION_EX {
            PartitionStyle: create.PartitionStyle,
            StartingOffset: p.offset as i64,
            PartitionLength: p.length as i64,
            PartitionNumber: p.number,
            RewritePartition: BOOLEAN(1),
            ..Default::default()
        };
        match (&layout.style, &p.kind) {
            (LayoutStyle::Mbr { .. }, PartitionKind::Mbr { partition_type, bootable, logical: false })
                if !matches!(partition_type, 0 | 5 | 15 | 0x85) =>
            {
                if p.number > 4 {
                    return Err(Error::Other("MBR primary number must be 1..4".into()));
                }
                entry.Anonymous.Mbr = PARTITION_INFORMATION_MBR {
                    PartitionType: *partition_type,
                    BootIndicator: BOOLEAN(u8::from(*bootable)),
                    RecognizedPartition: BOOLEAN(1),
                    HiddenSectors: u32::try_from(p.offset / sector as u64)
                        .map_err(|_| Error::Other("MBR offset overflow".into()))?,
                    ..Default::default()
                };
            }
            (LayoutStyle::Gpt { .. }, PartitionKind::Gpt { type_id, id, name, attributes }) => {
                let mut g = PARTITION_INFORMATION_GPT {
                    PartitionType: guid(*type_id),
                    PartitionId: guid(*id),
                    Attributes: GPT_ATTRIBUTES(*attributes),
                    ..Default::default()
                };
                let name: Vec<u16> = name.encode_utf16().collect();
                if name.len() > 36 {
                    return Err(Error::Other("GPT name exceeds 36 UTF-16 code units".into()));
                }
                g.Name[..name.len()].copy_from_slice(&name);
                entry.Anonymous.Gpt = g;
            }
            _ => return Err(Error::Other("partition kind does not match table style".into())),
        }
        let slot = if matches!(layout.style, LayoutStyle::Mbr { .. }) { p.number as usize - 1 } else { i };
        b.put(
            offset_of!(DRIVE_LAYOUT_INFORMATION_EX, PartitionEntry) + slot * size_of::<PARTITION_INFORMATION_EX>(),
            entry,
        )?;
    }
    Ok((b, create))
}
fn descriptor(h: HANDLE) -> Result<Buffer> {
    let q = STORAGE_PROPERTY_QUERY {
        PropertyId: StorageDeviceProperty,
        QueryType: PropertyStandardQuery,
        ..Default::default()
    };
    let mut b = Buffer::new(size_of::<STORAGE_PROPERTY_QUERY>(), 8)?;
    b.put(0, q)?;
    variable(h, IOCTL_STORAGE_QUERY_PROPERTY, b.bytes())
}
fn ansi(b: &[u8], offset: u32) -> String {
    if offset == 0 {
        return String::new();
    }
    let s = b.get(offset as usize..).unwrap_or_default();
    let n = s.iter().position(|&c| c == 0).unwrap_or(s.len());
    String::from_utf8_lossy(&s[..n]).trim().to_owned()
}
fn info(h: HANDLE, number: u32, volumes: &[VolumeInfo]) -> Result<DiskInfo> {
    // GET_LENGTH_INFO requires read access; GEOMETRY_EX works with a metadata-only handle.
    let geometry = variable(h, IOCTL_DISK_GET_DRIVE_GEOMETRY_EX, &[])?;
    let size: i64 = geometry.get(offset_of!(DISK_GEOMETRY_EX, DiskSize))?;
    let (sector_size, physical_sector_size) = alignment(h)?;
    let mut layout = parse_layout(&variable(h, IOCTL_DISK_GET_DRIVE_LAYOUT_EX, &[])?)?;
    let desc = descriptor(h)?;
    let d: STORAGE_DEVICE_DESCRIPTOR = desc.get(0)?;
    let bus_type = match d.BusType.0 {
        0 => "Unknown",
        1 => "SCSI",
        2 => "ATAPI",
        3 => "ATA",
        4 => "1394",
        5 => "SSA",
        6 => "Fibre",
        7 => "USB",
        8 => "RAID",
        9 => "iSCSI",
        10 => "SAS",
        11 => "SATA",
        12 => "SD",
        13 => "MMC",
        14 => "Virtual",
        15 => "FileBackedVirtual",
        16 => "Spaces",
        17 => "NVMe",
        18 => "SCM",
        19 => "UFS",
        _ => "Unknown",
    }
    .into();
    let attrs: GET_DISK_ATTRIBUTES = output(h, IOCTL_DISK_GET_DISK_ATTRIBUTES)?;
    let mut is_system = false;
    let pagefiles = crate::system::registry_strings(
        r"SYSTEM\CurrentControlSet\Control\Session Manager\Memory Management",
        "ExistingPageFiles",
    );
    for volume in volumes {
        if let Some(extents) = mapped_extents(&volume.guid_path)? {
            if extents.iter().any(|e| e.0 == number)
                && (volume.is_windows
                    || volume.mount_points.iter().any(|mount| {
                        pagefiles.iter().any(|p| {
                            p.trim_start_matches(r"\??\").to_ascii_lowercase().starts_with(&mount.to_ascii_lowercase())
                        })
                    }))
            {
                is_system = true;
            }
            for p in &mut layout.partitions {
                if extents.iter().any(|&(disk, offset, _)| disk == number && offset == p.offset) {
                    let mut v = volume.clone();
                    v.is_system = system_partition(&p.kind);
                    p.volume = Some(v);
                }
            }
        }
    }
    is_system |= layout.partitions.iter().any(|p| system_partition(&p.kind));
    Ok(DiskInfo {
        number,
        size: size.max(0) as u64,
        sector_size,
        physical_sector_size,
        model: ansi(desc.bytes(), d.ProductIdOffset),
        serial: ansi(desc.bytes(), d.SerialNumberOffset),
        bus_type,
        removable: d.RemovableMedia.as_bool(),
        is_virtual: matches!(d.BusType.0, 9 | 14 | 15),
        offline: attrs.Attributes & DISK_ATTRIBUTE_OFFLINE != 0,
        read_only: attrs.Attributes & DISK_ATTRIBUTE_READ_ONLY != 0,
        is_system,
        layout,
    })
}
/// Inspect a physical disk using a metadata-only handle.
pub fn disk_info(number: u32) -> Result<DiskInfo> {
    let h = open(&format!(r"\\.\PhysicalDrive{number}"), 0, FILE_FLAGS_AND_ATTRIBUTES(0))?;
    info(h.0, number, &list_volumes()?)
}
/// Enumerate disks, including non-contiguous disk numbers; propagate failures for present devices.
pub fn list_disks() -> Result<Vec<DiskInfo>> {
    let volumes = list_volumes()?;
    let mut result = Vec::new();
    // Windows disk numbers are sparse. QueryDosDevice avoids an arbitrary scan bound.
    let mut names = vec![0u16; 65536];
    let n = unsafe { QueryDosDeviceW(None, Some(&mut names)) };
    if n == 0 {
        return Err(crate::win("enumerate DOS devices", windows::core::Error::from_win32()));
    }
    let mut numbers: Vec<u32> = names[..n as usize]
        .split(|&c| c == 0)
        .filter_map(|s| string(s).strip_prefix("PhysicalDrive").and_then(|n| n.parse().ok()))
        .collect();
    numbers.sort_unstable();
    numbers.dedup();
    for number in numbers {
        match open(&format!(r"\\.\PhysicalDrive{number}"), 0, FILE_FLAGS_AND_ATTRIBUTES(0)) {
            Ok(h) => match info(h.0, number, &volumes) {
                Ok(disk) => result.push(disk),
                Err(Error::Win32 { source, .. })
                    if [ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, ERROR_NOT_READY, ERROR_NO_MEDIA_IN_DRIVE]
                        .iter()
                        .any(|e| e.to_hresult() == source.code()) => {}
                Err(e) => return Err(e),
            },
            Err(Error::Win32 { source, .. })
                if [ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, ERROR_NOT_READY]
                    .iter()
                    .any(|e| e.to_hresult() == source.code()) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(result)
}
/// Unbuffered physical disk with byte-addressable, synchronized positional I/O.
pub struct RawDisk {
    pub(crate) device: Device,
    number: u32,
}
impl RawDisk {
    /// Open a physical disk. Raw access normally requires elevation.
    pub fn open(number: u32, writable: bool) -> Result<Self> {
        Ok(Self { device: Device::open(&format!(r"\\.\PhysicalDrive{number}"), writable)?, number })
    }
    /// Physical disk number.
    pub fn number(&self) -> u32 {
        self.number
    }
    /// Refresh disk metadata.
    pub fn info(&self) -> Result<DiskInfo> {
        disk_info(self.number)
    }
    /// Replace a GPT or primary-only MBR layout. Logical MBR writes are rejected.
    pub fn set_layout(&self, layout: &DiskLayout) -> Result<()> {
        if !self.device.writable {
            return Err(Error::Other("disk is read-only".into()));
        }
        let (b, create) = build_layout(layout, self.device.sector, self.device.size)?;
        let current = parse_layout(&variable(self.device.handle.0, IOCTL_DISK_GET_DRIVE_LAYOUT_EX, &[])?)?;
        let same = match (&current.style, &layout.style) {
            (LayoutStyle::Mbr { signature: a }, LayoutStyle::Mbr { signature: b }) => a == b,
            (LayoutStyle::Gpt { disk_id: a, .. }, LayoutStyle::Gpt { disk_id: b, .. }) => a == b,
            _ => false,
        };
        if !same {
            input(self.device.handle.0, IOCTL_DISK_CREATE_DISK, &create)?;
        }
        ioctl(self.device.handle.0, IOCTL_DISK_SET_DRIVE_LAYOUT_EX, b.bytes(), &mut [])
            .0
            .map_err(|e| crate::win("set partition layout", e))?;
        self.update_properties()
    }
    /// Delete the disk's partition table.
    pub fn clear_layout(&self) -> Result<()> {
        control(self.device.handle.0, IOCTL_DISK_DELETE_DRIVE_LAYOUT)?;
        self.update_properties()
    }
    /// Ask the disk driver to refresh its cached properties.
    pub fn update_properties(&self) -> Result<()> {
        control(self.device.handle.0, IOCTL_DISK_UPDATE_PROPERTIES)
    }
    /// Change volatile offline/read-only attributes, preserving unspecified bits.
    pub fn set_attributes(&self, offline: Option<bool>, read_only: Option<bool>) -> Result<()> {
        let mut a = SET_DISK_ATTRIBUTES { Version: size_of::<SET_DISK_ATTRIBUTES>() as u32, ..Default::default() };
        for (v, bit) in [(offline, DISK_ATTRIBUTE_OFFLINE), (read_only, DISK_ATTRIBUTE_READ_ONLY)] {
            if let Some(v) = v {
                a.AttributesMask |= bit;
                if v {
                    a.Attributes |= bit;
                }
            }
        }
        input(self.device.handle.0, IOCTL_DISK_SET_DISK_ATTRIBUTES, &a)?;
        self.update_properties()
    }
    /// Lock and dismount every volume with an extent on this disk; returned handles retain locks.
    pub fn lock_and_dismount_volumes(&self) -> Result<Vec<Volume>> {
        let mut locked = Vec::new();
        for v in list_volumes()? {
            if mapped_extents(&v.guid_path)?.is_some_and(|e| e.iter().any(|e| e.0 == self.number)) {
                let volume = Volume::open(&v.guid_path, true)?;
                let mut result = volume.lock();
                for _ in 0..4 {
                    if result.is_ok() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(250));
                    result = volume.lock();
                }
                result?;
                volume.dismount()?;
                locked.push(volume);
            }
        }
        Ok(locked)
    }
}
block_device!(RawDisk);
/// Wait until at least one volume has appeared on a disk.
pub fn wait_for_disk_volumes(disk: u32, timeout: Duration) -> Result<Vec<String>> {
    let start = Instant::now();
    loop {
        let mut found = Vec::new();
        for v in list_volumes()? {
            if mapped_extents(&v.guid_path)?.is_some_and(|e| e.iter().any(|x| x.0 == disk)) {
                found.push(v.guid_path);
            }
        }
        if !found.is_empty() {
            return Ok(found);
        }
        if start.elapsed() >= timeout {
            return Err(Error::Other(format!("timed out waiting for disk {disk} volumes")));
        }
        std::thread::sleep(Duration::from_millis(200).min(timeout.saturating_sub(start.elapsed())));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn primary_mbr_roundtrip_and_validation() {
        let mut l = DiskLayout {
            style: LayoutStyle::Mbr { signature: 42 },
            partitions: vec![PartitionInfo {
                number: 2,
                offset: 1048576,
                length: 1048576,
                kind: PartitionKind::Mbr { partition_type: 7, bootable: true, logical: false },
                volume: None,
            }],
        };
        let (b, _) = build_layout(&l, 512, 64 << 20).unwrap();
        let empty: PARTITION_INFORMATION_EX = b.get(offset_of!(DRIVE_LAYOUT_INFORMATION_EX, PartitionEntry)).unwrap();
        assert!(empty.RewritePartition.as_bool());
        let parsed = parse_layout(&b).unwrap();
        assert_eq!(parsed.partitions[0].number, 2);
        assert!(system_partition(&parsed.partitions[0].kind));
        l.partitions[0].kind = PartitionKind::Mbr { partition_type: 7, bootable: false, logical: true };
        assert!(build_layout(&l, 512, 64 << 20).is_err());
    }
    #[test]
    fn extended_containers_filtered() {
        let mut b = Buffer::new(
            offset_of!(DRIVE_LAYOUT_INFORMATION_EX, PartitionEntry) + 8 * size_of::<PARTITION_INFORMATION_EX>(),
            8,
        )
        .unwrap();
        b.put(0, DRIVE_LAYOUT_INFORMATION_EX { PartitionStyle: 0, PartitionCount: 8, ..Default::default() }).unwrap();
        for (i, number, ty) in [(0, 1, 0x0f), (4, 2, 7), (5, 0, 7)] {
            let mut p =
                PARTITION_INFORMATION_EX { PartitionNumber: number, PartitionLength: 1024, ..Default::default() };
            p.Anonymous.Mbr.PartitionType = ty;
            b.put(
                offset_of!(DRIVE_LAYOUT_INFORMATION_EX, PartitionEntry) + i * size_of::<PARTITION_INFORMATION_EX>(),
                p,
            )
            .unwrap();
        }
        let l = parse_layout(&b).unwrap();
        assert_eq!(l.partitions.len(), 1);
        assert!(matches!(l.partitions[0].kind, PartitionKind::Mbr { logical: true, .. }));
    }
    #[test]
    fn enumerate_metadata() {
        let disks = list_disks().unwrap();
        assert!(!disks.is_empty());
        assert!(disks.iter().any(|d| d.is_system));
    }
    #[test]
    fn gpt_conversion_and_overlap_rejection() {
        let id = Uuid::new_v4();
        let partition_id = Uuid::new_v4();
        let mut layout = DiskLayout {
            style: LayoutStyle::Gpt {
                disk_id: id,
                starting_usable_offset: 1048576,
                usable_length: 62 << 20,
                max_partition_count: 128,
            },
            partitions: vec![PartitionInfo {
                number: 1,
                offset: 1048576,
                length: 32 << 20,
                kind: PartitionKind::Gpt {
                    type_id: Uuid::from_u128(0xc12a7328_f81f_11d2_ba4b_00a0c93ec93b),
                    id: partition_id,
                    name: "disk Ω".into(),
                    attributes: 1,
                },
                volume: None,
            }],
        };
        let (b, _) = build_layout(&layout, 512, 64 << 20).unwrap();
        let read = parse_layout(&b).unwrap();
        assert!(matches!(read.style,LayoutStyle::Gpt {disk_id,..} if disk_id==id));
        assert!(
            matches!(&read.partitions[0].kind,PartitionKind::Gpt {id,name,attributes,..} if *id==partition_id && name=="disk Ω" && *attributes==1)
        );
        assert!(system_partition(&read.partitions[0].kind));
        let mut other = layout.partitions[0].clone();
        other.number = 2;
        layout.partitions.push(other);
        assert!(build_layout(&layout, 512, 64 << 20).is_err());
        layout.partitions.pop();
        layout.partitions[0].offset += 1;
        assert!(build_layout(&layout, 512, 64 << 20).is_err());
    }
}
