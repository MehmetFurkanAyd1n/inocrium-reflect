//! Microsoft iSCSI initiator control. These operations usually require elevation.
use crate::{ChapSecret, Error, Result};
use std::{
    collections::HashSet,
    mem::size_of,
    net::{IpAddr, SocketAddr},
    ptr, thread,
    time::{Duration, Instant},
};
use windows::{
    core::{w, PCWSTR, PWSTR},
    Win32::{
        Foundation::{ERROR_INSUFFICIENT_BUFFER, ERROR_MORE_DATA, ERROR_SERVICE_ALREADY_RUNNING},
        Storage::IscsiDisc::*,
        System::Services::*,
    },
};

/// Serializable identifiers returned by the Microsoft initiator; contains no pointers.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Session {
    /// Adapter identifier and adapter-specific session identifier.
    pub session_id: [u64; 2],
    /// Adapter identifier and adapter-specific connection identifier.
    pub connection_id: [u64; 2],
}

fn native(id: [u64; 2]) -> ISCSI_UNIQUE_SESSION_ID {
    ISCSI_UNIQUE_SESSION_ID { AdapterUnique: id[0], AdapterSpecific: id[1] }
}
fn plain(id: ISCSI_UNIQUE_SESSION_ID) -> [u64; 2] {
    [id.AdapterUnique, id.AdapterSpecific]
}
fn check(code: u32, operation: &'static str) -> Result<()> {
    if code == 0 {
        Ok(())
    } else {
        Err(Error::Win32 { operation, code })
    }
}
fn win(error: windows::core::Error, operation: &'static str) -> Error {
    Error::Win32 { operation, code: error.code().0 as u32 & 0xffff }
}
fn resize(code: u32) -> bool {
    code == ERROR_INSUFFICIENT_BUFFER.0 || code == ERROR_MORE_DATA.0
}
fn wide(s: &str) -> Result<Vec<u16>> {
    if s.contains('\0') {
        return Err(Error::InvalidConfig("embedded NUL".into()));
    }
    Ok(s.encode_utf16().chain(Some(0)).collect())
}
fn portal(addr: SocketAddr) -> ISCSI_TARGET_PORTALW {
    let mut p = ISCSI_TARGET_PORTALW { Socket: addr.port(), ..Default::default() };
    let address: Vec<u16> = addr.ip().to_string().encode_utf16().collect();
    p.Address[..address.len()].copy_from_slice(&address);
    p
}

struct Options {
    options: ISCSI_LOGIN_OPTIONS,
    _user: Vec<u8>,
    _password: Vec<u8>,
}
impl Options {
    fn new(chap: Option<&ChapSecret>) -> Result<Self> {
        let mut options = ISCSI_LOGIN_OPTIONS {
            InformationSpecified: 0x87,
            AuthType: ISCSI_NO_AUTH_TYPE,
            HeaderDigest: ISCSI_DIGEST_TYPE_NONE,
            DataDigest: ISCSI_DIGEST_TYPE_NONE,
            MaximumConnections: 1,
            ..Default::default()
        };
        let (mut user, mut password) = match chap {
            Some(c) => {
                if c.user.is_empty()
                    || !c.user.is_ascii()
                    || c.user.contains('\0')
                    || !(12..=16).contains(&c.secret.len())
                    || !c.secret.iter().all(|b| (32..=126).contains(b))
                {
                    return Err(Error::InvalidConfig("invalid Microsoft CHAP credentials".into()));
                }
                options.AuthType = ISCSI_CHAP_AUTH_TYPE;
                options.InformationSpecified |= 0x60;
                (c.user.as_bytes().to_vec(), c.secret.clone())
            }
            None => (Vec::new(), Vec::new()),
        };
        options.UsernameLength = user.len() as u32;
        options.PasswordLength = password.len() as u32;
        if !user.is_empty() {
            options.Username = user.as_mut_ptr();
            options.Password = password.as_mut_ptr();
        }
        Ok(Self { options, _user: user, _password: password })
    }
}
impl Drop for Options {
    fn drop(&mut self) {
        self._password.fill(0);
    }
}

struct ServiceHandle(SC_HANDLE);
impl Drop for ServiceHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseServiceHandle(self.0);
        }
    }
}

/// Enable a disabled MSiSCSI service with demand start, then wait up to thirty seconds.
pub fn ensure_initiator_service() -> Result<()> {
    // Handles are owned by RAII guards; buffers are aligned and live throughout each call.
    unsafe {
        let manager = ServiceHandle(
            OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_CONNECT).map_err(|e| win(e, "OpenSCManagerW"))?,
        );
        let name = w!("MSiSCSI");
        let service = ServiceHandle(
            OpenServiceW(manager.0, name, SERVICE_QUERY_CONFIG | SERVICE_QUERY_STATUS | SERVICE_START)
                .map_err(|e| win(e, "OpenServiceW"))?,
        );
        let mut needed = 0;
        let initial = QueryServiceConfigW(service.0, None, 0, &mut needed);
        if let Err(e) = initial {
            if e.code().0 as u32 & 0xffff != ERROR_INSUFFICIENT_BUFFER.0 {
                return Err(win(e, "QueryServiceConfigW"));
            }
        }
        let mut buffer = vec![0u64; (needed as usize).div_ceil(8).max(size_of::<QUERY_SERVICE_CONFIGW>().div_ceil(8))];
        QueryServiceConfigW(service.0, Some(buffer.as_mut_ptr().cast()), (buffer.len() * 8) as u32, &mut needed)
            .map_err(|e| win(e, "QueryServiceConfigW"))?;
        let config = &*buffer.as_ptr().cast::<QUERY_SERVICE_CONFIGW>();
        if config.dwStartType == SERVICE_DISABLED {
            let change = ServiceHandle(
                OpenServiceW(manager.0, name, SERVICE_CHANGE_CONFIG)
                    .map_err(|e| win(e, "OpenServiceW(change config)"))?,
            );
            ChangeServiceConfigW(
                change.0,
                ENUM_SERVICE_TYPE(SERVICE_NO_CHANGE),
                SERVICE_DEMAND_START,
                SERVICE_ERROR(SERVICE_NO_CHANGE),
                PCWSTR::null(),
                PCWSTR::null(),
                None,
                PCWSTR::null(),
                PCWSTR::null(),
                PCWSTR::null(),
                PCWSTR::null(),
            )
            .map_err(|e| win(e, "ChangeServiceConfigW"))?;
        }
        if let Err(e) = StartServiceW(service.0, None) {
            if e.code().0 as u32 & 0xffff != ERROR_SERVICE_ALREADY_RUNNING.0 {
                return Err(win(e, "StartServiceW"));
            }
        }
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let mut status = SERVICE_STATUS_PROCESS::default();
            let bytes = std::slice::from_raw_parts_mut(
                (&mut status as *mut SERVICE_STATUS_PROCESS).cast::<u8>(),
                size_of::<SERVICE_STATUS_PROCESS>(),
            );
            QueryServiceStatusEx(service.0, SC_STATUS_PROCESS_INFO, Some(bytes), &mut needed)
                .map_err(|e| win(e, "QueryServiceStatusEx"))?;
            if status.dwCurrentState == SERVICE_RUNNING {
                return Ok(());
            }
            if status.dwCurrentState == SERVICE_STOPPED && status.dwWin32ExitCode != 0 {
                return check(status.dwWin32ExitCode, "MSiSCSI service startup");
            }
            if Instant::now() >= deadline {
                return Err(Error::Timeout("MSiSCSI service"));
            }
            thread::sleep(Duration::from_millis(100));
        }
    }
}

/// Register a SendTargets discovery portal using optional one-way CHAP credentials.
pub fn add_portal(addr: SocketAddr, chap: Option<&ChapSecret>) -> Result<()> {
    let mut p = portal(addr);
    let mut options = Options::new(chap)?;
    // All input pointers reference owned buffers until the synchronous API returns.
    unsafe {
        check(
            AddIScsiSendTargetPortalW(PCWSTR::null(), u32::MAX, &mut options.options, 0, &mut p),
            "AddIScsiSendTargetPortalW",
        )
    }
}

/// Remove a previously registered SendTargets portal.
pub fn remove_portal(addr: SocketAddr) -> Result<()> {
    let mut p = portal(addr);
    unsafe { check(RemoveIScsiSendTargetPortalW(PCWSTR::null(), u32::MAX, &mut p), "RemoveIScsiSendTargetPortalW") }
}

/// Log into a target without creating a persistent login record.
pub fn login(iqn: &str, addr: SocketAddr, chap: Option<&ChapSecret>) -> Result<Session> {
    let name = wide(iqn)?;
    let mut p = portal(addr);
    let mut options = Options::new(chap)?;
    let mut session = ISCSI_UNIQUE_SESSION_ID::default();
    let mut connection = ISCSI_UNIQUE_SESSION_ID::default();
    unsafe {
        check(
            LoginIScsiTargetW(
                PCWSTR(name.as_ptr()),
                false,
                PCWSTR::null(),
                u32::MAX,
                &mut p,
                0,
                ptr::null_mut(),
                &mut options.options,
                None,
                false,
                &mut session,
                &mut connection,
            ),
            "LoginIScsiTargetW",
        )?;
    }
    Ok(Session { session_id: plain(session), connection_id: plain(connection) })
}

/// Poll disk enumeration until physical drive numbers appear or the deadline expires.
pub fn session_disks(s: &Session, timeout: Duration) -> Result<Vec<u32>> {
    let deadline = Instant::now() + timeout;
    loop {
        let mut id = native(s.session_id);
        let mut count = 0;
        let code = unsafe { GetDevicesForIScsiSessionW(&mut id, &mut count, ptr::null_mut()) };
        if code != 0 && !resize(code) {
            check(code, "GetDevicesForIScsiSessionW")?;
        }
        if count > 0 {
            let mut devices = vec![ISCSI_DEVICE_ON_SESSIONW::default(); count as usize];
            let code = unsafe { GetDevicesForIScsiSessionW(&mut id, &mut count, devices.as_mut_ptr()) };
            if code == 0 {
                let mut disks: Vec<u32> = devices
                    .iter()
                    .take(count as usize)
                    .filter(|d| d.StorageDeviceNumber.DeviceType == 7 && d.StorageDeviceNumber.DeviceNumber != u32::MAX)
                    .map(|d| d.StorageDeviceNumber.DeviceNumber)
                    .collect();
                disks.sort_unstable();
                disks.dedup();
                if !disks.is_empty() {
                    return Ok(disks);
                }
            } else if !resize(code) {
                check(code, "GetDevicesForIScsiSessionW")?;
            }
        }
        if Instant::now() >= deadline {
            return Err(Error::Timeout("iSCSI disks"));
        }
        thread::sleep(Duration::from_millis(100).min(deadline.saturating_duration_since(Instant::now())));
    }
}

/// `ISDSC_DEVICE_BUSY_ON_SESSION`: Plug and Play could not remove a device of the session
/// because something holds it open.
const DEVICE_BUSY_ON_SESSION: u32 = 0xEFFF_0040;
/// How long [`logout`] keeps retrying while the session's device is busy.
const LOGOUT_BUSY_TIMEOUT: Duration = Duration::from_secs(15);
/// How often [`logout`] repeats a refused Plug and Play removal, in case the holder let go.
const REMOVAL_RETRY: Duration = Duration::from_secs(5);

/// Log out a session; the caller retains responsibility for removing its portal. When a disk
/// of the session is busy, its removal is requested through Plug and Play first, as "Safely
/// Remove Hardware" does, so holders of open handles are told to close them.
pub fn logout(s: &Session) -> Result<()> {
    let started = Instant::now();
    let mut removal: Option<(Instant, Option<String>)> = None;
    loop {
        let mut id = native(s.session_id);
        match unsafe { check(LogoutIScsiTarget(&mut id), "LogoutIScsiTarget") } {
            Err(Error::Win32 { code: DEVICE_BUSY_ON_SESSION, .. }) if started.elapsed() < LOGOUT_BUSY_TIMEOUT => {
                if removal.as_ref().is_none_or(|(at, _)| at.elapsed() >= REMOVAL_RETRY) {
                    removal = Some((Instant::now(), remove_session_devices(s)));
                }
                thread::sleep(Duration::from_millis(500));
            }
            Err(Error::Win32 { code: DEVICE_BUSY_ON_SESSION, .. }) => {
                let pnp = match removal.and_then(|(_, veto)| veto) {
                    Some(veto) => format!("Plug and Play removal refused: {veto}"),
                    None => "Plug and Play removal was not refused".into(),
                };
                return Err(Error::Io(std::io::Error::other(format!(
                    "LogoutIScsiTarget: a device of the session is busy ({pnp})"
                ))));
            }
            other => return other,
        }
    }
}

/// Requests Plug and Play removal of the session's devices; describes any veto.
fn remove_session_devices(s: &Session) -> Option<String> {
    use windows::Win32::Devices::DeviceAndDriverInstallation::{
        CM_Query_And_Remove_SubTreeW, PNP_VetoOutstandingOpen, CM_REMOVE_NO_RESTART, CR_SUCCESS, PNP_VETO_TYPE,
    };
    let mut id = native(s.session_id);
    let mut count = 0;
    let code = unsafe { GetDevicesForIScsiSessionW(&mut id, &mut count, ptr::null_mut()) };
    if (code != 0 && !resize(code)) || count == 0 {
        return None;
    }
    let mut devices = vec![ISCSI_DEVICE_ON_SESSIONW::default(); count as usize];
    if unsafe { GetDevicesForIScsiSessionW(&mut id, &mut count, devices.as_mut_ptr()) } != 0 {
        return None;
    }
    let mut vetoes = Vec::new();
    for d in devices.iter().take(count as usize).filter(|d| d.DeviceInstance != 0) {
        let mut veto = PNP_VETO_TYPE(0);
        let mut name = [0u16; 260];
        let r = unsafe {
            CM_Query_And_Remove_SubTreeW(d.DeviceInstance, Some(&mut veto), Some(&mut name), CM_REMOVE_NO_RESTART)
        };
        if r != CR_SUCCESS {
            // The process behind an outstanding open is recorded only in the event log
            // (Microsoft-Windows-Kernel-PnP/Configuration, event 225).
            let hint = if veto == PNP_VetoOutstandingOpen {
                "; a program holds the disk open, such as Task Manager's Performance page or a disk utility. \
                 Kernel-PnP event 225 names it"
            } else {
                ""
            };
            vetoes.push(format!(
                "disk {} was vetoed (CONFIGRET {}, veto type {}, by {:?}{hint})",
                d.StorageDeviceNumber.DeviceNumber,
                r.0,
                veto.0,
                fixed_string(&name)
            ));
        }
    }
    (!vetoes.is_empty()).then(|| vetoes.join("; "))
}

fn fixed_string(s: &[u16]) -> String {
    String::from_utf16_lossy(&s[..s.iter().position(|c| *c == 0).unwrap_or(s.len())])
}
fn loopback(address: &str, port: u16) -> Option<SocketAddr> {
    let ip = address.parse::<IpAddr>().ok()?;
    if ip == IpAddr::V4(std::net::Ipv4Addr::LOCALHOST) {
        Some(SocketAddr::new(ip, port))
    } else {
        None
    }
}

/// Log out loopback sessions matching a nonempty IQN prefix and remove their portals.
/// Cached discovery records also identify portals from crashes after session teardown.
/// Portals associated with other targets are retained; unattributable portals are retained.
pub fn cleanup_stale(iqn_prefix: &str) -> Result<()> {
    if iqn_prefix.is_empty() {
        return Err(Error::InvalidConfig("cleanup needs a nonempty IQN prefix".into()));
    }
    let mut candidates = HashSet::new();
    let mut protected = HashSet::new();
    let mut stale = Vec::new();
    // The enumeration buffer owns the embedded strings and connection arrays until copied.
    unsafe {
        let mut bytes = 0;
        let mut count = 0;
        let code = GetIScsiSessionListW(&mut bytes, &mut count, ptr::null_mut());
        if code != 0 && !resize(code) {
            check(code, "GetIScsiSessionListW")?;
        }
        if bytes > 0 {
            let mut buffer = vec![0u64; (bytes as usize).div_ceil(8)];
            check(GetIScsiSessionListW(&mut bytes, &mut count, buffer.as_mut_ptr().cast()), "GetIScsiSessionListW")?;
            for s in std::slice::from_raw_parts(buffer.as_ptr().cast::<ISCSI_SESSION_INFOW>(), count as usize) {
                let name = s
                    .TargetName
                    .to_string()
                    .map_err(|_| Error::InvalidConfig("invalid target name returned by Windows".into()))?;
                let mut addresses = Vec::new();
                for c in 0..s.ConnectionCount as usize {
                    let connection = &*s.Connections.add(c);
                    let address = connection.TargetAddress.to_string().unwrap_or_default();
                    if let Some(addr) = loopback(&address, connection.TargetSocket) {
                        addresses.push(addr);
                    }
                }
                // A session whose target died has no connections left while Windows keeps
                // trying to reconnect it; with our prefix it is still ours.
                if name.starts_with(iqn_prefix) && addresses.len() == s.ConnectionCount as usize {
                    candidates.extend(addresses);
                    stale.push(Session { session_id: plain(s.SessionId), connection_id: [0; 2] });
                } else {
                    protected.extend(addresses);
                }
            }
        }
        let mut chars = 0;
        let code = ReportIScsiTargetsW(false, &mut chars, PWSTR::null());
        if code != 0 && !resize(code) {
            check(code, "ReportIScsiTargetsW")?;
        }
        if chars > 0 {
            let mut names = vec![0u16; chars as usize];
            check(ReportIScsiTargetsW(false, &mut chars, PWSTR(names.as_mut_ptr())), "ReportIScsiTargetsW")?;
            for name in names.split(|c| *c == 0).filter(|n| !n.is_empty()) {
                let target = String::from_utf16_lossy(name);
                let name = wide(&target)?;
                let mut tag = 0;
                let mut count = 0;
                let code = ReportIScsiTargetPortalsW(
                    PCWSTR::null(),
                    PCWSTR(name.as_ptr()),
                    &mut tag,
                    &mut count,
                    ptr::null_mut(),
                );
                if code != 0 && !resize(code) {
                    check(code, "ReportIScsiTargetPortalsW")?;
                }
                if count == 0 {
                    continue;
                }
                let mut portals = vec![ISCSI_TARGET_PORTALW::default(); count as usize];
                check(
                    ReportIScsiTargetPortalsW(
                        PCWSTR::null(),
                        PCWSTR(name.as_ptr()),
                        &mut tag,
                        &mut count,
                        portals.as_mut_ptr(),
                    ),
                    "ReportIScsiTargetPortalsW",
                )?;
                for p in portals.iter().take(count as usize) {
                    if let Some(addr) = loopback(&fixed_string(&p.Address), p.Socket) {
                        if target.starts_with(iqn_prefix) {
                            candidates.insert(addr);
                        } else {
                            protected.insert(addr);
                        }
                    }
                }
            }
        }
    }
    // One session that cannot be logged out must not keep the others (and the portals) around.
    let mut first_error = None;
    for s in stale {
        if let Err(e) = logout(&s) {
            tracing::warn!("could not log out stale iSCSI session: {e}");
            first_error.get_or_insert(e);
        }
    }
    let mut count = 0;
    let code = unsafe { ReportIScsiSendTargetPortalsW(&mut count, ptr::null_mut()) };
    if code != 0 && !resize(code) {
        check(code, "ReportIScsiSendTargetPortalsW")?;
    }
    if count == 0 {
        return first_error.map_or(Ok(()), Err);
    }
    let mut portals = vec![ISCSI_TARGET_PORTAL_INFOW::default(); count as usize];
    unsafe {
        check(ReportIScsiSendTargetPortalsW(&mut count, portals.as_mut_ptr()), "ReportIScsiSendTargetPortalsW")?;
    }
    for p in portals.iter().take(count as usize) {
        if let Some(addr) = loopback(&fixed_string(&p.Address), p.Socket) {
            if candidates.contains(&addr) && !protected.contains(&addr) {
                let mut portal =
                    ISCSI_TARGET_PORTALW { Address: p.Address, SymbolicName: p.SymbolicName, Socket: p.Socket };
                unsafe {
                    check(
                        RemoveIScsiSendTargetPortalW(
                            PCWSTR(p.InitiatorName.as_ptr()),
                            p.InitiatorPortNumber,
                            &mut portal,
                        ),
                        "RemoveIScsiSendTargetPortalW",
                    )?;
                }
            }
        }
    }
    first_error.map_or(Ok(()), Err)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn plain_session_and_credentials() {
        fn send_sync<T: Send + Sync>() {}
        send_sync::<Session>();
        assert_eq!(plain(native([1, 2])), [1, 2]);
        let options = Options::new(Some(&crate::generate_chap_secret())).unwrap();
        assert_eq!(options.options.PasswordLength, 16);
        assert_eq!(options.options.InformationSpecified, 0xe7);
        assert_eq!(portal("127.0.0.1:12345".parse().unwrap()).Socket, 12345);
        assert!(wide("x\0y").is_err());
    }
}
