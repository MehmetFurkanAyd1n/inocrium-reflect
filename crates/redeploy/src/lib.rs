//! Repairs offline Windows storage-driver and boot configuration after a restore.
//!
//! Real repairs require administrator privileges. Registry and driver changes are
//! not transactional: a later failure can leave earlier repairs applied.

use std::{ffi::OsString, path::PathBuf, process::Command};

#[cfg(windows)]
mod platform;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid redeploy options: {0}")]
    InvalidOptions(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[cfg(windows)]
    #[error("Windows API failed: {0}")]
    Windows(#[from] windows::core::Error),
    #[error("invalid Windows data: {0}")]
    InvalidData(String),
    #[error("{program} failed (exit {code:?}): {output}")]
    Command { program: String, code: Option<i32>, output: String },
    #[error("this operation requires Windows")]
    Unsupported,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Firmware {
    Uefi,
    Bios,
}

impl Firmware {
    fn argument(self) -> &'static str {
        match self {
            Self::Uefi => "UEFI",
            Self::Bios => "BIOS",
        }
    }
}

#[derive(Debug, Clone)]
pub struct WindowsInstall {
    /// Volume GUID path, including its trailing backslash.
    pub volume: String,
    pub mount_path: Option<PathBuf>,
    /// ESP (GPT) or active partition (MBR), when found on the same disk.
    pub system_volume: Option<String>,
    /// Inferred from the disk partition style, not the running host firmware.
    pub firmware: Firmware,
}

#[derive(Debug, Clone)]
pub struct RedeployOptions {
    /// Volume root containing `Windows`, e.g. `E:\`.
    pub windows_root: PathBuf,
    pub system_volume: Option<String>,
    pub firmware: Firmware,
    pub drivers: Option<PathBuf>,
    /// Explicit opt-in to DISM's `/ForceUnsigned` option.
    pub force_unsigned: bool,
    pub dry_run: bool,
}

/// The unique standard inbox storage services considered for boot enablement.
pub const STORAGE_DRIVERS: &[&str] = &[
    "storahci",
    "stornvme",
    "iaStorV",
    "iaStorAVC",
    "iaStorAV",
    "msahci",
    "pciide",
    "intelide",
    "atapi",
    "storvsc",
    "vmbus",
    "LSI_SAS",
    "LSI_SAS2i",
    "LSI_SAS3i",
    "megasas",
    "megasas2i",
    "megasas35i",
    "percsas2i",
    "percsas3i",
    "SmartSAS",
    "ql2300i",
    "ADP80XX",
    "VSTXRAID",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceState {
    pub name: String,
    pub start: Option<u32>,
    pub has_start_override: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryChange {
    pub service: String,
    pub previous_start: Option<u32>,
    pub start: u32,
    pub clear_start_override: bool,
    /// False for dry-run candidates; their existence has not been checked.
    pub applied: bool,
}

/// Plans changes to existing inbox services; ignores unknown names and duplicates.
pub fn plan_storage_changes(services: &[ServiceState]) -> Vec<RegistryChange> {
    STORAGE_DRIVERS
        .iter()
        .filter_map(|name| {
            let state = services.iter().find(|s| s.name.eq_ignore_ascii_case(name))?;
            (state.start != Some(0) || state.has_start_override).then(|| RegistryChange {
                service: (*name).to_owned(),
                previous_start: state.start,
                start: 0,
                clear_start_override: state.has_start_override,
                applied: false,
            })
        })
        .collect()
}

#[derive(Debug, Clone)]
pub struct CommandReport {
    pub program: PathBuf,
    pub arguments: Vec<OsString>,
    /// None means planned, not executed.
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug, Clone, Default)]
pub struct RedeployReport {
    pub registry_changes: Vec<RegistryChange>,
    /// Successfully installed INF paths parsed from DISM's English output, when available.
    /// The complete output is also retained in `driver_install`.
    pub drivers_added: Vec<String>,
    pub driver_install: Option<CommandReport>,
    pub bcdboot: Option<CommandReport>,
    pub bcdboot_skipped: Option<String>,
    pub dry_run: bool,
}

/// Enumerates Windows installations and the disk's boot partition without mounting volumes.
pub fn find_windows_installs(disk_number: u32) -> Result<Vec<WindowsInstall>> {
    #[cfg(windows)]
    {
        platform::find_windows_installs(disk_number)
    }
    #[cfg(not(windows))]
    {
        let _ = disk_number;
        Err(Error::Unsupported)
    }
}

fn validate_options(opts: &RedeployOptions) -> Result<()> {
    if !opts.windows_root.is_absolute() {
        return Err(Error::InvalidOptions("windows_root must be an absolute volume root".into()));
    }
    if !opts.windows_root.join("Windows/System32/config/SYSTEM").is_file() {
        return Err(Error::InvalidOptions("windows_root does not contain a Windows SYSTEM hive".into()));
    }
    if let Some(drivers) = &opts.drivers {
        if !drivers.is_absolute() || !drivers.is_dir() {
            return Err(Error::InvalidOptions("drivers must be an absolute directory".into()));
        }
    }
    if let Some(volume) = &opts.system_volume {
        let guid = volume.strip_prefix("\\\\?\\Volume{").and_then(|s| s.strip_suffix("}\\"));
        if guid.and_then(|s| uuid::Uuid::parse_str(s).ok()).is_none() {
            return Err(Error::InvalidOptions(
                "system_volume must be a volume GUID path with a trailing backslash".into(),
            ));
        }
    }
    Ok(())
}

fn native_utility(name: &str) -> PathBuf {
    let root = std::env::var_os("WINDIR").unwrap_or_else(|| "C:\\Windows".into());
    let directory = if cfg!(target_pointer_width = "32") && std::env::var_os("PROCESSOR_ARCHITEW6432").is_some() {
        "Sysnative"
    } else {
        "System32"
    };
    PathBuf::from(root).join(directory).join(name)
}

fn dism_arguments(opts: &RedeployOptions, drivers: &std::path::Path) -> Vec<OsString> {
    let mut image = OsString::from("/Image:");
    image.push(&opts.windows_root);
    let mut driver = OsString::from("/Driver:");
    driver.push(drivers);
    let mut args = vec![image, "/Add-Driver".into(), driver, "/Recurse".into(), "/English".into()];
    if opts.force_unsigned {
        args.push("/ForceUnsigned".into());
    }
    args
}

fn bcdboot_arguments(opts: &RedeployOptions, letter: &str) -> Vec<OsString> {
    vec![
        opts.windows_root.join("Windows").into_os_string(),
        "/s".into(),
        letter.into(),
        "/f".into(),
        opts.firmware.argument().into(),
    ]
}

fn installed_drivers(output: &str) -> Vec<String> {
    let mut packages = Vec::new();
    for line in output.lines() {
        let Some((prefix, outcome)) = line.rsplit_once(':') else {
            continue;
        };
        if !outcome.trim().eq_ignore_ascii_case("The driver package was successfully installed.") {
            continue;
        }
        let Some((_, path)) = prefix.split_once(" - ") else {
            continue;
        };
        let path = path.trim();
        if path.to_ascii_lowercase().ends_with(".inf")
            && !packages.iter().any(|p: &String| p.eq_ignore_ascii_case(path))
        {
            packages.push(path.to_owned());
        }
    }
    packages
}

fn execute(program: PathBuf, arguments: Vec<OsString>, dry_run: bool, log: &dyn Fn(&str)) -> Result<CommandReport> {
    log(&format!("{} {:?}{}", program.display(), arguments, if dry_run { " (dry run)" } else { "" }));
    let mut report =
        CommandReport { program, arguments, exit_code: None, stdout: String::new(), stderr: String::new() };
    if dry_run {
        return Ok(report);
    }
    let mut command = Command::new(&report.program);
    command.args(&report.arguments);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW for LocalSystem service execution.
    }
    let output = command.output()?;
    report.exit_code = output.status.code();
    report.stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    report.stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    for line in report.stdout.lines().chain(report.stderr.lines()) {
        log(line);
    }
    if !output.status.success() {
        return Err(Error::Command {
            program: report.program.display().to_string(),
            code: report.exit_code,
            output: format!("{}\n{}", report.stdout, report.stderr),
        });
    }
    Ok(report)
}

/// Repairs the SYSTEM hive, optionally injects drivers, and rebuilds boot files.
/// Dry-run never loads the hive: registry entries are conditional candidate changes.
pub fn redeploy(opts: &RedeployOptions, log: &dyn Fn(&str)) -> Result<RedeployReport> {
    validate_options(opts)?;
    let mut report = RedeployReport { dry_run: opts.dry_run, ..RedeployReport::default() };
    if opts.dry_run {
        log("Dry run: would enable SeBackupPrivilege/SeRestorePrivilege and load the offline SYSTEM hive; no changes made.");
        report.registry_changes = STORAGE_DRIVERS
            .iter()
            .map(|name| RegistryChange {
                service: (*name).into(),
                previous_start: None,
                start: 0,
                clear_start_override: true,
                applied: false,
            })
            .collect();
        for change in &report.registry_changes {
            log(&format!("Would set {} Start=0 and remove StartOverride if present", change.service));
        }
    } else {
        #[cfg(windows)]
        {
            report.registry_changes = platform::repair_hive(&opts.windows_root, log)?;
        }
        #[cfg(not(windows))]
        {
            return Err(Error::Unsupported);
        }
    }
    if let Some(drivers) = &opts.drivers {
        let result = execute(native_utility("dism.exe"), dism_arguments(opts, drivers), opts.dry_run, log)?;
        report.drivers_added = installed_drivers(&result.stdout);
        report.driver_install = Some(result);
    }
    if let Some(volume) = &opts.system_volume {
        if opts.dry_run {
            log(&format!("Would assign a free drive letter to {volume}, run bcdboot, and remove the letter"));
            report.bcdboot =
                Some(execute(native_utility("bcdboot.exe"), bcdboot_arguments(opts, "<free-letter>:"), true, log)?);
        } else {
            #[cfg(windows)]
            {
                let mut mount = platform::TemporaryMount::new(volume)?;
                report.bcdboot =
                    Some(execute(native_utility("bcdboot.exe"), bcdboot_arguments(opts, mount.letter()), false, log)?);
                mount.remove()?;
            }
            #[cfg(not(windows))]
            {
                return Err(Error::Unsupported);
            }
        }
    } else {
        let reason = "Skipping bcdboot: no system volume was supplied".to_owned();
        log(&reason);
        report.bcdboot_skipped = Some(reason);
    }
    tracing::info!(dry_run = opts.dry_run, changes = report.registry_changes.len(), "Redeploy completed");
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_plan_is_conditional_and_idempotent() {
        let states = [
            ServiceState { name: "STORAHCI".into(), start: Some(3), has_start_override: true },
            ServiceState { name: "stornvme".into(), start: Some(0), has_start_override: false },
            ServiceState { name: "vmbus".into(), start: Some(0), has_start_override: true },
            ServiceState { name: "unrelated".into(), start: Some(3), has_start_override: true },
            ServiceState { name: "storahci".into(), start: Some(4), has_start_override: false },
        ];
        let changes = plan_storage_changes(&states);
        assert_eq!(changes.len(), 2);
        assert_eq!(changes[0].previous_start, Some(3));
        assert_eq!(changes[1].service, "vmbus");
        assert!(changes.iter().all(|c| c.start == 0 && c.clear_start_override && !c.applied));
        let applied: Vec<_> = changes
            .into_iter()
            .map(|c| ServiceState { name: c.service, start: Some(c.start), has_start_override: false })
            .collect();
        assert!(plan_storage_changes(&applied).is_empty());
        assert!(plan_storage_changes(&[]).is_empty());
    }

    #[test]
    fn driver_names_are_unique() {
        let mut names: Vec<_> = STORAGE_DRIVERS.iter().map(|s| s.to_ascii_lowercase()).collect();
        let count = names.len();
        names.sort();
        names.dedup();
        assert_eq!(count, names.len());
    }

    #[test]
    fn dry_run_never_loads_or_modifies_hive() {
        let dir = tempfile::tempdir().unwrap();
        let hive = dir.path().join("Windows/System32/config/SYSTEM");
        std::fs::create_dir_all(hive.parent().unwrap()).unwrap();
        std::fs::write(&hive, b"not a registry hive").unwrap();
        let opts = RedeployOptions {
            windows_root: dir.path().into(),
            system_volume: Some("\\\\?\\Volume{f6789b89-a176-415e-9216-d04e7053533e}\\".into()),
            firmware: Firmware::Uefi,
            drivers: Some(dir.path().into()),
            force_unsigned: false,
            dry_run: true,
        };
        let report = redeploy(&opts, &|_| {}).unwrap();
        assert_eq!(std::fs::read(&hive).unwrap(), b"not a registry hive");
        assert_eq!(report.registry_changes.len(), STORAGE_DRIVERS.len());
        assert!(report.registry_changes.iter().all(|c| !c.applied));
        assert_eq!(report.bcdboot.unwrap().exit_code, None);
        assert_eq!(report.driver_install.unwrap().exit_code, None);
    }

    #[test]
    fn unsigned_drivers_require_explicit_option() {
        let mut opts = RedeployOptions {
            windows_root: PathBuf::from("E:\\"),
            system_volume: None,
            firmware: Firmware::Bios,
            drivers: None,
            force_unsigned: false,
            dry_run: true,
        };
        let driver = std::path::Path::new("C:\\drivers with spaces");
        assert!(!dism_arguments(&opts, driver).contains(&OsString::from("/ForceUnsigned")));
        opts.force_unsigned = true;
        assert!(dism_arguments(&opts, driver).contains(&OsString::from("/ForceUnsigned")));
        assert_eq!(bcdboot_arguments(&opts, "Z:")[4], "BIOS");
        assert_eq!(dism_arguments(&opts, driver)[2], "/Driver:C:\\drivers with spaces");
    }

    #[test]
    fn parses_only_successful_inf_installs() {
        let output = "Installing 1 of 3 - C:\\driver dir\\disk.inf: The driver package was successfully installed.\n\
                      Installing 2 of 3 - C:\\drivers\\bad.inf: Error - The driver package could not be installed.\n\
                      Installing 3 of 3 - C:\\DRIVER DIR\\DISK.INF: The driver package was successfully installed.";
        assert_eq!(installed_drivers(output), vec!["C:\\driver dir\\disk.inf"]);
        assert!(installed_drivers("unrecognized output").is_empty());
    }
}
