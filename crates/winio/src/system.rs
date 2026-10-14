use crate::{
    handle::{string, wide, Handle},
    win, Result,
};
use std::{mem::size_of, path::PathBuf};
use windows::{
    core::{w, PCWSTR, PWSTR},
    Win32::{
        Foundation::*,
        Security::*,
        System::{Registry::*, Threading::*, WindowsProgramming::GetComputerNameW},
    },
};

/// Whether the current process token is elevated.
pub fn is_elevated() -> bool {
    (|| -> Result<bool> {
        let mut h = HANDLE::default();
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut h) }
            .map_err(|e| win("open process token", e))?;
        let h = Handle(h);
        let mut e = TOKEN_ELEVATION::default();
        let mut n = 0;
        unsafe {
            GetTokenInformation(
                h.0,
                TokenElevation,
                Some((&mut e as *mut TOKEN_ELEVATION).cast()),
                size_of::<TOKEN_ELEVATION>() as u32,
                &mut n,
            )
        }
        .map_err(|e| win("token elevation", e))?;
        Ok(e.TokenIsElevated != 0)
    })()
    .unwrap_or(false)
}
/// Enable named privileges; fails if any privilege is absent from the token.
pub fn enable_privileges(names: &[&str]) -> Result<()> {
    let mut h = HANDLE::default();
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY, &mut h) }
        .map_err(|e| win("open process token", e))?;
    let h = Handle(h);
    for name in names {
        let mut luid = LUID::default();
        let name = wide(name);
        unsafe { LookupPrivilegeValueW(None, PCWSTR(name.as_ptr()), &mut luid) }
            .map_err(|e| win("lookup privilege", e))?;
        let state = TOKEN_PRIVILEGES {
            PrivilegeCount: 1,
            Privileges: [LUID_AND_ATTRIBUTES { Luid: luid, Attributes: SE_PRIVILEGE_ENABLED }],
        };
        unsafe {
            SetLastError(ERROR_SUCCESS);
            AdjustTokenPrivileges(h.0, false, Some(&state), 0, None, None)
        }
        .map_err(|e| win("enable privilege", e))?;
        let status = unsafe { GetLastError() };
        if status != ERROR_SUCCESS {
            return Err(win("enable privilege", windows::core::Error::from_hresult(status.to_hresult())));
        }
    }
    Ok(())
}
/// Local computer name.
pub fn hostname() -> String {
    let mut b = [0u16; 256];
    let mut n = b.len() as u32;
    if unsafe { GetComputerNameW(PWSTR(b.as_mut_ptr()), &mut n) }.is_ok() {
        string(&b)
    } else {
        std::env::var("COMPUTERNAME").unwrap_or_default()
    }
}
/// Running Windows directory.
pub fn windows_dir() -> PathBuf {
    std::env::var_os("SystemRoot").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
}
/// Shared application data directory.
pub fn program_data() -> PathBuf {
    std::env::var_os("ProgramData").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"))
}
pub(crate) fn registry_strings(key: &str, name: &str) -> Vec<String> {
    let key = wide(key);
    let name = wide(name);
    let mut b = vec![0u16; 32768];
    let mut n = (b.len() * 2) as u32;
    let r = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(key.as_ptr()),
            PCWSTR(name.as_ptr()),
            RRF_RT_REG_SZ | RRF_RT_REG_MULTI_SZ,
            None,
            Some(b.as_mut_ptr().cast()),
            Some(&mut n),
        )
    };
    if r != ERROR_SUCCESS {
        return Vec::new();
    }
    b.truncate(n as usize / 2);
    b.split(|&c| c == 0).filter(|s| !s.is_empty()).map(String::from_utf16_lossy).collect()
}
/// Windows edition, release and build, read from the registry.
pub fn os_version() -> String {
    let key = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion";
    let get = |name| registry_strings(key, name).into_iter().next().unwrap_or_default();
    let build = get("CurrentBuildNumber");
    let mut product = get("ProductName");
    if build.parse::<u32>().unwrap_or(0) >= 22000 && product.contains("Windows 10") {
        product = product.replace("Windows 10", "Windows 11");
    }
    let mut ubr = 0u32;
    let mut n = 4;
    unsafe {
        let _ = RegGetValueW(
            HKEY_LOCAL_MACHINE,
            w!("SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion"),
            w!("UBR"),
            RRF_RT_REG_DWORD,
            None,
            Some((&mut ubr as *mut u32).cast()),
            Some(&mut n),
        );
    }
    format!("{product} {} (10.0.{build}.{ubr})", get("DisplayVersion"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn host_metadata() {
        assert!(!hostname().is_empty());
        assert!(!os_version().is_empty());
        assert!(windows_dir().is_dir());
        assert!(program_data().is_dir());
    }
    #[test]
    fn non_elevated_session() {
        if std::env::var_os("INOCrium_ELEVATED_TESTS").is_none() {
            assert!(!is_elevated(), "set INOCrium_ELEVATED_TESTS=1 when running elevated tests");
        }
    }
    #[test]
    fn invalid_privilege_fails() {
        assert!(enable_privileges(&["InocriumNonexistentPrivilege"]).is_err());
    }
}
