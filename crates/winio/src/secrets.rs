use crate::{handle::wide, win, Error, Result};
use std::path::Path;
use windows::{
    core::PCWSTR,
    Win32::{
        Foundation::*, NetworkManagement::WNet::*, Security::Cryptography::*, Storage::FileSystem::GetDiskFreeSpaceExW,
    },
};

struct LocalBlob(CRYPT_INTEGER_BLOB);
impl Drop for LocalBlob {
    fn drop(&mut self) {
        unsafe {
            let _ = LocalFree(HLOCAL(self.0.pbData.cast()));
        }
    }
}
fn blob(data: &[u8]) -> Result<CRYPT_INTEGER_BLOB> {
    Ok(CRYPT_INTEGER_BLOB {
        cbData: data.len().try_into().map_err(|_| Error::Other("DPAPI input exceeds u32 length".into()))?,
        pbData: data.as_ptr().cast_mut(),
    })
}
fn copy_blob(blob: &LocalBlob) -> Vec<u8> {
    if blob.0.cbData == 0 {
        Vec::new()
    } else {
        unsafe { std::slice::from_raw_parts(blob.0.pbData, blob.0.cbData as usize) }.to_vec()
    }
}
/// Protect a secret using DPAPI and fixed application entropy; never displays UI.
pub fn dpapi_protect(data: &[u8], local_machine: bool) -> Result<Vec<u8>> {
    let data = blob(data)?;
    let entropy = blob(b"inocrium")?;
    let mut output = LocalBlob(CRYPT_INTEGER_BLOB::default());
    unsafe {
        CryptProtectData(
            &data,
            None,
            Some(&entropy),
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN | if local_machine { CRYPTPROTECT_LOCAL_MACHINE } else { 0 },
            &mut output.0,
        )
    }
    .map_err(|e| win("DPAPI protect", e))?;
    Ok(copy_blob(&output))
}
/// Decrypt an application DPAPI blob without displaying UI.
pub fn dpapi_unprotect(data: &[u8]) -> Result<Vec<u8>> {
    let data = blob(data)?;
    let entropy = blob(b"inocrium")?;
    let mut output = LocalBlob(CRYPT_INTEGER_BLOB::default());
    unsafe { CryptUnprotectData(&data, None, Some(&entropy), None, None, CRYPTPROTECT_UI_FORBIDDEN, &mut output.0) }
        .map_err(|e| win("DPAPI unprotect", e))?;
    Ok(copy_blob(&output))
}
fn status(context: &str, r: WIN32_ERROR) -> Result<()> {
    r.ok().map_err(|e| win(context, e))
}
/// Connect to an SMB share without assigning a drive letter; retry a credential conflict once.
pub fn connect_share(unc: &str, user: Option<&str>, password: Option<&str>) -> Result<()> {
    let mut remote = wide(unc);
    let u = user.map(wide);
    let p = password.map(wide);
    let resource = NETRESOURCEW {
        dwType: RESOURCETYPE_DISK,
        lpRemoteName: windows::core::PWSTR(remote.as_mut_ptr()),
        ..Default::default()
    };
    let connect = || unsafe {
        WNetAddConnection2W(
            &resource,
            p.as_ref().map_or(PCWSTR::null(), |p| PCWSTR(p.as_ptr())),
            u.as_ref().map_or(PCWSTR::null(), |u| PCWSTR(u.as_ptr())),
            NET_CONNECT_FLAGS(0),
        )
    };
    let mut r = connect();
    if r == ERROR_SESSION_CREDENTIAL_CONFLICT {
        disconnect_share(unc)?;
        r = connect();
    }
    status("connect SMB share", r)
}
/// Disconnect an SMB share connection without forcing open files closed.
pub fn disconnect_share(unc: &str) -> Result<()> {
    let p = wide(unc);
    let r = unsafe { WNetCancelConnection2W(PCWSTR(p.as_ptr()), NET_CONNECT_FLAGS(0), false) };
    if r == ERROR_NOT_CONNECTED {
        Ok(())
    } else {
        status("disconnect SMB share", r)
    }
}
/// UNC share root, including extended-length UNC paths.
pub fn share_root(path: &Path) -> Option<String> {
    let s = path.as_os_str().to_string_lossy().replace('/', "\\");
    let rest =
        if s.get(..8).is_some_and(|p| p.eq_ignore_ascii_case(r"\\?\UNC\")) { &s[8..] } else { s.strip_prefix(r"\\")? };
    let mut parts = rest.split('\\');
    let server = parts.next()?;
    let share = parts.next()?;
    if server.is_empty() || share.is_empty() || server == "." || server == "?" {
        return None;
    }
    Some(format!(r"\\{server}\{share}"))
}
/// Free bytes available to the caller on a filesystem containing the given path.
pub fn free_space(path: &Path) -> Result<u64> {
    let p = wide(path);
    let mut n = 0;
    unsafe { GetDiskFreeSpaceExW(PCWSTR(p.as_ptr()), Some(&mut n), None, None) }
        .map_err(|e| win("filesystem free space", e))?;
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dpapi_roundtrip() {
        for data in [&b"secret\0with bytes"[..], &b""[..]] {
            let protected = dpapi_protect(data, false).unwrap();
            assert_ne!(protected, data);
            assert_eq!(dpapi_unprotect(&protected).unwrap(), data);
        }
        assert!(dpapi_unprotect(b"invalid").is_err());
    }
    #[test]
    fn unc_roots() {
        assert_eq!(share_root(Path::new(r"\\server\share\file")), Some(r"\\server\share".into()));
        assert_eq!(share_root(Path::new(r"\\?\UNC\server\share\file")), Some(r"\\server\share".into()));
        assert!(share_root(Path::new(r"C:\file")).is_none());
        assert!(share_root(Path::new(r"\\.\PhysicalDrive0")).is_none());
    }
    #[test]
    fn local_free_space() {
        assert!(free_space(&std::env::temp_dir()).unwrap() > 0);
    }
}
