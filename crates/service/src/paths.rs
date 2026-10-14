//! Where the service keeps its state, and the ACL protecting it.

use std::io;
use std::path::{Path, PathBuf};

use uuid::Uuid;

/// Only SYSTEM and Administrators may read or change the service's state (definitions hold
/// DPAPI blobs; logs reveal what was backed up where). The default ProgramData ACL lets
/// users create files, so the inherited ACL is replaced.
const DATA_SDDL: &str = "D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)";

#[derive(Debug, Clone)]
pub struct Paths {
    pub root: PathBuf,
}

impl Paths {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// `%ProgramData%\inocrium`.
    pub fn default_root() -> PathBuf {
        winio::program_data().join("inocrium")
    }

    pub fn definitions(&self) -> PathBuf {
        self.root.join("definitions")
    }
    pub fn cow(&self) -> PathBuf {
        self.root.join("cow")
    }
    pub fn logs(&self) -> PathBuf {
        self.root.join("logs")
    }
    pub fn settings(&self) -> PathBuf {
        self.root.join("settings.json")
    }
    pub fn credentials(&self) -> PathBuf {
        self.root.join("credentials.json")
    }
    pub fn history(&self) -> PathBuf {
        self.root.join("history.json")
    }
    pub fn vms(&self) -> PathBuf {
        self.root.join("vms.json")
    }
    pub fn job_log(&self, id: Uuid) -> PathBuf {
        self.logs().join(format!("{id}.log"))
    }

    /// Creates the directory tree and locks the root down.
    pub fn create(&self, protect: bool) -> io::Result<()> {
        std::fs::create_dir_all(&self.root)?;
        if protect {
            protect_dir(&self.root)?;
        }
        for d in [self.definitions(), self.cow(), self.logs()] {
            std::fs::create_dir_all(d)?;
        }
        Ok(())
    }
}

/// Writes `data` to `path` through a temporary file so readers never see a partial file.
pub fn write_atomic(path: &Path, data: &[u8]) -> io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, path)
}

/// Replaces the DACL of `dir` with [`DATA_SDDL`] (protected: nothing inherited from above).
fn protect_dir(dir: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::{PCWSTR, PWSTR};
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SetNamedSecurityInfoW, SDDL_REVISION_1, SE_FILE_OBJECT,
    };
    use windows::Win32::Security::{
        GetSecurityDescriptorDacl, ACL, DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION,
        PSECURITY_DESCRIPTOR,
    };

    let sddl: Vec<u16> = DATA_SDDL.encode_utf16().chain(Some(0)).collect();
    let mut path: Vec<u16> = dir.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    let os = |e: windows::core::Error| io::Error::from_raw_os_error((e.code().0 as u32 & 0xffff) as i32);
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(sddl.as_ptr()),
            SDDL_REVISION_1,
            &mut descriptor,
            None,
        )
        .map_err(os)?;
        let mut present = false.into();
        let mut defaulted = false.into();
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let result =
            GetSecurityDescriptorDacl(descriptor, &mut present, &mut dacl, &mut defaulted).map_err(os).and_then(|()| {
                let status = SetNamedSecurityInfoW(
                    PWSTR(path.as_mut_ptr()),
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                    None,
                    None,
                    Some(dacl),
                    None,
                );
                if status.is_ok() {
                    Ok(())
                } else {
                    Err(io::Error::from_raw_os_error(status.0 as i32))
                }
            });
        LocalFree(HLOCAL(descriptor.0));
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_and_atomic_write() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path().join("data"));
        paths.create(false).unwrap();
        assert!(paths.definitions().is_dir() && paths.cow().is_dir() && paths.logs().is_dir());
        write_atomic(&paths.settings(), b"{}").unwrap();
        write_atomic(&paths.settings(), b"{\"a\":1}").unwrap();
        assert_eq!(std::fs::read(paths.settings()).unwrap(), b"{\"a\":1}");
        let id = Uuid::nil();
        assert!(paths.job_log(id).ends_with(format!("{id}.log")));
    }
}
