//! Job bodies: backup (with resume), verify, restore, VHDX export, ReDeploy and viBoot.
//!
//! Requests are validated before a job is queued so clients get parameter, password and
//! "not found" errors directly; the job then does the long-running work. Passwords are never
//! logged or put into job titles.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, ensure, Context};
use chrono::{Local, Utc};
use engine::win::{AnyImage, DiskRestoreOptions, PartitionTarget, PrepareOptions};
use engine::BackupOptions;
use ipc::proto::{
    notifications, BackupMode, Definition, ExportVhdxParams, JobKind, MountParams, RedeployParams, RestoreParams,
    RetentionKind, RpcError, VibootStartParams, VirtualMachine, BUSY, METHOD_NOT_FOUND, NOT_FOUND, PASSWORD_REQUIRED,
};
use irimg::Checkpoint;
use serde_json::json;
use uuid::Uuid;

use crate::app::{App, VmRecord};
use crate::disks;
use crate::images;
use crate::jobs::{JobCtx, JobOutcome, JobSpec};
use crate::store::{internal, not_found};

fn progress_of(ctx: &JobCtx) -> impl Fn(&engine::Progress) + Sync + '_ {
    move |p| ctx.progress(p)
}

// ---- backup ----

/// Queues a backup of a stored definition (manual runs and the scheduler).
pub fn submit_definition_backup(app: &Arc<App>, id: Uuid) -> Result<Uuid, RpcError> {
    let def = app.store.stored(id).ok_or_else(|| not_found("definition"))?.def;
    let password = if def.encrypt {
        Some(app.store.definition_password(id).ok_or_else(|| {
            RpcError::new(PASSWORD_REQUIRED, "the definition's password could not be read; save the definition again")
        })?)
    } else {
        None
    };
    submit_backup(app, def, password, true)
}

/// Queues a one-off backup of a definition that is not saved.
pub fn submit_adhoc_backup(app: &Arc<App>, mut def: Definition, password: Option<String>) -> Result<Uuid, RpcError> {
    crate::store::validate_definition(&mut def)?;
    def.id = Uuid::nil();
    let password = password.filter(|p| !p.is_empty());
    if def.encrypt && password.is_none() {
        return Err(RpcError::new(PASSWORD_REQUIRED, "an encrypted backup needs a password"));
    }
    let password = if def.encrypt { password } else { None };
    submit_backup(app, def, password, false)
}

fn submit_backup(app: &Arc<App>, def: Definition, password: Option<String>, stored: bool) -> Result<Uuid, RpcError> {
    let job_app = app.clone();
    app.jobs.submit(JobSpec {
        kind: JobKind::Backup,
        title: format!("Backup: {}", def.name),
        exclusive: true,
        definition: stored.then_some(def.id),
        body: Box::new(move |ctx| run_backup(&job_app, ctx, &def, password.as_deref(), stored)),
    })
}

/// Submits a resume job for every enabled definition with an interrupted backup (after a restart).
pub fn resume_interrupted(app: &Arc<App>) {
    for stored in app.store.all_stored() {
        let def = &stored.def;
        if !def.enabled {
            continue;
        }
        let dest = Path::new(def.destination.trim());
        let _ = images::connect_share(&app.store, dest);
        if find_resumable(dest, def).is_none() {
            continue;
        }
        match submit_definition_backup(app, def.id) {
            Ok(job) => tracing::info!("resuming the interrupted backup of {} ({}) as job {job}", def.name, def.id),
            Err(e) => tracing::warn!("could not resume the backup of {} ({}): {}", def.name, def.id, e.message),
        }
    }
}

struct Resumable {
    path: PathBuf,
    checkpoint: Checkpoint,
    snapshot: Option<Uuid>,
}

/// The newest interrupted image of `def` in `dest` whose checkpoint names this definition.
fn find_resumable(dest: &Path, def: &Definition) -> Option<Resumable> {
    let images = engine::find_images(dest, &def.prefix).ok()?;
    images.into_iter().rev().filter(|i| i.incomplete).find_map(|i| {
        let checkpoint = engine::load_checkpoint(&i.path).ok()??;
        let state = engine::checkpoint_caller_state(&checkpoint);
        if state["definition"].as_str() != Some(def.id.to_string().as_str()) {
            return None;
        }
        let snapshot = state["snapshot"].as_str().and_then(|s| Uuid::parse_str(s).ok());
        Some(Resumable { path: i.path, checkpoint, snapshot })
    })
}

/// Deletes every file of an unfinished image, its checkpoint included.
fn discard(path: &Path) {
    let mut files = irimg::image_files(path);
    files.push(engine::checkpoint_path(path));
    for f in files {
        match std::fs::remove_file(&f) {
            Ok(()) => tracing::info!("deleted {}", f.display()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!("could not delete {}: {e}", f.display()),
        }
    }
}

fn run_backup(
    app: &Arc<App>,
    ctx: &JobCtx,
    def: &Definition,
    password: Option<&str>,
    stored: bool,
) -> anyhow::Result<JobOutcome> {
    let dest = PathBuf::from(def.destination.trim());
    if let Err(e) = images::connect_share(&app.store, &dest) {
        ctx.log(&format!("warning: connecting to the destination share: {}", e.message));
    }
    ctx.phase("preparing", "resolving disks");
    let selections = disks::resolve(&def.disks)?;
    std::fs::create_dir_all(&dest).with_context(|| format!("cannot create {}", dest.display()))?;

    let resume = if stored { find_resumable(&dest, def) } else { None };
    let (path, outcome) = match resume {
        Some(r) => {
            ctx.log(&format!("resuming the interrupted backup {}", r.path.display()));
            let stale = r.path.clone();
            match backup_once(app, ctx, def, password, &selections, &dest, Some(r), stored) {
                Err(engine::Error::Invalid(msg)) => {
                    ctx.log(&format!("cannot resume ({msg}); starting a new backup"));
                    discard(&stale);
                    backup_once(app, ctx, def, password, &selections, &dest, None, stored)?
                }
                other => other?,
            }
        }
        None => backup_once(app, ctx, def, password, &selections, &dest, None, stored)?,
    };
    ctx.log(&format!("image written: {} ({} bytes)", path.display(), outcome.summary.total_bytes));

    if def.verify {
        ctx.phase("verifying", &path.display().to_string());
        let reader = irimg::ImageReader::open(&path, password).context("opening the new image")?;
        let _guard = app.in_use.acquire(&path);
        let report = engine::verify(&reader, &progress_of(ctx), &ctx.cancel)?;
        ensure!(report.ok(), "verification found {} corrupt blocks", report.bad_blocks.len());
        ctx.log(&format!("verified {} blocks", report.blocks_checked));
    }
    ctx.cancel.check()?;
    if let Some(retention) = &def.retention {
        ctx.phase("retention", "deleting expired images");
        let policy = match retention.kind {
            RetentionKind::KeepLast => engine::Retention::KeepLast(retention.value),
            RetentionKind::KeepDays => engine::Retention::KeepDays(retention.value),
        };
        let mounted = app.mounted_images();
        let candidates = engine::find_images(&dest, &def.prefix)?;
        let busy: Vec<&Path> = candidates
            .iter()
            .map(|i| i.path.as_path())
            .filter(|p| app.in_use.contains(p) || mounted.iter().any(|m| images::path_key(m) == images::path_key(p)))
            .collect();
        if busy.is_empty() {
            for deleted in engine::apply_retention(&dest, &def.prefix, policy, Utc::now().timestamp(), Some(&path))? {
                ctx.log(&format!("retention deleted {}", deleted.display()));
            }
        } else {
            ctx.log("retention skipped: an image of this definition is mounted or in use");
        }
    }
    Ok(JobOutcome { bytes: outcome.summary.total_bytes })
}

#[allow(clippy::too_many_arguments)]
fn backup_once(
    app: &Arc<App>,
    ctx: &JobCtx,
    def: &Definition,
    password: Option<&str>,
    selections: &[engine::win::DiskSelection],
    dest: &Path,
    resume: Option<Resumable>,
    stored: bool,
) -> engine::Result<(PathBuf, engine::BackupOutcome)> {
    let resuming = resume.is_some();
    let (path, checkpoint, snapshot) = match resume {
        Some(r) => (r.path, Some(r.checkpoint), r.snapshot),
        None => (dest.join(engine::image_name(&def.prefix, &Local::now())), None, None),
    };
    if !resuming && path.exists() {
        return Err(engine::Error::Invalid(format!("{} already exists", path.display())));
    }
    ctx.phase("snapshot", "creating volume shadow copies");
    let prepared = engine::win::prepare_backup(
        selections,
        &PrepareOptions {
            forensic: def.mode == BackupMode::Forensic,
            persistent_snapshot: stored,
            resume_snapshot: if resuming { snapshot } else { None },
        },
    )?;
    for w in &prepared.warnings {
        ctx.log(&format!("warning: {w}"));
    }
    let snapshot_id = prepared.shadow.as_ref().map(|s| s.set_id());
    let settings = app.store.settings();
    let options = BackupOptions {
        writer: irimg::WriterOptions {
            compression_level: i32::from(def.compression_level),
            password: password.map(String::from),
            split_size: def.split_size,
            ..Default::default()
        },
        comment: def.comment.clone(),
        definition_id: stored.then(|| def.id.to_string()),
        source: irimg::SourceInfo { hostname: winio::hostname(), os_version: winio::os_version() },
        threads: settings.threads as usize,
        ..Default::default()
    };
    let caller = json!({
        "definition": def.id.to_string(),
        "snapshot": snapshot_id.map(|s| s.to_string()),
    });
    let result = {
        let _guard = app.in_use.acquire(&path);
        engine::backup(&path, &prepared.disks, &options, checkpoint.as_ref(), caller, &progress_of(ctx), &ctx.cancel)
    };
    // A stale resume (the sources or snapshot changed) starts over, so its snapshot goes too.
    let stale = resuming && matches!(result, Err(engine::Error::Invalid(_)));
    let keep = stored && !stale && result.is_err() && engine::checkpoint_path(&path).exists();
    if let Err(e) = prepared.release(result.is_ok(), keep) {
        ctx.log(&format!("warning: releasing the snapshot: {e}"));
    }
    match result {
        Ok(outcome) => Ok((path, outcome)),
        Err(e) => {
            if keep {
                ctx.log("the backup can be resumed: run the definition again");
            } else if !stale {
                discard(&path);
            }
            Err(e)
        }
    }
}

// ---- verify ----

pub fn submit_verify(app: &Arc<App>, path: &str, password: Option<&str>) -> Result<Uuid, RpcError> {
    let path = PathBuf::from(path);
    let image = images::open(&app.store, &path, password)?;
    let job_app = app.clone();
    app.jobs.submit(JobSpec {
        kind: JobKind::Verify,
        title: format!("Verify: {}", file_name(&path)),
        exclusive: false,
        definition: None,
        body: Box::new(move |ctx| {
            let _guard = job_app.in_use.acquire(&path);
            ctx.phase("verifying", &path.display().to_string());
            verify_image(ctx, &image)?;
            ctx.log("the image is intact");
            Ok(JobOutcome { bytes: 0 })
        }),
    })
}

fn verify_image(ctx: &JobCtx, image: &AnyImage) -> anyhow::Result<()> {
    match image {
        AnyImage::Irimg(reader) => {
            let report = engine::verify(reader, &progress_of(ctx), &ctx.cancel)?;
            ensure!(report.ok(), "verification found {} corrupt blocks", report.bad_blocks.len());
            ctx.log(&format!("verified {} blocks ({} bytes)", report.blocks_checked, report.bytes_checked));
        }
        AnyImage::Mrimgx(reader) => {
            let started = std::time::Instant::now();
            let result = reader.verify(&mut |done, total| {
                let rate = (done as f64 / started.elapsed().as_secs_f64().max(0.001)) as u64;
                ctx.progress(&engine::Progress {
                    phase: "verifying".into(),
                    detail: "blocks".into(),
                    bytes_done: done,
                    bytes_total: total,
                    bytes_written: 0,
                    rate,
                });
                !ctx.cancel.is_cancelled()
            })?;
            ctx.cancel.check()?;
            ensure!(result.ok, "{}", result.message);
        }
    }
    Ok(())
}

// ---- restore and export ----

pub fn submit_restore(app: &Arc<App>, p: RestoreParams) -> Result<Uuid, RpcError> {
    let target = match p.partition {
        None if p.target_partition.is_some() || p.target_offset.is_some() => {
            return Err(RpcError::invalid_params("a whole-disk restore takes no target partition or offset"));
        }
        None => None,
        Some(_) if p.target_partition.is_some() && p.target_offset.is_some() => {
            return Err(RpcError::invalid_params("give either a target partition or a target offset, not both"));
        }
        Some(_) => Some(match p.target_partition {
            Some(number) => PartitionTarget::Existing { disk: p.target_disk, number },
            None => PartitionTarget::FreeSpace { disk: p.target_disk, offset: p.target_offset },
        }),
    };
    let path = PathBuf::from(&p.path);
    let image = images::open(&app.store, &path, p.password.as_deref())?;
    if image.disk(p.disk).is_none() {
        return Err(RpcError::new(NOT_FOUND, format!("the image has no disk {}", p.disk)));
    }
    if app.vms.lock().iter().any(|v| v.disk_number == p.target_disk) {
        return Err(RpcError::new(BUSY, format!("disk {} is attached to a viBoot VM", p.target_disk)));
    }
    let title = match p.partition {
        Some(n) => format!("Restore: {} partition {n} to disk {}", file_name(&path), p.target_disk),
        None => format!("Restore: {} to disk {}", file_name(&path), p.target_disk),
    };
    let job_app = app.clone();
    app.jobs.submit(JobSpec {
        kind: JobKind::Restore,
        title,
        exclusive: true,
        definition: None,
        body: Box::new(move |ctx| {
            let _guard = job_app.in_use.acquire(&path);
            let threads = job_app.store.settings().threads as usize;
            let opts = DiskRestoreOptions { extend: p.extend, threads, ..Default::default() };
            let progress = progress_of(ctx);
            let report = match (p.partition, target) {
                (Some(number), Some(target)) => engine::win::restore_image_partition(
                    &image,
                    &path,
                    p.disk,
                    number,
                    target,
                    &opts,
                    &progress,
                    &ctx.cancel,
                )?,
                _ => engine::win::restore_image_disk(
                    &image,
                    &path,
                    p.disk,
                    p.target_disk,
                    &opts,
                    &progress,
                    &ctx.cancel,
                )?,
            };
            for w in &report.warnings {
                ctx.log(&format!("warning: {w}"));
            }
            ctx.log(&format!("restore report: {}", serde_json::to_string(&report).unwrap_or_default()));
            Ok(JobOutcome { bytes: report.bytes_written })
        }),
    })
}

pub fn submit_export(app: &Arc<App>, p: ExportVhdxParams) -> Result<Uuid, RpcError> {
    let out = PathBuf::from(p.output.trim());
    if !out.is_absolute() || !out.extension().is_some_and(|e| e.eq_ignore_ascii_case("vhdx")) {
        return Err(RpcError::invalid_params("the output must be an absolute path ending in .vhdx"));
    }
    let _ = images::connect_share(&app.store, &out);
    if out.exists() {
        return Err(RpcError::invalid_params(format!("{} already exists", out.display())));
    }
    let path = PathBuf::from(&p.path);
    let image = images::open(&app.store, &path, p.password.as_deref())?;
    if image.disk(p.disk).is_none() {
        return Err(RpcError::new(NOT_FOUND, format!("the image has no disk {}", p.disk)));
    }
    let job_app = app.clone();
    app.jobs.submit(JobSpec {
        kind: JobKind::ExportVhdx,
        title: format!("Export to VHDX: {}", file_name(&out)),
        exclusive: false,
        definition: None,
        body: Box::new(move |ctx| {
            let _guard = job_app.in_use.acquire(&path);
            if let Some(dir) = out.parent() {
                std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
            }
            let result = engine::win::export_vhdx(&image, p.disk, &out, &progress_of(ctx), &ctx.cancel);
            match result {
                Ok(bytes) => {
                    ctx.log(&format!("exported {} bytes to {}", bytes, out.display()));
                    Ok(JobOutcome { bytes })
                }
                Err(e) => {
                    let _ = std::fs::remove_file(&out);
                    Err(e.into())
                }
            }
        }),
    })
}

// ---- ReDeploy ----

pub fn submit_redeploy(app: &Arc<App>, p: RedeployParams) -> Result<Uuid, RpcError> {
    let drivers: Vec<PathBuf> = p.drivers.unwrap_or_default().iter().map(|d| PathBuf::from(d.trim())).collect();
    if let Some(d) = drivers.iter().find(|d| !d.is_absolute()) {
        return Err(RpcError::invalid_params(format!("driver path {} is not absolute", d.display())));
    }
    let info = winio::disk_info(p.disk).map_err(|e| RpcError::new(NOT_FOUND, format!("disk {}: {e}", p.disk)))?;
    if info.is_system {
        return Err(RpcError::invalid_params(format!(
            "disk {} holds the running Windows installation; ReDeploy works on restored, offline systems",
            p.disk
        )));
    }
    if app.vms.lock().iter().any(|v| v.disk_number == p.disk) {
        return Err(RpcError::new(BUSY, format!("disk {} is attached to a viBoot VM", p.disk)));
    }
    let disk = p.disk;
    app.jobs.submit(JobSpec {
        kind: JobKind::Redeploy,
        title: format!("ReDeploy: disk {disk}"),
        exclusive: true,
        definition: None,
        body: Box::new(move |ctx| {
            ctx.phase("redeploy", "looking for Windows");
            let installs = redeploy::find_windows_installs(disk)?;
            ensure!(installs.len() == 1, "expected one Windows installation on disk {disk}, found {}", installs.len());
            let install = &installs[0];
            let root = install.mount_path.clone().unwrap_or_else(|| PathBuf::from(&install.volume));
            ctx.log(&format!("Windows found at {}", root.display()));
            let passes: Vec<Option<PathBuf>> =
                if drivers.is_empty() { vec![None] } else { drivers.into_iter().map(Some).collect() };
            let count = passes.len();
            let mut registry_changes = 0;
            for (index, drivers) in passes.into_iter().enumerate() {
                ctx.cancel.check()?;
                if let Some(d) = &drivers {
                    ctx.phase("redeploy", &format!("adding drivers from {}", d.display()));
                }
                let last = index + 1 == count;
                let opts = redeploy::RedeployOptions {
                    windows_root: root.clone(),
                    system_volume: if last { install.system_volume.clone() } else { None },
                    firmware: install.firmware,
                    drivers,
                    force_unsigned: false,
                    dry_run: false,
                };
                let report = redeploy::redeploy(&opts, &|line| ctx.log(line))?;
                registry_changes += report.registry_changes.len();
                for d in &report.drivers_added {
                    ctx.log(&format!("driver added: {d}"));
                }
                if let Some(why) = &report.bcdboot_skipped {
                    if last {
                        ctx.log(&format!("boot files not rebuilt: {why}"));
                    }
                }
            }
            ctx.log(&format!("{registry_changes} registry changes applied"));
            Ok(JobOutcome { bytes: 0 })
        }),
    })
}

// ---- viBoot ----

pub fn submit_viboot(app: &Arc<App>, p: VibootStartParams) -> Result<Uuid, RpcError> {
    if !hyperv::is_available() {
        return Err(RpcError::new(METHOD_NOT_FOUND, "Hyper-V is not available on this machine"));
    }
    let path = PathBuf::from(&p.path);
    let password = images::resolve_password(&app.store, &path, p.password.as_deref())?;
    let image = images::open(&app.store, &path, password.as_deref())?;
    let geometry =
        image.disk(p.disk).ok_or_else(|| RpcError::new(NOT_FOUND, format!("the image has no disk {}", p.disk)))?;
    drop(image);
    let name = p
        .vm_name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(String::from)
        .unwrap_or_else(|| format!("inocrium {} {}", file_stem(&path), Local::now().format("%Y%m%d-%H%M%S")));
    hyperv::validate_vm_name(&name).map_err(|e| RpcError::invalid_params(e.to_string()))?;
    if app.vms.lock().iter().any(|v| v.name.eq_ignore_ascii_case(&name)) {
        return Err(RpcError::new(BUSY, format!("a viBoot VM named {name} already exists")));
    }
    let job_app = app.clone();
    app.jobs.submit(JobSpec {
        kind: JobKind::Viboot,
        title: format!("viBoot: {name}"),
        exclusive: false,
        definition: None,
        body: Box::new(move |ctx| {
            ctx.phase("mounting", "attaching the image writable (changes are discarded)");
            let params = MountParams {
                path: p.path.clone(),
                password: None,
                disk: Some(p.disk),
                partition: None,
                writable: true,
            };
            let mount =
                job_app.mount_raw(&path, password.as_deref(), &params, false).map_err(|e| anyhow!(e.message))?;
            let Some(&disk_number) = mount.disks.first() else {
                let _ = job_app.unmount_raw(&mount.id);
                return Err(anyhow!("the mounted image disk did not appear"));
            };
            let cleanup = |e: anyhow::Error| {
                let _ = job_app.unmount_raw(&mount.id);
                job_app.notify(notifications::MOUNTS_CHANGED);
                e
            };
            ctx.cancel.check().map_err(|e| cleanup(e.into()))?;
            ctx.phase("creating VM", &name);
            let generation = hyperv::Generation::for_partition_style(geometry.gpt);
            let opts = hyperv::VmOptions {
                name: name.clone(),
                disk_number,
                generation,
                memory_mb: u64::from(p.memory_mb.max(512)),
                cpus: p.cpus.max(1),
                ..Default::default()
            };
            hyperv::create_vm(&opts).map_err(|e| cleanup(e.into()))?;
            job_app.vms.lock().push(VmRecord {
                name: name.clone(),
                image: path.clone(),
                mount_id: mount.id.clone(),
                disk_number,
            });
            job_app.save_vms();
            ctx.log(&format!("created {generation:?} VM {name} on disk {disk_number}"));
            ctx.phase("starting VM", &name);
            if let Err(e) = hyperv::start_vm(&name) {
                let _ = stop_viboot(&job_app, &name);
                return Err(e.into());
            }
            ctx.log("the VM is running; stop it from viBoot to discard its changes");
            Ok(JobOutcome { bytes: 0 })
        }),
    })
}

pub fn list_viboot(app: &App) -> Vec<VirtualMachine> {
    let vms = app.vms.lock().clone();
    vms.into_iter()
        .map(|v| {
            let state = match hyperv::vm_state(&v.name) {
                Ok(hyperv::VmState::Off) => "off".to_string(),
                Ok(hyperv::VmState::Running) => "running".into(),
                Ok(hyperv::VmState::Paused) => "paused".into(),
                Ok(hyperv::VmState::Saved) => "saved".into(),
                Ok(hyperv::VmState::Starting) => "starting".into(),
                Ok(hyperv::VmState::Stopping) => "stopping".into(),
                Ok(hyperv::VmState::Other(s)) => s.to_lowercase(),
                Err(_) => "unknown".into(),
            };
            VirtualMachine { name: v.name, state, image: v.image.to_string_lossy().into_owned() }
        })
        .collect()
}

/// Turns the VM off, removes it and discards its writable mount.
pub fn stop_viboot(app: &App, name: &str) -> Result<(), RpcError> {
    let record = app
        .vms
        .lock()
        .iter()
        .find(|v| v.name.eq_ignore_ascii_case(name))
        .cloned()
        .ok_or_else(|| not_found("viBoot VM"))?;
    hyperv::remove_vm(&record.name).map_err(internal)?;
    app.vms.lock().retain(|v| v.name != record.name);
    app.save_vms();
    let result = app.unmount_raw(&record.mount_id);
    app.notify(notifications::MOUNTS_CHANGED);
    result
}

fn file_name(path: &Path) -> String {
    path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned())
}

fn file_stem(path: &Path) -> String {
    path.file_stem().map_or_else(|| "image".into(), |n| n.to_string_lossy().into_owned())
}
