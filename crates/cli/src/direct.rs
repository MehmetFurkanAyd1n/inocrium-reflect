use crate::{
    args::{Cli, Command, DefinitionCommand},
    output,
    password::{self, Passwords},
    progress,
};
use anyhow::{bail, ensure, Context, Result};
use engine::win::{AnyImage, DiskRestoreOptions, MountManager, MountRequest, PartitionTarget, PrepareOptions};
use ipc::proto::{self, BackupMode, Definition, DiskStyle, RetentionKind};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

pub fn run(cli: &Cli, passwords: &mut Passwords, cancel: &engine::Cancel) -> Result<i32> {
    let report = |p: &engine::Progress| progress::render(p, cli.json);
    match &cli.command {
        Command::Disks => output::disks(&disks()?, cli.json)?,
        Command::Backup(args) => {
            let definition = args.definition(&disks()?, &winio::hostname(), "Backup", None, false)?;
            let password = if args.encrypt { Some(passwords.required(true)?) } else { None };
            let path = backup(&definition, None, password, &report, cancel)?;
            progress::finish(cli.json);
            output::value(&serde_json::json!({"image":path}), cli.json)?;
        }
        Command::Verify { img } => {
            let image = open_image(img, passwords)?;
            verify(&image, &report, cancel, cli.json)?;
            progress::finish(cli.json);
            output::value(&serde_json::json!({"verified":true, "image":img}), cli.json)?;
        }
        Command::Mount { img, writable, keep_permissions, disk, partition } => {
            let _ = open_image(img, passwords)?;
            cancel.check()?;
            let manager = MountManager::new(winio::program_data().join("inocrium/cow"));
            let mounted = manager.mount(&MountRequest { image: img.clone(), password: passwords.current(),
                writable: *writable, disk: *disk, partition: *partition, assign_letters: true,
                keep_permissions: *keep_permissions, ..Default::default() })?;
            for warning in &mounted.warnings { eprintln!("Warning: {warning}"); }
            let mount = proto::Mount { id: mounted.id.parse()?, image: img.to_string_lossy().into_owned(),
                disk: disk.unwrap_or(0), partition: *partition, writable: *writable,
                disk_number: mounted.disks.first().copied(),
                letters: mounted.volumes.iter().flat_map(|v| v.mount_points.clone()).collect() };
            let printed = output::mount(&mount, cli.json);
            if printed.is_ok() {
                eprintln!("Direct mount is active until Ctrl+C; writable changes are discarded at unmount.");
                while !cancel.is_cancelled() { std::thread::sleep(Duration::from_millis(200)); }
            }
            let warnings = manager.unmount(&mounted.id)?;
            for warning in warnings { eprintln!("Warning: {warning}"); }
            printed?;
        }
        Command::Unmount { .. } => bail!("Direct mounts belong to their mounting process. Press Ctrl+C in that process, or use the service to manage persistent mounts."),
        Command::Restore(args) => {
            let image = open_image(&args.img, passwords)?;
            password::confirm_restore(args.target_disk, args.yes)?;
            cancel.check()?;
            let opts = DiskRestoreOptions { extend: args.extend, ..Default::default() };
            let result = if let Some(number) = args.partition {
                let target = match args.target_partition {
                    Some(number) => PartitionTarget::Existing { disk: args.target_disk, number },
                    None => PartitionTarget::FreeSpace { disk: args.target_disk, offset: args.target_offset },
                };
                engine::win::restore_image_partition(&image, &args.img, args.disk, number, target, &opts, &report, cancel)?
            } else { engine::win::restore_image_disk(&image, &args.img, args.disk, args.target_disk, &opts, &report, cancel)? };
            progress::finish(cli.json);
            output::value(&result, cli.json)?;
        }
        Command::ExportVhdx { img, out, disk } => {
            let image = open_image(img, passwords)?;
            let bytes = engine::win::export_vhdx(&image, *disk, out, &report, cancel)?;
            progress::finish(cli.json);
            output::value(&serde_json::json!({"output":out, "bytesWritten":bytes}), cli.json)?;
        }
        Command::Redeploy { disk, drivers } => {
            cancel.check()?;
            ensure!(!winio::disk_info(*disk)?.is_system, "ReDeploy requires an offline disk; disk {disk} holds the running Windows installation");
            let installs = redeploy::find_windows_installs(*disk)?;
            ensure!(installs.len() == 1, "Expected one offline Windows installation on disk {disk}; found {}", installs.len());
            let install = &installs[0];
            let root = install.mount_path.clone().unwrap_or_else(|| PathBuf::from(&install.volume));
            let paths: Vec<Option<PathBuf>> = if drivers.is_empty() { vec![None] } else { drivers.iter().cloned().map(Some).collect() };
            let count = paths.len();
            let mut registry_changes = 0;
            let mut drivers_added = Vec::new();
            let mut boot_repair_skipped = None;
            for (index, drivers) in paths.into_iter().enumerate() {
                cancel.check()?;
                let opts = redeploy::RedeployOptions { windows_root: root.clone(),
                    system_volume: (index + 1 == count).then(|| install.system_volume.clone()).flatten(),
                    firmware: install.firmware, drivers, force_unsigned: false, dry_run: false };
                let result = redeploy::redeploy(&opts, &|line| eprintln!("{line}"))?;
                registry_changes += result.registry_changes.len();
                drivers_added.extend(result.drivers_added);
                boot_repair_skipped = result.bcdboot_skipped;
            }
            output::value(&serde_json::json!({"disk":disk, "registryChanges":registry_changes, "driversAdded":drivers_added, "bootRepairSkipped":boot_repair_skipped}), cli.json)?;
        }
        Command::Def { command } => match command {
            DefinitionCommand::List => output::value(&definitions()?, cli.json)?,
            DefinitionCommand::Run { id } => {
                let definition = read_definition(&definition_path(*id))?.def;
                ensure!(definition.id == *id, "Definition file ID mismatch");
                let password = if definition.encrypt {
                    eprintln!("Direct mode cannot decrypt the service's LocalSystem user-scope DPAPI password. Supply the image password for this run.");
                    Some(passwords.required(false)?)
                } else { None };
                let path = backup(&definition, Some(*id), password, &report, cancel)?;
                progress::finish(cli.json);
                output::value(&serde_json::json!({"image":path}), cli.json)?;
            }
            DefinitionCommand::Create { name, backup, schedule } => {
                ensure_definition_writes_allowed(ipc::client::is_service_running())?;
                ensure!(!backup.encrypt, "Creating encrypted definitions requires the service to store the password securely");
                let definition = backup.definition(&disks()?, &winio::hostname(), name, schedule.schedule()?, schedule.run_missed)?;
                prepare_definition_directory(&winio::program_data().join("inocrium"))?;
                write_definition(&definition_path(definition.id), &definition)?;
                output::value(&definition.id, cli.json)?;
            }
            DefinitionCommand::Delete { id } => {
                ensure_definition_writes_allowed(ipc::client::is_service_running())?;
                fs::remove_file(definition_path(*id)).context("Delete definition")?;
                output::value(&serde_json::json!({"deleted":id}), cli.json)?;
            }
        },
        Command::Viboot { .. } => bail!("viBoot requires the service to maintain a persistent VM and writable mount"),
        Command::Jobs { .. } | Command::History { .. } | Command::Logs { .. } => bail!("This command requires the background service"),
    }
    Ok(0)
}

fn backup(
    definition: &Definition,
    id: Option<uuid::Uuid>,
    password: Option<String>,
    report: engine::ProgressFn,
    cancel: &engine::Cancel,
) -> Result<PathBuf> {
    definition.validate()?;
    cancel.check()?;
    let current = disks()?;
    let selections = resolve_against(&definition.disks, &current)?;
    let destination = Path::new(&definition.destination);
    fs::create_dir_all(destination)?;
    let path = destination.join(engine::image_name(&definition.prefix, &chrono::Local::now()));
    ensure!(!path.exists(), "Image already exists: {}", path.display());
    let prepared = engine::win::prepare_backup(
        &selections,
        &PrepareOptions { forensic: definition.mode == BackupMode::Forensic, ..Default::default() },
    )?;
    for warning in &prepared.warnings {
        eprintln!("Warning: {warning}");
    }
    let options = engine::BackupOptions {
        writer: irimg::WriterOptions {
            compression_level: i32::from(definition.compression_level),
            password: password.clone(),
            split_size: definition.split_size,
            ..Default::default()
        },
        comment: definition.comment.clone(),
        definition_id: id.map(|id| id.to_string()),
        source: irimg::SourceInfo { hostname: winio::hostname(), os_version: winio::os_version() },
        ..Default::default()
    };
    let result = engine::backup(&path, &prepared.disks, &options, None, serde_json::Value::Null, report, cancel);
    let released = prepared.release(result.is_ok(), false);
    if result.is_err() {
        if let Err(error) = &released {
            eprintln!("Warning: releasing backup snapshots: {error}");
        }
    }
    result?;
    released?;
    if definition.verify {
        let image = AnyImage::Irimg(Arc::new(irimg::ImageReader::open(&path, password.as_deref())?));
        verify(&image, report, cancel, false)?;
    }
    cancel.check()?;
    if let Some(retention) = &definition.retention {
        let policy = match retention.kind {
            RetentionKind::KeepLast => engine::Retention::KeepLast(retention.value),
            RetentionKind::KeepDays => engine::Retention::KeepDays(retention.value),
        };
        engine::apply_retention(destination, &definition.prefix, policy, chrono::Utc::now().timestamp(), Some(&path))?;
    }
    Ok(path)
}

fn open_image(path: &Path, passwords: &mut Passwords) -> Result<AnyImage> {
    for attempt in 0..3 {
        let password = passwords.current();
        let result: Result<AnyImage> = if mrimgx::MrimgxImage::is_mrimgx(path) {
            mrimgx::MrimgxImage::open(path, password.as_deref())
                .map(|r| AnyImage::Mrimgx(Arc::new(r)))
                .map_err(Into::into)
        } else {
            irimg::ImageReader::open(path, password.as_deref())
                .map(|r| AnyImage::Irimg(Arc::new(r)))
                .map_err(Into::into)
        };
        match result {
            Ok(image) => return Ok(image),
            Err(error) => {
                let required = matches!(error.downcast_ref::<irimg::Error>(), Some(irimg::Error::PasswordRequired))
                    || matches!(error.downcast_ref::<mrimgx::Error>(), Some(mrimgx::Error::PasswordRequired));
                let wrong = matches!(error.downcast_ref::<irimg::Error>(), Some(irimg::Error::WrongPassword))
                    || matches!(error.downcast_ref::<mrimgx::Error>(), Some(mrimgx::Error::WrongPassword));
                if attempt == 2 || !(required || wrong) {
                    return Err(error);
                }
                if wrong {
                    passwords.retry()?;
                } else {
                    passwords.required(false)?;
                }
            }
        }
    }
    unreachable!()
}

fn verify(image: &AnyImage, report: engine::ProgressFn, cancel: &engine::Cancel, json: bool) -> Result<()> {
    cancel.check()?;
    match image {
        AnyImage::Irimg(reader) => {
            ensure!(engine::verify(reader, report, cancel)?.ok(), "Image verification found corrupt blocks");
        }
        AnyImage::Mrimgx(reader) => {
            let started = Instant::now();
            let mut last = Instant::now() - Duration::from_secs(1);
            let result = reader.verify(&mut |done, total| {
                if last.elapsed() >= Duration::from_millis(250) || done == total {
                    progress::render_blocks(done, total, json);
                    last = Instant::now();
                }
                !cancel.is_cancelled()
            })?;
            cancel.check()?;
            ensure!(result.ok, "{} (elapsed {:.1}s)", result.message, started.elapsed().as_secs_f64());
        }
    }
    Ok(())
}

fn same_id(a: &str, b: &str) -> bool {
    !a.is_empty()
        && a.trim_matches(|c| c == '{' || c == '}').eq_ignore_ascii_case(b.trim_matches(|c| c == '{' || c == '}'))
}

// Keep the service's identity-first resolution rules without depending on its crate.
fn resolve_against(
    selections: &[proto::DiskSelection],
    disks: &[proto::Disk],
) -> Result<Vec<engine::win::DiskSelection>> {
    ensure!(!selections.is_empty(), "no disks selected");
    let mut out: Vec<engine::win::DiskSelection> = Vec::with_capacity(selections.len());
    for sel in selections {
        let wanted = sel.disk_id.trim();
        let disk = if wanted.is_empty() {
            disks.iter().find(|d| d.number == sel.number)
        } else {
            disks.iter().find(|d| d.number == sel.number && same_id(&d.disk_id, wanted)).or_else(|| {
                let mut matches = disks.iter().filter(|d| same_id(&d.disk_id, wanted));
                matches.next().filter(|_| matches.next().is_none())
            })
        };
        let what = if wanted.is_empty() { format!("disk {}", sel.number) } else { format!("disk {wanted}") };
        let disk = disk.with_context(|| format!("{what} is not attached"))?;
        let partitions = match &sel.partitions {
            None => None,
            Some(list) => {
                ensure!(!list.is_empty(), "disk {}: empty partition selection", disk.number);
                for n in list {
                    ensure!(
                        disk.partitions.iter().any(|p| p.number == *n),
                        "disk {} has no partition {n}",
                        disk.number
                    );
                }
                let mut list = list.clone();
                list.sort_unstable();
                list.dedup();
                Some(list)
            }
        };
        ensure!(!out.iter().any(|s| s.disk == disk.number), "disk {} selected twice", disk.number);
        out.push(engine::win::DiskSelection { disk: disk.number, partitions });
    }
    Ok(out)
}

fn disks() -> Result<Vec<proto::Disk>> {
    Ok(winio::list_disks()?.into_iter().map(|d| {
        let (style, disk_id) = match d.layout.style {
            winio::LayoutStyle::Gpt { disk_id, .. } => (DiskStyle::Gpt, disk_id.to_string()),
            winio::LayoutStyle::Mbr { signature } => (DiskStyle::Mbr, format!("{signature:08X}")),
            winio::LayoutStyle::Raw => (DiskStyle::Raw, String::new()),
        };
        proto::Disk { number: d.number, model: d.model, serial: d.serial, size: d.size, sector_size: d.sector_size,
            style, disk_id, is_system: d.is_system, partitions: d.layout.partitions.into_iter().map(|p| {
                let v = p.volume.as_ref();
                let esp = matches!(&p.kind, winio::PartitionKind::Gpt { type_id, .. } if *type_id == uuid::Uuid::from_u128(0xc12a7328_f81f_11d2_ba4b_00a0c93ec93b));
                proto::Partition { number: p.number, offset: p.offset, length: p.length, type_name: format!("{:?}", p.kind),
                    file_system: v.map(|v| v.filesystem.clone()), label: v.map(|v| v.label.clone()),
                    letter: v.and_then(|v| v.drive_letters.first()).map(|l| format!("{l}:")),
                    used_bytes: v.filter(|v| v.total_bytes > 0).map(|v| v.total_bytes.saturating_sub(v.free_bytes)),
                    is_system: v.is_some_and(|v| v.is_system), is_boot: v.is_some_and(|v| v.is_windows), is_esp: esp }
            }).collect() }
    }).collect())
}

fn definition_path(id: uuid::Uuid) -> PathBuf {
    winio::program_data().join("inocrium/definitions").join(format!("{id}.json"))
}
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredDefinition {
    #[serde(flatten)]
    def: Definition,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    password_blob: Option<String>,
    #[serde(default)]
    anchor: Option<chrono::DateTime<chrono::Utc>>,
}

fn read_definition(path: &Path) -> Result<StoredDefinition> {
    let definition: StoredDefinition =
        serde_json::from_reader(fs::File::open(path).context("Open definition")?).context("Read definition")?;
    definition.def.validate()?;
    Ok(definition)
}
fn definitions() -> Result<Vec<Definition>> {
    let directory = winio::program_data().join("inocrium/definitions");
    if !directory.exists() {
        return Ok(Vec::new());
    }
    let mut definitions = Vec::new();
    for entry in fs::read_dir(directory)? {
        let path = entry?.path();
        if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("json")) {
            let stored = read_definition(&path)?;
            let mut definition = stored.def;
            definition.password_set = stored.password_blob.is_some();
            definitions.push(definition);
        }
    }
    definitions.sort_by(|a, b| a.name.cmp(&b.name).then(a.id.cmp(&b.id)));
    Ok(definitions)
}
fn write_definition(path: &Path, definition: &Definition) -> Result<()> {
    use std::io::Write;
    let parent = path.parent().context("Definition path has no parent")?;
    fs::create_dir_all(parent)?;
    let temporary = parent.join(format!("{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut file = fs::OpenOptions::new().write(true).create_new(true).open(&temporary)?;
        let stored =
            StoredDefinition { def: definition.clone(), password_blob: None, anchor: Some(chrono::Utc::now()) };
        file.write_all(&serde_json::to_vec_pretty(&stored)?)?;
        file.sync_all()?;
        drop(file);
        ensure!(!path.exists(), "Definition already exists");
        // Publishing with a hard link is atomic and cannot overwrite an existing file,
        // even if another writer creates it after the existence check. Preserve that
        // file byte-for-byte, including its passwordBlob and schedule anchor.
        fs::hard_link(&temporary, path)?;
        fs::remove_file(&temporary)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn ensure_definition_writes_allowed(service_running: bool) -> Result<()> {
    ensure!(
        !service_running,
        "The service is running; create or delete definitions through the service (omit --direct)"
    );
    Ok(())
}

fn prepare_definition_directory(root: &Path) -> Result<()> {
    fs::create_dir_all(root)?;
    protect_data_directory(root).context("Protect definition data directory")?;
    fs::create_dir_all(root.join("definitions"))?;
    Ok(())
}

// Match the service's protected ProgramData DACL before creating definition files.
fn protect_data_directory(dir: &Path) -> std::io::Result<()> {
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

    let sddl: Vec<u16> = "D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)".encode_utf16().chain(Some(0)).collect();
    let mut path: Vec<u16> = dir.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    let os = |e: windows::core::Error| std::io::Error::from_raw_os_error((e.code().0 as u32 & 0xffff) as i32);
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
                    Err(std::io::Error::from_raw_os_error(status.0 as i32))
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
    fn definition_files_are_flattened_anchored_and_atomic() {
        let directory = tempfile::tempdir().unwrap();
        let definition = crate::args::tests::definition();
        let path = directory.path().join("definitions").join(format!("{}.json", definition.id));
        write_definition(&path, &definition).unwrap();
        let original = fs::read(&path).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&original).unwrap();
        assert_eq!(value["id"], definition.id.to_string());
        assert!(value.get("passwordBlob").is_none());
        assert!(value.get("definition").is_none());
        chrono::DateTime::parse_from_rfc3339(value["anchor"].as_str().unwrap()).unwrap();
        assert_eq!(read_definition(&path).unwrap().def.id, definition.id);
        assert!(write_definition(&path, &definition).is_err());
        assert_eq!(fs::read(&path).unwrap(), original);
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
    }

    #[test]
    fn service_definition_extras_are_ignored() {
        let directory = tempfile::tempdir().unwrap();
        let mut definition = crate::args::tests::definition();
        definition.encrypt = true;
        definition.password_set = true;
        let mut value = serde_json::to_value(&definition).unwrap();
        value["passwordBlob"] = serde_json::json!("00abff");
        value["anchor"] = serde_json::json!(chrono::Utc::now().to_rfc3339());
        let path = directory.path().join("service.json");
        fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        value["futureField"] = serde_json::json!({"ignored":true});
        fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        let original = fs::read(&path).unwrap();
        let loaded = read_definition(&path).unwrap();
        assert_eq!(loaded.def.id, definition.id);
        assert!(loaded.def.encrypt);
        assert_eq!(loaded.password_blob.as_deref(), Some("00abff"));
        assert!(loaded.anchor.is_some());
        assert!(write_definition(&path, &definition).is_err());
        assert_eq!(fs::read(&path).unwrap(), original);
    }

    #[test]
    fn definition_mutations_require_the_service_to_be_stopped() {
        assert!(ensure_definition_writes_allowed(false).is_ok());
        let error = ensure_definition_writes_allowed(true).unwrap_err().to_string();
        assert!(error.contains("omit --direct"));
    }

    fn disk(number: u32, id: &str) -> proto::Disk {
        serde_json::from_value(serde_json::json!({
            "number":number, "model":"Test", "serial":"", "size":1048576,
            "sectorSize":512, "style":"mbr", "diskId":id, "isSystem":false,
            "partitions":[{"number":1, "offset":512, "length":1024, "typeName":"NTFS",
                "fileSystem":null, "label":null, "letter":null, "usedBytes":null,
                "isSystem":false, "isBoot":false, "isEsp":false}]
        }))
        .unwrap()
    }

    fn selection(number: u32, id: &str, partitions: Option<Vec<u32>>) -> proto::DiskSelection {
        proto::DiskSelection { number, disk_id: id.into(), partitions }
    }

    #[test]
    fn saved_identity_beats_number_and_normalizes_case_and_braces() {
        let disks = [disk(0, "0000ABCD"), disk(2, "0000EFAB")];
        let resolved = resolve_against(&[selection(0, " {0000efab} ", Some(vec![1, 1]))], &disks).unwrap();
        assert_eq!(resolved[0].disk, 2);
        assert_eq!(resolved[0].partitions.as_deref(), Some([1].as_slice()));
        assert_eq!(resolve_against(&[selection(0, "", None)], &disks).unwrap()[0].disk, 0);
        assert!(same_id("{ABCDEF01-2345-6789-ABCD-EF0123456789}", "abcdef01-2345-6789-abcd-ef0123456789"));
        assert!(!same_id("", ""));
        assert!(resolve_against(&[selection(0, "gone", None)], &disks).is_err());
        assert!(resolve_against(&[selection(9, "", None)], &disks).is_err());
    }

    #[test]
    fn disk_resolution_rejects_ambiguity_duplicates_and_missing_partitions() {
        let disks = [disk(0, "0000ABCD"), disk(2, "0000ABCD")];
        assert!(resolve_against(&[selection(9, "0000abcd", None)], &disks).is_err());
        // The saved number disambiguates duplicate identities, just as in the service.
        assert_eq!(resolve_against(&[selection(2, "0000abcd", None)], &disks).unwrap()[0].disk, 2);
        assert!(resolve_against(&[selection(0, "", None), selection(0, "0000ABCD", None)], &disks).is_err());
        assert!(resolve_against(&[selection(0, "", Some(vec![]))], &disks).is_err());
        assert!(resolve_against(&[selection(0, "", Some(vec![2]))], &disks).is_err());
        assert!(resolve_against(&[], &disks).is_err());
    }

    #[test]
    #[ignore = "requires an elevated Windows console and physical disk enumeration"]
    fn admin_disk_enumeration() {
        assert!(winio::is_elevated());
        assert!(!disks().unwrap().is_empty());
    }
}
