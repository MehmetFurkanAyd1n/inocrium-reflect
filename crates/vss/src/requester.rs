//! ABI shim for IVssBackupComponents, absent from windows-rs 0.58.
//! Slots/signatures verified against Windows SDK 10.0.26100.0 um/vsbackup.h.
//! Unused slots are opaque function pointers and are never invoked.
use crate::{check, error, Result, Snapshot};
use std::{
    ffi::c_void,
    ptr::NonNull,
    sync::OnceLock,
    time::{Duration, Instant},
};
use uuid::Uuid;
use windows::{
    core::{s, w, Interface, GUID, HRESULT, PCWSTR},
    Win32::{
        Foundation::*,
        Security::PSECURITY_DESCRIPTOR,
        Storage::{FileSystem::GetVolumeNameForVolumeMountPointW, Vss::*},
        System::{Com::*, LibraryLoader::*},
    },
};

type AsyncFn = unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> HRESULT;
type SimpleFn = unsafe extern "system" fn(*mut c_void) -> HRESULT;
#[repr(C)]
struct BackupVtbl {
    unknown: [usize; 2],
    release: unsafe extern "system" fn(*mut c_void) -> u32,
    writer_components: [usize; 2],
    initialize: unsafe extern "system" fn(*mut c_void, *const u16) -> HRESULT,
    backup_state: unsafe extern "system" fn(*mut c_void, bool, bool, VSS_BACKUP_TYPE, bool) -> HRESULT,
    restore: [usize; 2],
    gather: AsyncFn,
    metadata_get: [usize; 2],
    free_metadata: SimpleFn,
    add_component: usize,
    prepare: AsyncFn,
    abort: SimpleFn,
    writer_and_restore: [usize; 11],
    complete: AsyncFn,
    restore_files: [usize; 7],
    context: unsafe extern "system" fn(*mut c_void, i32) -> HRESULT,
    start: unsafe extern "system" fn(*mut c_void, *mut GUID) -> HRESULT,
    add: unsafe extern "system" fn(*mut c_void, *mut u16, GUID, *mut GUID) -> HRESULT,
    snapshot: AsyncFn,
    delete: unsafe extern "system" fn(*mut c_void, GUID, VSS_OBJECT_TYPE, BOOL, *mut i32, *mut GUID) -> HRESULT,
    import_and_break: [usize; 2],
    properties: unsafe extern "system" fn(*mut c_void, GUID, *mut VSS_SNAPSHOT_PROP) -> HRESULT,
    query: unsafe extern "system" fn(*mut c_void, GUID, VSS_OBJECT_TYPE, VSS_OBJECT_TYPE, *mut *mut c_void) -> HRESULT,
}
struct Api {
    module: HMODULE,
    create: unsafe extern "system" fn(*mut *mut c_void) -> HRESULT,
    free: unsafe extern "system" fn(*mut VSS_SNAPSHOT_PROP),
}
impl Api {
    fn load() -> Result<Self> {
        let module = unsafe { LoadLibraryExW(w!("vssapi.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32) }
            .map_err(|e| error("load vssapi.dll", e.code()))?;
        let create = unsafe { GetProcAddress(module, s!("CreateVssBackupComponentsInternal")) };
        let free = unsafe { GetProcAddress(module, s!("VssFreeSnapshotPropertiesInternal")) };
        match (create, free) {
            (Some(c), Some(f)) => Ok(Self {
                module,
                create: unsafe {
                    std::mem::transmute::<
                        unsafe extern "system" fn() -> isize,
                        unsafe extern "system" fn(*mut *mut c_void) -> HRESULT,
                    >(c)
                },
                free: unsafe {
                    std::mem::transmute::<
                        unsafe extern "system" fn() -> isize,
                        unsafe extern "system" fn(*mut VSS_SNAPSHOT_PROP),
                    >(f)
                },
            }),
            _ => {
                unsafe {
                    let _ = FreeLibrary(module);
                }
                Err(error("missing VSS entry point", E_NOINTERFACE))
            }
        }
    }
}
impl Drop for Api {
    fn drop(&mut self) {
        unsafe {
            let _ = FreeLibrary(self.module);
        }
    }
}
struct Apartment {
    uninitialize: bool,
}
impl Apartment {
    fn new() -> Result<Self> {
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if hr.is_err() && hr != RPC_E_CHANGED_MODE {
            return Err(error("initialize COM MTA", hr));
        }
        let apartment = Self { uninitialize: hr.is_ok() };
        static SECURITY: OnceLock<Result<()>> = OnceLock::new();
        SECURITY
            .get_or_init(|| {
                let r = unsafe {
                    CoInitializeSecurity(
                        PSECURITY_DESCRIPTOR::default(),
                        -1,
                        None,
                        None,
                        RPC_C_AUTHN_LEVEL_PKT_PRIVACY,
                        RPC_C_IMP_LEVEL_IDENTIFY,
                        None,
                        EOAC_NONE,
                        None,
                    )
                };
                match r {
                    Err(e) if e.code() == RPC_E_TOO_LATE => Ok(()),
                    r => check("initialize COM security", r),
                }
            })
            .clone()?;
        Ok(apartment)
    }
}
impl Drop for Apartment {
    fn drop(&mut self) {
        if self.uninitialize {
            unsafe {
                CoUninitialize();
            }
        }
    }
}
struct Backup {
    ptr: NonNull<c_void>,
    api: Api,
    _apartment: Apartment,
}
impl Backup {
    fn new(context: i32) -> Result<Self> {
        let apartment = Apartment::new()?;
        let api = Api::load()?;
        let mut ptr = std::ptr::null_mut();
        check("create VSS backup components", unsafe { (api.create)(&mut ptr) }.ok())?;
        let backup = Self {
            ptr: NonNull::new(ptr).ok_or_else(|| error("null backup components", E_POINTER))?,
            api,
            _apartment: apartment,
        };
        check("InitializeForBackup", unsafe { (backup.v().initialize)(backup.p(), std::ptr::null()) }.ok())?;
        check("SetContext", unsafe { (backup.v().context)(backup.p(), context) }.ok())?;
        Ok(backup)
    }
    fn p(&self) -> *mut c_void {
        self.ptr.as_ptr()
    }
    fn v(&self) -> &BackupVtbl {
        unsafe { &**self.p().cast::<*const BackupVtbl>() }
    }
    fn asynchronous(&self, context: &str, f: AsyncFn) -> Result<()> {
        let mut ptr = std::ptr::null_mut();
        let hr = unsafe { f(self.p(), &mut ptr) };
        // Own even an unexpected non-null result accompanying a failed HRESULT.
        let op = if ptr.is_null() { None } else { Some(unsafe { IVssAsync::from_raw(ptr) }) };
        check(context, hr.ok())?;
        let op = op.ok_or_else(|| error(format!("{context}: null async interface"), E_POINTER))?;
        wait(context, &op)
    }
    fn properties(&self, id: GUID) -> Result<Snapshot> {
        let mut prop = VSS_SNAPSHOT_PROP::default();
        let hr = unsafe { (self.v().properties)(self.p(), id, &mut prop) };
        let r = check("GetSnapshotProperties", hr.ok()).map(|()| snapshot(&prop));
        unsafe { (self.api.free)(&mut prop) };
        r
    }
    fn delete(&self, id: GUID) -> Result<()> {
        let mut count = 0;
        let mut remaining = GUID::zeroed();
        let hr =
            unsafe { (self.v().delete)(self.p(), id, VSS_OBJECT_SNAPSHOT_SET, BOOL(1), &mut count, &mut remaining) };
        if hr == VSS_E_OBJECT_NOT_FOUND {
            return Ok(());
        }
        check("DeleteSnapshots", hr.ok())
    }
}
impl Drop for Backup {
    fn drop(&mut self) {
        unsafe {
            (self.v().release)(self.p());
        }
    }
}
fn wait(context: &str, op: &IVssAsync) -> Result<()> {
    let start = Instant::now();
    loop {
        check(context, unsafe { op.Wait(1000) })?;
        let mut result = HRESULT(0);
        let mut reserved = 0;
        check("QueryStatus", unsafe { op.QueryStatus(&mut result, &mut reserved) })?;
        if result == VSS_S_ASYNC_FINISHED || result == S_OK {
            return Ok(());
        }
        if result != VSS_S_ASYNC_PENDING {
            return Err(error(context, result));
        }
        if start.elapsed() >= Duration::from_secs(120) {
            unsafe {
                let _ = op.Cancel();
            }
            return Err(error(format!("{context}: timed out after 120 seconds"), HRESULT::from_win32(ERROR_TIMEOUT.0)));
        }
    }
}
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}
pub(crate) fn normalize_volume(volume: &str) -> Result<String> {
    if volume.is_empty() || volume.contains('\0') {
        return Err(error("invalid volume path", E_INVALIDARG));
    }
    let path = format!("{}\\", volume.trim_end_matches('\\'));
    if path.starts_with(r"\\?\") {
        return Ok(path);
    }
    let p = wide(&path);
    let mut guid = [0u16; 1024];
    unsafe { GetVolumeNameForVolumeMountPointW(PCWSTR(p.as_ptr()), &mut guid) }
        .map_err(|e| error("resolve volume GUID", e.code()))?;
    Ok(String::from_utf16_lossy(&guid[..guid.iter().position(|&c| c == 0).unwrap_or(guid.len())]))
}
fn guid(id: Uuid) -> GUID {
    GUID::from_u128(id.as_u128())
}
fn uuid(id: GUID) -> Uuid {
    Uuid::from_u128(id.to_u128())
}
fn text(p: *const u16) -> String {
    if p.is_null() {
        String::new()
    } else {
        unsafe { PCWSTR(p).to_string().unwrap_or_default() }
    }
}
fn snapshot(p: &VSS_SNAPSHOT_PROP) -> Snapshot {
    Snapshot {
        id: uuid(p.m_SnapshotId),
        set_id: uuid(p.m_SnapshotSetId),
        original_volume: text(p.m_pwszOriginalVolumeName),
        device_path: text(p.m_pwszSnapshotDeviceObject),
    }
}

pub(crate) struct Session {
    backup: Backup,
    id: GUID,
    snapshots: Vec<Snapshot>,
    persistent: bool,
    metadata: bool,
    keep: bool,
    finished: bool,
}
impl Session {
    pub fn create(volumes: &[String], persistent: bool) -> Result<Self> {
        let mut normalized = Vec::new();
        for v in volumes {
            let v = normalize_volume(v)?;
            if normalized.iter().any(|p: &String| p.eq_ignore_ascii_case(&v)) {
                return Err(error("duplicate volume in snapshot set", E_INVALIDARG));
            }
            normalized.push(v);
        }
        let backup = Backup::new(if persistent { VSS_CTX_CLIENT_ACCESSIBLE.0 } else { VSS_CTX_BACKUP.0 })?;
        let mut session = Self {
            backup,
            id: GUID::zeroed(),
            snapshots: Vec::new(),
            persistent,
            metadata: false,
            keep: false,
            finished: false,
        };
        if !persistent {
            check(
                "SetBackupState",
                unsafe { (session.backup.v().backup_state)(session.backup.p(), false, false, VSS_BT_FULL, false) }.ok(),
            )?;
            session.metadata = true;
            session.backup.asynchronous("GatherWriterMetadata", session.backup.v().gather)?;
        }
        check("StartSnapshotSet", unsafe { (session.backup.v().start)(session.backup.p(), &mut session.id) }.ok())?;
        let mut ids = Vec::new();
        for v in normalized {
            let mut p = wide(&v);
            let mut id = GUID::zeroed();
            check(
                "AddToSnapshotSet",
                unsafe { (session.backup.v().add)(session.backup.p(), p.as_mut_ptr(), GUID::zeroed(), &mut id) }.ok(),
            )?;
            ids.push(id);
        }
        if !persistent {
            session.backup.asynchronous("PrepareForBackup", session.backup.v().prepare)?;
        }
        session.backup.asynchronous("DoSnapshotSet", session.backup.v().snapshot)?;
        for id in ids {
            session.snapshots.push(session.backup.properties(id)?);
        }
        if session.metadata {
            check("FreeWriterMetadata", unsafe { (session.backup.v().free_metadata)(session.backup.p()) }.ok())?;
            session.metadata = false;
        }
        session.keep = persistent;
        Ok(session)
    }
    pub fn id(&self) -> Uuid {
        uuid(self.id)
    }
    pub fn snapshots(&self) -> &[Snapshot] {
        &self.snapshots
    }
    pub fn complete(&mut self, success: bool) -> Result<()> {
        let result = if self.persistent {
            Ok(())
        } else if success {
            self.backup.asynchronous("BackupComplete", self.backup.v().complete)
        } else {
            check("AbortBackup", unsafe { (self.backup.v().abort)(self.backup.p()) }.ok())
        };
        self.finished = result.is_ok();
        if !self.persistent {
            let deleted = self.backup.delete(self.id);
            if result.is_ok() {
                return deleted;
            }
        }
        result
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        if !self.persistent && !self.finished {
            unsafe {
                let _ = (self.backup.v().abort)(self.backup.p());
            }
        }
        if self.metadata {
            unsafe {
                let _ = (self.backup.v().free_metadata)(self.backup.p());
            }
        }
        if !self.keep && self.id != GUID::zeroed() {
            let _ = self.backup.delete(self.id);
        }
    }
}
pub(crate) fn query() -> Result<Vec<(Snapshot, bool)>> {
    let backup = Backup::new(VSS_CTX_ALL.0)?;
    let mut ptr = std::ptr::null_mut();
    let hr = unsafe { (backup.v().query)(backup.p(), GUID::zeroed(), VSS_OBJECT_NONE, VSS_OBJECT_SNAPSHOT, &mut ptr) };
    let enumerator = if ptr.is_null() { None } else { Some(unsafe { IVssEnumObject::from_raw(ptr) }) };
    if hr == S_FALSE && enumerator.is_none() {
        return Ok(Vec::new());
    }
    check("Query snapshots", hr.ok())?;
    let enumerator = enumerator.ok_or_else(|| error("null VSS enumerator", E_POINTER))?;
    let mut result = Vec::new();
    loop {
        let mut objects = [VSS_OBJECT_PROP::default()];
        let mut fetched = 0;
        check("enumerate snapshots", unsafe { enumerator.Next(&mut objects, &mut fetched) })?;
        if fetched == 0 {
            break;
        }
        if objects[0].Type != VSS_OBJECT_SNAPSHOT {
            return Err(error("unexpected VSS object type", E_UNEXPECTED));
        }
        let mut prop = unsafe { objects[0].Obj.Snap };
        result.push((snapshot(&prop), prop.m_lSnapshotAttributes & VSS_VOLSNAP_ATTR_PERSISTENT.0 != 0));
        unsafe { (backup.api.free)(&mut prop) };
    }
    Ok(result)
}
pub(crate) fn delete(id: Uuid) -> Result<()> {
    Backup::new(VSS_CTX_ALL.0)?.delete(guid(id))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn abi_slot_offsets() {
        let pointer = std::mem::size_of::<usize>();
        assert_eq!(std::mem::offset_of!(BackupVtbl, gather), 9 * pointer);
        assert_eq!(std::mem::offset_of!(BackupVtbl, complete), 27 * pointer);
        assert_eq!(std::mem::offset_of!(BackupVtbl, context), 35 * pointer);
        assert_eq!(std::mem::offset_of!(BackupVtbl, query), 43 * pointer);
    }
    #[test]
    fn load_api_and_initialize_mta() {
        std::thread::spawn(|| {
            let _apartment = Apartment::new().unwrap();
            let _api = Api::load().unwrap();
        })
        .join()
        .unwrap();
    }
}
