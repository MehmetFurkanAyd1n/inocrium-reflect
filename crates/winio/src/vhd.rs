use crate::{
    disk::guid,
    handle::{string, wide, Handle},
    win, Error, Result,
};
use std::path::Path;
use uuid::Uuid;
use windows::{
    core::{PCWSTR, PWSTR},
    Win32::{Foundation::*, Storage::Vhd::*},
};

/// A VHD/VHDX handle. Non-permanent attachments end when the handle is closed.
pub struct VirtualDisk {
    handle: Handle,
}
fn check(context: &str, r: WIN32_ERROR) -> Result<()> {
    r.ok().map_err(|e| win(context, e))
}
impl VirtualDisk {
    /// Create a version-2 VHDX, dynamic or fully allocated, with explicit sector sizes.
    pub fn create_vhdx(
        path: &Path,
        size: u64,
        logical_sector: u32,
        physical_sector: u32,
        dynamic: bool,
    ) -> Result<Self> {
        if !matches!(logical_sector, 512 | 4096)
            || !matches!(physical_sector, 512 | 4096)
            || physical_sector < logical_sector
            || size == 0
            || size & (logical_sector as u64 - 1) != 0
        {
            return Err(Error::Other("invalid VHDX geometry".into()));
        }
        let p = wide(path);
        let storage = VIRTUAL_STORAGE_TYPE {
            DeviceId: VIRTUAL_STORAGE_TYPE_DEVICE_VHDX,
            VendorId: VIRTUAL_STORAGE_TYPE_VENDOR_MICROSOFT,
        };
        let params = CREATE_VIRTUAL_DISK_PARAMETERS {
            Version: CREATE_VIRTUAL_DISK_VERSION_2,
            Anonymous: CREATE_VIRTUAL_DISK_PARAMETERS_0 {
                Version2: CREATE_VIRTUAL_DISK_PARAMETERS_0_1 {
                    UniqueId: guid(Uuid::new_v4()),
                    MaximumSize: size,
                    SectorSizeInBytes: logical_sector,
                    PhysicalSectorSizeInBytes: physical_sector,
                    ..Default::default()
                },
            },
        };
        let mut handle = HANDLE::default();
        check("create VHDX", unsafe {
            CreateVirtualDisk(
                &storage,
                PCWSTR(p.as_ptr()),
                VIRTUAL_DISK_ACCESS_NONE,
                None,
                if dynamic { CREATE_VIRTUAL_DISK_FLAG_NONE } else { CREATE_VIRTUAL_DISK_FLAG_FULL_PHYSICAL_ALLOCATION },
                0,
                &params,
                None,
                &mut handle,
            )
        })?;
        Ok(Self { handle: Handle(handle) })
    }
    /// Open a VHD/VHDX for inspection and temporary attachment.
    pub fn open(path: &Path, read_only: bool) -> Result<Self> {
        let p = wide(path);
        let storage = VIRTUAL_STORAGE_TYPE {
            DeviceId: VIRTUAL_STORAGE_TYPE_DEVICE_UNKNOWN,
            VendorId: VIRTUAL_STORAGE_TYPE_VENDOR_MICROSOFT,
        };
        let params = OPEN_VIRTUAL_DISK_PARAMETERS {
            Version: OPEN_VIRTUAL_DISK_VERSION_2,
            Anonymous: OPEN_VIRTUAL_DISK_PARAMETERS_0 {
                Version2: OPEN_VIRTUAL_DISK_PARAMETERS_0_1 { ReadOnly: BOOL::from(read_only), ..Default::default() },
            },
        };
        let mut handle = HANDLE::default();
        check("open virtual disk", unsafe {
            OpenVirtualDisk(
                &storage,
                PCWSTR(p.as_ptr()),
                VIRTUAL_DISK_ACCESS_NONE,
                OPEN_VIRTUAL_DISK_FLAG_NONE,
                Some(&params),
                &mut handle,
            )
        })?;
        Ok(Self { handle: Handle(handle) })
    }
    /// Attach until detached or closed; optionally suppress drive-letter assignment.
    pub fn attach(&self, read_only: bool, no_drive_letters: bool) -> Result<()> {
        let mut flags = ATTACH_VIRTUAL_DISK_FLAG_NONE;
        if read_only {
            flags |= ATTACH_VIRTUAL_DISK_FLAG_READ_ONLY;
        }
        if no_drive_letters {
            flags |= ATTACH_VIRTUAL_DISK_FLAG_NO_DRIVE_LETTER;
        }
        check("attach virtual disk", unsafe { AttachVirtualDisk(self.handle.0, None, flags, 0, None, None) })
    }
    /// Physical disk number assigned to an attached virtual disk.
    pub fn disk_number(&self) -> Result<u32> {
        let mut p = vec![0u16; 1024];
        let mut bytes = (p.len() * 2) as u32;
        let mut r = unsafe { GetVirtualDiskPhysicalPath(self.handle.0, &mut bytes, PWSTR(p.as_mut_ptr())) };
        if r == ERROR_INSUFFICIENT_BUFFER {
            p.resize((bytes as usize).div_ceil(2), 0);
            r = unsafe { GetVirtualDiskPhysicalPath(self.handle.0, &mut bytes, PWSTR(p.as_mut_ptr())) };
        }
        check("virtual disk physical path", r)?;
        string(&p)
            .to_ascii_lowercase()
            .strip_prefix(r"\\.\physicaldrive")
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| Error::Other("unexpected virtual disk physical path".into()))
    }
    /// Detach a virtual disk.
    pub fn detach(&self) -> Result<()> {
        check("detach virtual disk", unsafe { DetachVirtualDisk(self.handle.0, DETACH_VIRTUAL_DISK_FLAG_NONE, 0) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{disk_info, DiskLayout, LayoutStyle, PartitionInfo, PartitionKind, RawDisk};
    use blockdev::BlockDevice;
    // Run from an elevated terminal: cargo test -p winio -- --ignored (set INOCrium_ELEVATED_TESTS=1).
    #[test]
    #[ignore = "requires administrator: creates and attaches only a disposable VHDX"]
    fn vhdx_gpt_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let v = VirtualDisk::create_vhdx(&dir.path().join("test.vhdx"), 64 << 20, 512, 4096, true).unwrap();
        v.attach(false, true).unwrap();
        let n = v.disk_number().unwrap();
        let disk = RawDisk::open(n, true).unwrap();
        let id = Uuid::new_v4();
        let layout = DiskLayout {
            style: LayoutStyle::Gpt {
                disk_id: id,
                starting_usable_offset: 1048576,
                usable_length: (62 << 20),
                max_partition_count: 128,
            },
            partitions: vec![PartitionInfo {
                number: 1,
                offset: 1048576,
                length: 32 << 20,
                kind: PartitionKind::Gpt {
                    type_id: Uuid::from_u128(0xebd0a0a2_b9e5_4433_87c0_68b6b72699c7),
                    id: Uuid::new_v4(),
                    name: "inocrium test".into(),
                    attributes: 0,
                },
                volume: None,
            }],
        };
        disk.set_layout(&layout).unwrap();
        let info = disk_info(n).unwrap();
        assert!(matches!(info.layout.style,LayoutStyle::Gpt {disk_id,..} if disk_id==id));
        assert_eq!(info.layout.partitions.len(), 1);
        disk.write_at(1048576 + 3, b"unaligned").unwrap();
        let mut b = [0; 9];
        disk.read_at(1048576 + 3, &mut b).unwrap();
        assert_eq!(&b, b"unaligned");
        drop(disk);
        v.detach().unwrap();
    }
}
