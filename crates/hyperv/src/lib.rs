//! Hyper-V pass-through VMs. Callers own the lifetime of the backing iSCSI mount.

use std::{path::PathBuf, process::Command};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid VM options: {0}")]
    InvalidOptions(String),
    #[error("could not execute Windows utility: {0}")]
    Io(#[from] std::io::Error),
    #[error("PowerShell failed (exit {code:?}): {message}")]
    PowerShell { code: Option<i32>, message: String },
    #[error("invalid PowerShell response: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Generation {
    Gen1,
    Gen2,
}

impl Generation {
    pub const fn for_partition_style(gpt: bool) -> Self {
        if gpt {
            Self::Gen2
        } else {
            Self::Gen1
        }
    }

    const fn number(self) -> u32 {
        match self {
            Self::Gen1 => 1,
            Self::Gen2 => 2,
        }
    }
}

#[derive(Debug, Clone)]
pub struct VmOptions {
    pub name: String,
    pub disk_number: u32,
    pub generation: Generation,
    pub memory_mb: u64,
    pub cpus: u32,
    pub switch: Option<String>,
    pub secure_boot: bool,
}

impl Default for VmOptions {
    fn default() -> Self {
        Self {
            name: String::new(),
            disk_number: 0,
            generation: Generation::Gen2,
            memory_mb: 4096,
            cpus: 2,
            switch: None,
            secure_boot: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmInfo {
    pub name: String,
    pub id: Uuid,
    pub generation: Generation,
    pub disk_number: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VmState {
    Off,
    Running,
    Paused,
    Saved,
    Starting,
    Stopping,
    Other(String),
}

/// Reject control characters and names longer than 100 Unicode characters.
pub fn validate_vm_name(name: &str) -> Result<()> {
    if name.trim().is_empty() || name.chars().count() > 100 || name.chars().any(char::is_control) {
        return Err(Error::InvalidOptions("name must contain 1–100 characters and no control characters".into()));
    }
    Ok(())
}

fn validate_options(opts: &VmOptions) -> Result<()> {
    validate_vm_name(&opts.name)?;
    if opts.memory_mb < 32 || opts.memory_mb.checked_mul(1024 * 1024).is_none_or(|bytes| bytes > i64::MAX as u64) {
        return Err(Error::InvalidOptions(
            "memory must be at least 32 MiB and fit in a signed 64-bit byte count".into(),
        ));
    }
    if !(1..=1024).contains(&opts.cpus) {
        return Err(Error::InvalidOptions("CPU count must be between 1 and 1024".into()));
    }
    if opts.generation == Generation::Gen1 && opts.secure_boot {
        return Err(Error::InvalidOptions("secure boot requires generation 2".into()));
    }
    if let Some(switch) = &opts.switch {
        if switch.trim().is_empty() || switch.chars().any(char::is_control) {
            return Err(Error::InvalidOptions("switch must be nonempty and have no control characters".into()));
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

const PREAMBLE: &str = r#"
$ErrorActionPreference='Stop'; $ProgressPreference='SilentlyContinue';
[Console]::OutputEncoding=[System.Text.UTF8Encoding]::new($false);
function Get-InoVM {
    $found=@(Get-VM | Where-Object { $_.Name -eq $env:INO_VM_NAME });
    if ($found.Count -ne 1) { throw 'VM name does not identify exactly one VM' };
    $found[0];
};
"#;

fn powershell_command(script: &str, vars: &[(&str, String)]) -> Command {
    let mut command = Command::new(native_utility("WindowsPowerShell/v1.0/powershell.exe"));
    command.args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command"]);
    command.arg(format!("{PREAMBLE}{script}"));
    for (key, value) in vars {
        command.env(key, value);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW: service commands have no interactive window.
    }
    command
}

fn run(script: &str, vars: &[(&str, String)]) -> Result<String> {
    let output = powershell_command(script, vars).output()?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim_start_matches('\u{feff}').trim().to_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    if !output.status.success() || !stderr.is_empty() {
        let message = if stderr.is_empty() { stdout } else { stderr };
        tracing::error!(code = ?output.status.code(), %message, "Hyper-V command failed");
        return Err(Error::PowerShell { code: output.status.code(), message });
    }
    Ok(stdout)
}

const AVAILABLE: &str = "[bool](Get-Command New-VM -ErrorAction SilentlyContinue) | ConvertTo-Json -Compress";

/// Checks whether the Hyper-V PowerShell cmdlets are installed, without elevation.
pub fn is_available() -> bool {
    run(AVAILABLE, &[]).ok().and_then(|s| serde_json::from_str::<bool>(&s).ok()).unwrap_or(false)
}

// All caller-controlled values are child environment variables, never script source.
const CREATE: &str = r#"
Import-Module Hyper-V;
$name=$env:INO_VM_NAME; $number=[uint32]$env:INO_DISK_NUMBER;
$generation=[int]$env:INO_GENERATION; $memory=[uint64]$env:INO_MEMORY_MB * 1MB;
if (Get-VM | Where-Object { $_.Name -eq $name }) { throw 'A VM with this name already exists' };
$disk=Get-Disk -Number $number;
if ($disk.IsBoot -or $disk.IsSystem) { throw 'Refusing to offline a host boot or system disk' };
if ([string]::IsNullOrEmpty([string]$disk.UniqueId)) { throw 'The disk has no stable unique ID' };
$id=[string]$disk.UniqueId; $restoreOnline=-not $disk.IsOffline; $created=$false; $offlined=$false;
try {
    if ($restoreOnline) { Set-Disk -Number $number -IsOffline $true; $offlined=$true };
    $vm=New-VM -Name $name -Generation $generation -MemoryStartupBytes $memory -NoVHD; $created=$true;
    $metadata=@{owner='inocrium-reflect';version=1;diskNumber=$number;diskUniqueId=$id;restoreOnline=$restoreOnline};
    Set-VM -VM $vm -AutomaticCheckpointsEnabled $false -Notes ('inocrium-reflect:' + ($metadata | ConvertTo-Json -Compress));
    Set-VMProcessor -VM $vm -Count ([uint32]$env:INO_CPUS);
    if ($generation -eq 1) {
        Add-VMHardDiskDrive -VM $vm -DiskNumber $number -ControllerType IDE -ControllerNumber 0 -ControllerLocation 0;
        Set-VMBios -VM $vm -StartupOrder @('IDE','CD','LegacyNetworkAdapter','Floppy');
    } else {
        Add-VMHardDiskDrive -VM $vm -DiskNumber $number -ControllerType SCSI -ControllerNumber 0 -ControllerLocation 0;
        $bootDisk=Get-VMHardDiskDrive -VM $vm | Where-Object { $_.DiskNumber -eq $number } | Select-Object -First 1;
        Set-VMFirmware -VM $vm -FirstBootDevice $bootDisk;
        if ($env:INO_SECURE_BOOT -eq 'true') {
            Set-VMFirmware -VM $vm -EnableSecureBoot On -SecureBootTemplate MicrosoftWindows;
        } else { Set-VMFirmware -VM $vm -EnableSecureBoot Off };
    };
    if ($env:INO_SWITCH) { Get-VMNetworkAdapter -VM $vm | Connect-VMNetworkAdapter -SwitchName $env:INO_SWITCH };
    @{name=$vm.Name;id=$vm.Id.ToString();generation=('Gen'+$generation);disk_number=$number} | ConvertTo-Json -Compress;
} catch {
    $failure=$_.Exception.Message; $removed=-not $created;
    if ($created) {
        try { Remove-VM -VM $vm -Force; $removed=$true } catch { $failure += '; VM cleanup: ' + $_.Exception.Message };
    };
    if ($offlined -and $removed) {
        try {
            $current=Get-Disk -Number $number;
            if ([string]$current.UniqueId -ne $id) { throw 'Disk identity changed; refusing to online it' };
            Set-Disk -Number $number -IsOffline $false;
        } catch { $failure += '; disk cleanup: ' + $_.Exception.Message };
    };
    throw $failure;
}
"#;

fn create_vars(opts: &VmOptions) -> Vec<(&'static str, String)> {
    vec![
        ("INO_VM_NAME", opts.name.clone()),
        ("INO_DISK_NUMBER", opts.disk_number.to_string()),
        ("INO_GENERATION", opts.generation.number().to_string()),
        ("INO_MEMORY_MB", opts.memory_mb.to_string()),
        ("INO_CPUS", opts.cpus.to_string()),
        ("INO_SWITCH", opts.switch.clone().unwrap_or_default()),
        ("INO_SECURE_BOOT", opts.secure_boot.to_string()),
    ]
}

/// Creates a powered-off VM and takes its backing host disk offline.
/// On failure, attempts to remove the partial VM and restore the previous disk state.
pub fn create_vm(opts: &VmOptions) -> Result<VmInfo> {
    validate_options(opts)?;
    let info = serde_json::from_str(&run(CREATE, &create_vars(opts))?)?;
    tracing::info!(name = %opts.name, disk = opts.disk_number, "Created Hyper-V VM");
    Ok(info)
}

fn named(script: &str, name: &str) -> Result<String> {
    validate_vm_name(name)?;
    run(script, &[("INO_VM_NAME", name.to_owned())])
}

/// Starts exactly the named VM.
pub fn start_vm(name: &str) -> Result<()> {
    named("Get-InoVM | Start-VM", name)?;
    Ok(())
}

/// Turns the VM off. `force` permits interruption of locked Hyper-V operations.
pub fn stop_vm(name: &str, force: bool) -> Result<()> {
    validate_vm_name(name)?;
    run(
        "Get-InoVM | Stop-VM -TurnOff -Force:($env:INO_FORCE -eq 'true') -Confirm:$false",
        &[("INO_VM_NAME", name.to_owned()), ("INO_FORCE", force.to_string())],
    )?;
    Ok(())
}

const REMOVE: &str = r#"
$vm=Get-InoVM; $metadata=$null; $disk=$null;
if ($vm.Notes -and $vm.Notes.StartsWith('inocrium-reflect:')) {
    $metadata=$vm.Notes.Substring('inocrium-reflect:'.Length) | ConvertFrom-Json;
    if ($metadata.owner -ne 'inocrium-reflect' -or $metadata.version -ne 1) { throw 'Invalid disk ownership metadata' };
    if ($metadata.restoreOnline) {
        $disk=Get-Disk -Number ([uint32]$metadata.diskNumber);
        if ([string]$disk.UniqueId -ne [string]$metadata.diskUniqueId) { throw 'Disk identity changed; refusing to online it' };
        if ($disk.IsBoot -or $disk.IsSystem) { throw 'Refusing to alter a host boot or system disk' };
    };
};
if ($vm.State -ne 'Off') { Stop-VM -VM $vm -TurnOff -Force -Confirm:$false };
Remove-VM -VM $vm -Force;
if ($disk) {
    try {
        $current=Get-Disk -Number $disk.Number;
        if ([string]$current.UniqueId -ne [string]$metadata.diskUniqueId) { throw 'Disk identity changed after VM removal' };
        Set-Disk -Number $disk.Number -IsOffline $false;
    } catch { throw ('VM removed; disk ' + $disk.Number + ' could not be brought online: ' + $_.Exception.Message) };
}
"#;

/// Stops and removes a VM. Only disks this crate took offline are brought online.
/// Backing image/iSCSI resources remain the caller's responsibility.
pub fn remove_vm(name: &str) -> Result<()> {
    named(REMOVE, name)?;
    Ok(())
}

/// Queries the VM's current state; preserves unrecognized Hyper-V state names.
pub fn vm_state(name: &str) -> Result<VmState> {
    let state: String = serde_json::from_str(&named("(Get-InoVM).State.ToString() | ConvertTo-Json -Compress", name)?)?;
    Ok(match state.as_str() {
        "Off" => VmState::Off,
        "Running" => VmState::Running,
        "Paused" => VmState::Paused,
        "Saved" => VmState::Saved,
        "Starting" => VmState::Starting,
        "Stopping" => VmState::Stopping,
        _ => VmState::Other(state),
    })
}

/// Opens VMConnect without waiting. A LocalSystem service runs in session 0;
/// call this from the interactive client to display a console to the user.
pub fn open_console(name: &str) -> Result<()> {
    validate_vm_name(name)?;
    Command::new(native_utility("vmconnect.exe")).args(["localhost", name]).spawn()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generation_and_defaults() {
        assert_eq!(Generation::for_partition_style(true), Generation::Gen2);
        assert_eq!(Generation::for_partition_style(false), Generation::Gen1);
        let opts = VmOptions::default();
        assert_eq!((opts.memory_mb, opts.cpus), (4096, 2));
    }

    #[test]
    fn validates_names_and_resources() {
        for name in ["", "  ", "bad\nname", "bad\0name", &"a".repeat(101)] {
            assert!(validate_vm_name(name).is_err());
        }
        assert!(validate_vm_name(&"é".repeat(100)).is_ok());
        let mut opts = VmOptions { name: "test".into(), ..VmOptions::default() };
        assert!(validate_options(&opts).is_ok());
        opts.memory_mb = u64::MAX;
        assert!(validate_options(&opts).is_err());
        opts.memory_mb = 4096;
        opts.cpus = 0;
        assert!(validate_options(&opts).is_err());
        opts.cpus = 2;
        opts.generation = Generation::Gen1;
        opts.secure_boot = true;
        assert!(validate_options(&opts).is_err());
    }

    #[test]
    fn untrusted_values_are_environment_data() {
        let name = "test'; throw 'injected'; # $() 🚀";
        let opts = VmOptions { name: name.into(), switch: Some(name.into()), ..VmOptions::default() };
        validate_options(&opts).unwrap();
        let vars = create_vars(&opts);
        let command = powershell_command(CREATE, &vars);
        let args: Vec<_> = command.get_args().map(|a| a.to_string_lossy()).collect();
        assert!(args.iter().all(|a| !a.contains(name)));
        assert!(command.get_envs().any(|(k, v)| k == "INO_VM_NAME" && v == Some(std::ffi::OsStr::new(name))));
        assert!(command.get_envs().any(|(k, v)| k == "INO_SWITCH" && v == Some(std::ffi::OsStr::new(name))));
    }

    #[test]
    fn parses_vm_result() {
        let info: VmInfo = serde_json::from_str(
            r#"{"name":"test","id":"f6789b89-a176-415e-9216-d04e7053533e","generation":"Gen2","disk_number":7}"#,
        )
        .unwrap();
        assert_eq!(info.disk_number, 7);
        assert_eq!(info.generation, Generation::Gen2);
    }

    #[test]
    #[ignore = "requires Windows PowerShell; validates scripts without running Hyper-V commands"]
    fn powershell_parses_scripts() {
        for script in [
            CREATE,
            REMOVE,
            AVAILABLE,
            "Get-InoVM | Start-VM",
            "Get-InoVM | Stop-VM -TurnOff -Force:($env:INO_FORCE -eq 'true') -Confirm:$false",
            "(Get-InoVM).State.ToString() | ConvertTo-Json -Compress",
        ] {
            run(
                "$tokens=$null; $errors=$null; [void][System.Management.Automation.Language.Parser]::ParseInput($env:INO_SCRIPT,[ref]$tokens,[ref]$errors); if ($errors.Count) { throw ($errors | Out-String) }",
                &[("INO_SCRIPT", format!("{PREAMBLE}{script}"))],
            ).unwrap();
        }
    }

    #[test]
    #[ignore = "requires Windows PowerShell; all Hyper-V and disk commands are mocked"]
    fn mocked_lifecycle_and_rollback() {
        const MOCKS: &str = r#"
$script:events=[System.Collections.Generic.List[string]]::new(); $script:vm=$null;
$script:disk=[pscustomobject]@{UniqueId='test-disk';Number=7;IsOffline=($env:INO_TEST_OFFLINE -eq 'true');IsBoot=$false;IsSystem=$false};
function Import-Module {}
function Get-VM { if ($script:vm) { $script:vm } }
function Get-Disk { $script:disk }
function Set-Disk { param($Number,[bool]$IsOffline); $script:disk.IsOffline=$IsOffline; if ($IsOffline) { $script:events.Add('offline') } else { $script:events.Add('online') } }
function New-VM { $script:events.Add('new'); $script:vm=[pscustomobject]@{Name=$env:INO_VM_NAME;Id=[guid]'f6789b89-a176-415e-9216-d04e7053533e';State='Off';Notes=''}; $script:vm }
function Set-VM { param($VM,$AutomaticCheckpointsEnabled,$Notes); $VM.Notes=$Notes }
function Set-VMProcessor {}
function Add-VMHardDiskDrive { param($VM,$DiskNumber,$ControllerType,$ControllerNumber,$ControllerLocation); $script:events.Add('attach:'+$ControllerType); if ($env:INO_TEST_FAIL -eq 'true') { throw 'mock attachment failure' } }
function Set-VMBios {}
function Get-VMHardDiskDrive { [pscustomobject]@{DiskNumber=7} }
function Set-VMFirmware {}
function Get-VMNetworkAdapter { [pscustomobject]@{Name='network'} }
function Connect-VMNetworkAdapter { $script:events.Add('network') }
function Stop-VM { $script:events.Add('stop'); $script:vm.State='Off' }
function Remove-VM { $script:events.Add('remove'); $script:vm=$null }
$failure=$null;
"#;
        for (offline, fail, replace, generation) in [
            (false, false, false, Generation::Gen2),
            (true, false, false, Generation::Gen1),
            (false, true, false, Generation::Gen2),
            (false, false, true, Generation::Gen2),
        ] {
            let opts = VmOptions {
                name: "restore[*]'; $() 🚀".into(),
                disk_number: 7,
                generation,
                switch: Some("switch'; $()".into()),
                ..VmOptions::default()
            };
            let mut vars = create_vars(&opts);
            vars.extend([
                ("INO_TEST_OFFLINE", offline.to_string()),
                ("INO_TEST_FAIL", fail.to_string()),
                ("INO_TEST_REPLACE", replace.to_string()),
            ]);
            let script = format!(
                "{MOCKS} try {{ $null = & {{ {CREATE} }}; if ($env:INO_TEST_REPLACE -eq 'true') {{ $script:disk.UniqueId='replacement' }}; & {{ {REMOVE} }} }} catch {{ $failure=$_.Exception.Message }}; @{{events=@($script:events.ToArray());offline=$script:disk.IsOffline;vmExists=($null -ne $script:vm);failure=$failure}} | ConvertTo-Json -Compress"
            );
            let result: serde_json::Value = serde_json::from_str(&run(&script, &vars).unwrap()).unwrap();
            if replace {
                assert_eq!(result["vmExists"], true);
                assert_eq!(result["offline"], true);
                assert!(result["failure"].as_str().unwrap().contains("Disk identity changed"));
                assert!(!result["events"].as_array().unwrap().iter().any(|v| v == "online" || v == "remove"));
            } else {
                assert_eq!(result["vmExists"], false);
                assert_eq!(result["offline"], offline);
                let events: Vec<String> = serde_json::from_value(result["events"].clone()).unwrap();
                let controller = if generation == Generation::Gen1 { "attach:IDE" } else { "attach:SCSI" };
                let expected = if offline {
                    vec!["new", controller, "network", "remove"]
                } else if fail {
                    vec!["offline", "new", controller, "remove", "online"]
                } else {
                    vec!["offline", "new", controller, "network", "remove", "online"]
                };
                assert_eq!(events, expected);
                if fail {
                    assert!(result["failure"].as_str().unwrap().contains("mock attachment failure"));
                } else {
                    assert!(result["failure"].is_null());
                }
            }
        }
    }

    #[test]
    #[ignore = "requires Hyper-V installed; read-only availability probe"]
    fn hyperv_available() {
        assert!(is_available());
    }
}
