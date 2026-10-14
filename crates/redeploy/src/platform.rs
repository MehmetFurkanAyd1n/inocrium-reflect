//! Win32 implementation; handles, registry keys, and temporary mounts are scoped.

use std::{
    ffi::OsStr,
    mem::{offset_of, size_of},
    os::windows::ffi::OsStrExt,
    path::{Path, PathBuf},
    sync::Mutex,
};

use windows::{
    core::PCWSTR,
    Win32::{
        Foundation::{
            CloseHandle, GetLastError, ERROR_FILE_NOT_FOUND, ERROR_INSUFFICIENT_BUFFER, ERROR_MORE_DATA,
            ERROR_NO_MORE_FILES, HANDLE, LUID, WIN32_ERROR,
        },
        Security::{
            AdjustTokenPrivileges, LookupPrivilegeValueW, LUID_AND_ATTRIBUTES, SE_PRIVILEGE_ENABLED,
            TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY,
        },
        Storage::FileSystem::{
            CreateFileW, DeleteVolumeMountPointW, FindFirstVolumeW, FindNextVolumeW, FindVolumeClose, GetLogicalDrives,
            GetVolumeNameForVolumeMountPointW, GetVolumePathNamesForVolumeNameW, SetVolumeMountPointW,
            FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS,
            OPEN_EXISTING, PARTITION_SYSTEM_GUID,
        },
        System::{
            Ioctl::{
                DISK_EXTENT, DRIVE_LAYOUT_INFORMATION_EX, IOCTL_DISK_GET_DRIVE_LAYOUT_EX, PARTITION_INFORMATION_EX,
                PARTITION_STYLE, PARTITION_STYLE_GPT, PARTITION_STYLE_MBR, VOLUME_DISK_EXTENTS,
            },
            Registry::{
                RegCloseKey, RegDeleteTreeW, RegLoadKeyW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
                RegUnLoadKeyW, HKEY, HKEY_LOCAL_MACHINE, KEY_ALL_ACCESS, KEY_WOW64_64KEY, REG_DWORD, REG_VALUE_TYPE,
            },
            Threading::{GetCurrentProcess, OpenProcessToken},
            IO::DeviceIoControl,
        },
    },
};

use crate::{
    plan_storage_changes, Error, Firmware, RegistryChange, Result, ServiceState, WindowsInstall, STORAGE_DRIVERS,
};

fn wide(value: impl AsRef<OsStr>) -> Result<Vec<u16>> {
    let value: Vec<_> = value.as_ref().encode_wide().chain(Some(0)).collect();
    if value[..value.len() - 1].contains(&0) {
        return Err(Error::InvalidOptions("Windows path contains a NUL character".into()));
    }
    Ok(value)
}

fn win32(status: WIN32_ERROR) -> Result<()> {
    status.ok().map_err(Error::from)
}

fn is_error(error: &windows::core::Error, code: WIN32_ERROR) -> bool {
    error.code() == windows::core::HRESULT::from_win32(code.0)
}

struct Handle(HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: this guard exclusively owns a successful CreateFile/OpenProcessToken handle.
        if let Err(error) = unsafe { CloseHandle(self.0) } {
            tracing::warn!(%error, "Could not close handle");
        }
    }
}

fn open_device(path: &str) -> Result<Handle> {
    let path = wide(path)?;
    // SAFETY: path is NUL-terminated and lives through the call; no borrowed handle is transferred.
    Ok(Handle(unsafe {
        CreateFileW(
            PCWSTR(path.as_ptr()),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )?
    }))
}

/// Buffer is u64-aligned and grows on variable-length IOCTL responses.
fn ioctl(handle: HANDLE, code: u32) -> Result<(Vec<u64>, usize)> {
    let mut bytes = 4096usize;
    loop {
        let mut buffer = vec![0u64; bytes / size_of::<u64>()];
        let mut returned = 0;
        // SAFETY: buffer has `bytes` writable bytes; operation is synchronous with no input buffer.
        let result = unsafe {
            DeviceIoControl(
                handle,
                code,
                None,
                0,
                Some(buffer.as_mut_ptr().cast()),
                bytes as u32,
                Some(&mut returned),
                None,
            )
        };
        match result {
            Ok(()) => {
                if returned as usize > bytes {
                    return Err(Error::InvalidData("IOCTL returned an oversized response".into()));
                }
                return Ok((buffer, returned as usize));
            }
            Err(error)
                if (is_error(&error, ERROR_MORE_DATA) || is_error(&error, ERROR_INSUFFICIENT_BUFFER))
                    && bytes < 16 * 1024 * 1024 =>
            {
                bytes *= 2;
            }
            Err(error) => return Err(error.into()),
        }
    }
}

fn read_pod<T: Copy>(buffer: &[u64], bytes: usize, offset: usize) -> Result<T> {
    if offset.checked_add(size_of::<T>()).is_none_or(|end| end > bytes || end > std::mem::size_of_val(buffer)) {
        return Err(Error::InvalidData("truncated disk layout or volume extents".into()));
    }
    // SAFETY: bounds are checked; callers use only Win32 POD structs filled by synchronous IOCTLs.
    Ok(unsafe { buffer.as_ptr().cast::<u8>().add(offset).cast::<T>().read_unaligned() })
}

fn disk_layout(disk_number: u32) -> Result<(Firmware, Vec<(i64, i64)>)> {
    let disk = open_device(&format!("\\\\.\\PhysicalDrive{disk_number}"))?;
    let (buffer, bytes) = ioctl(disk.0, IOCTL_DISK_GET_DRIVE_LAYOUT_EX)?;
    parse_layout(&buffer, bytes)
}

fn parse_layout(buffer: &[u64], bytes: usize) -> Result<(Firmware, Vec<(i64, i64)>)> {
    let style: u32 = read_pod(buffer, bytes, offset_of!(DRIVE_LAYOUT_INFORMATION_EX, PartitionStyle))?;
    let count: u32 = read_pod(buffer, bytes, offset_of!(DRIVE_LAYOUT_INFORMATION_EX, PartitionCount))?;
    validate_array(
        bytes,
        offset_of!(DRIVE_LAYOUT_INFORMATION_EX, PartitionEntry),
        size_of::<PARTITION_INFORMATION_EX>(),
        count,
    )?;
    let firmware = match PARTITION_STYLE(style as i32) {
        PARTITION_STYLE_GPT => Firmware::Uefi,
        PARTITION_STYLE_MBR => Firmware::Bios,
        _ => return Err(Error::InvalidData("disk has no GPT or MBR partition layout".into())),
    };
    let mut system_partitions = Vec::new();
    for i in 0..count as usize {
        let partition: PARTITION_INFORMATION_EX = read_pod(
            buffer,
            bytes,
            offset_of!(DRIVE_LAYOUT_INFORMATION_EX, PartitionEntry) + i * size_of::<PARTITION_INFORMATION_EX>(),
        )?;
        if partition.PartitionLength <= 0 {
            continue;
        }
        // SAFETY: the union arm is selected by this partition's PartitionStyle.
        let system = unsafe {
            match partition.PartitionStyle {
                PARTITION_STYLE_GPT => partition.Anonymous.Gpt.PartitionType == PARTITION_SYSTEM_GUID,
                PARTITION_STYLE_MBR => partition.Anonymous.Mbr.BootIndicator.0 != 0,
                _ => false,
            }
        };
        if system {
            system_partitions.push((partition.StartingOffset, partition.PartitionLength));
        }
    }
    Ok((firmware, system_partitions))
}

fn validate_array(bytes: usize, offset: usize, element_size: usize, count: u32) -> Result<()> {
    if (count as usize)
        .checked_mul(element_size)
        .and_then(|size| offset.checked_add(size))
        .is_none_or(|size| size > bytes)
    {
        return Err(Error::InvalidData("truncated IOCTL array".into()));
    }
    Ok(())
}

fn volume_extents(volume: &str) -> Result<Vec<DISK_EXTENT>> {
    let handle = open_device(volume.trim_end_matches('\\'))?;
    let (buffer, bytes) = ioctl(handle.0, IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS)?;
    let count: u32 = read_pod(&buffer, bytes, 0)?;
    validate_array(bytes, offset_of!(VOLUME_DISK_EXTENTS, Extents), size_of::<DISK_EXTENT>(), count)?;
    (0..count as usize)
        .map(|i| read_pod(&buffer, bytes, offset_of!(VOLUME_DISK_EXTENTS, Extents) + i * size_of::<DISK_EXTENT>()))
        .collect()
}

fn mount_paths(volume: &str) -> Result<Vec<PathBuf>> {
    let volume = wide(volume)?;
    let mut buffer = vec![0u16; 256];
    loop {
        let mut length = 0;
        // SAFETY: volume is NUL-terminated; buffer and length are valid writable storage.
        match unsafe { GetVolumePathNamesForVolumeNameW(PCWSTR(volume.as_ptr()), Some(&mut buffer), &mut length) } {
            Ok(()) => {
                return Ok(buffer
                    .split(|c| *c == 0)
                    .take_while(|s| !s.is_empty())
                    .map(|s| {
                        use std::os::windows::ffi::OsStringExt;
                        PathBuf::from(std::ffi::OsString::from_wide(s))
                    })
                    .collect())
            }
            Err(error)
                if is_error(&error, ERROR_MORE_DATA) && length > buffer.len() as u32 && length <= 1024 * 1024 =>
            {
                buffer.resize(length as usize, 0);
            }
            Err(error) => return Err(error.into()),
        }
    }
}

struct VolumeSearch(HANDLE);

impl Drop for VolumeSearch {
    fn drop(&mut self) {
        // SAFETY: handle came from FindFirstVolumeW and is closed exactly once.
        if let Err(error) = unsafe { FindVolumeClose(self.0) } {
            tracing::warn!(%error, "Could not close volume enumeration");
        }
    }
}

pub(super) fn find_windows_installs(disk_number: u32) -> Result<Vec<WindowsInstall>> {
    let (firmware, partitions) = disk_layout(disk_number)?;
    let mut buffer = [0u16; 1024];
    // SAFETY: buffer remains valid for this synchronous call.
    let search = match unsafe { FindFirstVolumeW(&mut buffer) } {
        Ok(handle) => VolumeSearch(handle),
        Err(error) if is_error(&error, ERROR_NO_MORE_FILES) => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut installs = Vec::new();
    let mut system_volume = None;
    loop {
        let end =
            buffer.iter().position(|c| *c == 0).ok_or_else(|| Error::InvalidData("unterminated volume name".into()))?;
        let volume =
            String::from_utf16(&buffer[..end]).map_err(|_| Error::InvalidData("invalid volume name".into()))?;
        match volume_extents(&volume) {
            Ok(extents) if extents.iter().any(|e| e.DiskNumber == disk_number) => {
                if extents
                    .iter()
                    .any(|e| e.DiskNumber == disk_number && partitions.contains(&(e.StartingOffset, e.ExtentLength)))
                {
                    if system_volume.is_none() {
                        system_volume = Some(volume.clone());
                    } else {
                        tracing::warn!(%volume, "Multiple boot partitions found; using the first");
                    }
                }
                let paths = mount_paths(&volume)?;
                if Path::new(&volume).join("Windows/System32/config/SYSTEM").try_exists()? {
                    let mount_path = paths.iter().find(|p| p.as_os_str().len() == 3).or(paths.first()).cloned();
                    installs.push(WindowsInstall { volume, mount_path, system_volume: None, firmware });
                }
            }
            Ok(_) => {}
            Err(error) => tracing::debug!(%volume, %error, "Skipping volume whose disk extents are unavailable"),
        }
        // SAFETY: search handle remains open; buffer is writable and sufficiently sized for volume GUID names.
        match unsafe { FindNextVolumeW(search.0, &mut buffer) } {
            Ok(()) => {}
            Err(error) if is_error(&error, ERROR_NO_MORE_FILES) => break,
            Err(error) => return Err(error.into()),
        }
    }
    for install in &mut installs {
        install.system_volume.clone_from(&system_volume);
    }
    Ok(installs)
}

// Serialize this crate's process-token privilege changes and hive repairs.
static HIVE_LOCK: Mutex<()> = Mutex::new(());

struct Privileges {
    token: Handle,
    previous: Vec<TOKEN_PRIVILEGES>,
}

impl Privileges {
    fn enable() -> Result<Self> {
        let mut token = HANDLE::default();
        // SAFETY: process pseudo-handle is valid; token output points to initialized storage.
        unsafe {
            OpenProcessToken(GetCurrentProcess(), TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY, &mut token)?;
        }
        let mut guard = Self { token: Handle(token), previous: Vec::new() };
        for name in ["SeBackupPrivilege", "SeRestorePrivilege"] {
            let name = wide(name)?;
            let mut luid = LUID::default();
            // SAFETY: local lookup uses a NUL-terminated privilege name and writable LUID.
            unsafe {
                LookupPrivilegeValueW(None, PCWSTR(name.as_ptr()), &mut luid)?;
            }
            let desired = TOKEN_PRIVILEGES {
                PrivilegeCount: 1,
                Privileges: [LUID_AND_ATTRIBUTES { Luid: luid, Attributes: SE_PRIVILEGE_ENABLED }],
            };
            let mut previous = TOKEN_PRIVILEGES::default();
            // SAFETY: both privilege buffers hold exactly one entry; token permits adjustment/query.
            unsafe {
                AdjustTokenPrivileges(
                    token,
                    false,
                    Some(&desired),
                    size_of::<TOKEN_PRIVILEGES>() as u32,
                    Some(&mut previous),
                    None,
                )?;
                let status = GetLastError();
                guard.previous.push(previous);
                win32(status)?;
            }
        }
        Ok(guard)
    }
}

impl Drop for Privileges {
    fn drop(&mut self) {
        for previous in self.previous.iter().rev() {
            // SAFETY: saved privilege state came from this token; token remains owned and open.
            if let Err(error) = unsafe { AdjustTokenPrivileges(self.token.0, false, Some(previous), 0, None, None) } {
                tracing::warn!(%error, "Could not restore token privileges");
            }
        }
    }
}

struct Key(HKEY);

impl Key {
    fn open(parent: HKEY, path: &str) -> Result<Option<Self>> {
        let path = wide(path)?;
        let mut key = HKEY::default();
        // SAFETY: parent is valid, path is NUL-terminated, and key output is writable.
        let status =
            unsafe { RegOpenKeyExW(parent, PCWSTR(path.as_ptr()), 0, KEY_ALL_ACCESS | KEY_WOW64_64KEY, &mut key) };
        if status == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        win32(status)?;
        Ok(Some(Self(key)))
    }

    fn dword(&self, name: &str) -> Result<Option<u32>> {
        let name = wide(name)?;
        let mut value = 0u32;
        let mut kind = REG_VALUE_TYPE::default();
        let mut size = size_of::<u32>() as u32;
        // SAFETY: a DWORD-sized output and its type/size outputs remain valid during the call.
        let status = unsafe {
            RegQueryValueExW(
                self.0,
                PCWSTR(name.as_ptr()),
                None,
                Some(&mut kind),
                Some((&mut value as *mut u32).cast()),
                Some(&mut size),
            )
        };
        if status == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        win32(status)?;
        if kind != REG_DWORD || size != size_of::<u32>() as u32 {
            return Err(Error::InvalidData("registry value is not a DWORD".into()));
        }
        Ok(Some(value))
    }
}

impl Drop for Key {
    fn drop(&mut self) {
        // SAFETY: owns a registry key returned by RegOpenKeyExW, not a predefined root handle.
        if let Err(error) = unsafe { RegCloseKey(self.0) }.ok() {
            tracing::warn!(%error, "Could not close registry key");
        }
    }
}

struct Hive {
    name: Vec<u16>,
    loaded: bool,
}

impl Hive {
    fn load(path: &Path) -> Result<Self> {
        let name = wide(format!("IRRedeploy_{}", uuid::Uuid::new_v4().simple()))?;
        let file = wide(path)?;
        // SAFETY: both strings live through the call; privileges are enabled by the caller.
        win32(unsafe { RegLoadKeyW(HKEY_LOCAL_MACHINE, PCWSTR(name.as_ptr()), PCWSTR(file.as_ptr())) })?;
        Ok(Self { name, loaded: true })
    }

    fn unload(&mut self) -> Result<()> {
        // SAFETY: this is the unique temporary key loaded by this guard; child keys have been closed.
        win32(unsafe { RegUnLoadKeyW(HKEY_LOCAL_MACHINE, PCWSTR(self.name.as_ptr())) })?;
        self.loaded = false;
        Ok(())
    }
}

impl Drop for Hive {
    fn drop(&mut self) {
        if self.loaded {
            if let Err(error) = self.unload() {
                tracing::error!(%error, key = %String::from_utf16_lossy(&self.name), "Could not unload offline hive");
            }
        }
    }
}

pub(super) fn repair_hive(root: &Path, log: &dyn Fn(&str)) -> Result<Vec<RegistryChange>> {
    let _lock = HIVE_LOCK.lock().map_err(|_| Error::InvalidData("hive repair lock poisoned".into()))?;
    let _privileges = Privileges::enable()?;
    let mut hive = Hive::load(&root.join("Windows/System32/config/SYSTEM"))?;
    let changes = {
        let name = String::from_utf16_lossy(&hive.name[..hive.name.len() - 1]);
        let key = Key::open(HKEY_LOCAL_MACHINE, &name)?
            .ok_or_else(|| Error::InvalidData("loaded hive disappeared".into()))?;
        let select =
            Key::open(key.0, "Select")?.ok_or_else(|| Error::InvalidData("SYSTEM hive lacks Select key".into()))?;
        let current = select
            .dword("Current")?
            .filter(|n| (1..=999).contains(n))
            .ok_or_else(|| Error::InvalidData("SYSTEM hive has invalid Select\\Current".into()))?;
        let services_path = format!("ControlSet{current:03}\\Services");
        let services = Key::open(key.0, &services_path)?
            .ok_or_else(|| Error::InvalidData("selected control set lacks Services".into()))?;
        let mut states = Vec::new();
        for name in STORAGE_DRIVERS {
            if let Some(service) = Key::open(services.0, name)? {
                states.push(ServiceState {
                    name: (*name).into(),
                    start: service.dword("Start")?,
                    has_start_override: Key::open(service.0, "StartOverride")?.is_some(),
                });
            }
        }
        let mut changes = plan_storage_changes(&states);
        for change in &mut changes {
            let service = Key::open(services.0, &change.service)?
                .ok_or_else(|| Error::InvalidData("storage service disappeared".into()))?;
            let start = wide("Start")?;
            // SAFETY: the service key is writable, name is terminated, and data is a four-byte DWORD.
            win32(unsafe {
                RegSetValueExW(service.0, PCWSTR(start.as_ptr()), 0, REG_DWORD, Some(&change.start.to_le_bytes()))
            })?;
            if change.clear_start_override {
                let name = wide("StartOverride")?;
                // SAFETY: deletes only the selected service's override subtree, with a live writable key.
                let status = unsafe { RegDeleteTreeW(service.0, PCWSTR(name.as_ptr())) };
                if status != ERROR_FILE_NOT_FOUND {
                    win32(status)?;
                }
            }
            change.applied = true;
            log(&format!(
                "{}: Start {:?} → 0{}",
                change.service,
                change.previous_start,
                if change.clear_start_override { "; removed StartOverride" } else { "" }
            ));
            tracing::info!(service = %change.service, "Enabled inbox boot driver");
        }
        changes
    }; // All child keys close before unloading the hive.
    hive.unload()?;
    Ok(changes)
}

pub(super) struct TemporaryMount {
    mount: String,
    volume: String,
    mounted: bool,
}

impl TemporaryMount {
    pub(super) fn new(volume: &str) -> Result<Self> {
        let volume_wide = wide(volume)?;
        // SAFETY: GetLogicalDrives takes no pointers or handles.
        let drives = unsafe { GetLogicalDrives() };
        if drives == 0 {
            return Err(windows::core::Error::from_win32().into());
        }
        let mut last_error = None;
        for index in (3..26u8).rev().filter(|i| drives & (1 << i) == 0) {
            let mount = format!("{}:\\", char::from(b'A' + index));
            let path = wide(&mount)?;
            // SAFETY: mount and volume are NUL-terminated; only a previously unused letter is requested.
            match unsafe { SetVolumeMountPointW(PCWSTR(path.as_ptr()), PCWSTR(volume_wide.as_ptr())) } {
                Ok(()) => return Ok(Self { mount, volume: volume.into(), mounted: true }),
                Err(error) => last_error = Some(error), // A concurrent assignment can consume a free letter.
            }
        }
        Err(last_error
            .map(Error::from)
            .unwrap_or_else(|| Error::InvalidData("no free drive letter from D: to Z:".into())))
    }

    pub(super) fn letter(&self) -> &str {
        &self.mount[..2]
    }

    pub(super) fn remove(&mut self) -> Result<()> {
        if !self.mounted {
            return Ok(());
        }
        let mount = wide(&self.mount)?;
        let mut volume = [0u16; 1024];
        // SAFETY: mount is terminated, output is writable, and the mount still belongs to this guard.
        unsafe {
            GetVolumeNameForVolumeMountPointW(PCWSTR(mount.as_ptr()), &mut volume)?;
        }
        let end = volume
            .iter()
            .position(|c| *c == 0)
            .ok_or_else(|| Error::InvalidData("unterminated mount volume".into()))?;
        if !String::from_utf16_lossy(&volume[..end]).eq_ignore_ascii_case(&self.volume) {
            return Err(Error::InvalidData("temporary letter now belongs to a different volume".into()));
        }
        // SAFETY: deletes this guard's mount point, not the backing volume or its files.
        unsafe {
            DeleteVolumeMountPointW(PCWSTR(mount.as_ptr()))?;
        }
        self.mounted = false;
        Ok(())
    }
}

impl Drop for TemporaryMount {
    fn drop(&mut self) {
        if let Err(error) = self.remove() {
            tracing::error!(%error, mount = %self.mount, "Could not remove temporary drive letter");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_truncated_ioctl_buffers() {
        assert!(read_pod::<DISK_EXTENT>(&[0; 8], 4, 0).is_err());
        assert!(read_pod::<u32>(&[0; 8], 64, usize::MAX).is_err());
        assert_eq!(read_pod::<u32>(&[7; 1], 8, 0).unwrap(), 7);
        assert!(validate_array(64, 8, size_of::<DISK_EXTENT>(), u32::MAX).is_err());
    }

    fn write_field<T: Copy>(buffer: &mut [u64], offset: usize, value: T) {
        assert!(offset + size_of::<T>() <= std::mem::size_of_val(buffer));
        // SAFETY: bounds are checked; tests only write fully initialized scalar/GUID fields.
        unsafe {
            buffer.as_mut_ptr().cast::<u8>().add(offset).cast::<T>().write_unaligned(value);
        }
    }

    #[test]
    fn recognizes_gpt_esp_and_mbr_active_partition() {
        use windows::Win32::System::Ioctl::PARTITION_INFORMATION_MBR;
        for (style, firmware) in [(PARTITION_STYLE_GPT, Firmware::Uefi), (PARTITION_STYLE_MBR, Firmware::Bios)] {
            let mut buffer = vec![0u64; 256];
            let bytes = std::mem::size_of_val(buffer.as_slice());
            write_field(&mut buffer, offset_of!(DRIVE_LAYOUT_INFORMATION_EX, PartitionStyle), style.0 as u32);
            write_field(&mut buffer, offset_of!(DRIVE_LAYOUT_INFORMATION_EX, PartitionCount), 2u32);
            for i in 0..2 {
                let base =
                    offset_of!(DRIVE_LAYOUT_INFORMATION_EX, PartitionEntry) + i * size_of::<PARTITION_INFORMATION_EX>();
                write_field(&mut buffer, base + offset_of!(PARTITION_INFORMATION_EX, PartitionStyle), style);
                write_field(
                    &mut buffer,
                    base + offset_of!(PARTITION_INFORMATION_EX, StartingOffset),
                    1024i64 + i as i64 * 4096,
                );
                write_field(&mut buffer, base + offset_of!(PARTITION_INFORMATION_EX, PartitionLength), 4096i64);
                if i == 1 {
                    let union = base + offset_of!(PARTITION_INFORMATION_EX, Anonymous);
                    if style == PARTITION_STYLE_GPT {
                        write_field(&mut buffer, union, PARTITION_SYSTEM_GUID);
                    } else {
                        write_field(&mut buffer, union + offset_of!(PARTITION_INFORMATION_MBR, BootIndicator), 1u8);
                    }
                }
            }
            let (actual_firmware, partitions) = parse_layout(&buffer, bytes).unwrap();
            assert_eq!(actual_firmware, firmware);
            assert_eq!(partitions, vec![(5120, 4096)]);
            assert!(parse_layout(&buffer, 8).is_err());
        }
    }

    #[test]
    fn rejects_embedded_nul_paths() {
        assert!(wide("E:\\bad\0path").is_err());
    }

    #[test]
    #[ignore = "requires administrator privileges and an offline Windows installation; set IR_REDEPLOY_TEST_ROOT"]
    fn repair_offline_test_hive() {
        let root = std::env::var_os("IR_REDEPLOY_TEST_ROOT").expect("set a disposable offline installation root");
        repair_hive(Path::new(&root), &|s| println!("{s}")).unwrap();
    }

    #[test]
    #[ignore = "requires administrator disk-query access; set IR_REDEPLOY_TEST_DISK"]
    fn enumerate_test_disk() {
        let disk = std::env::var("IR_REDEPLOY_TEST_DISK").expect("set a test disk number").parse().unwrap();
        find_windows_installs(disk).unwrap();
    }
}
