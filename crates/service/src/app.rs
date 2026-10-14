//! Service state shared by the RPC handler, the job bodies and the scheduler.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, Weak};

use engine::win::{MountInfo, MountManager, MountRequest};
use ipc::proto::{
    self, notifications, MountId, MountParams, RpcError, BAD_PASSWORD, BUSY, INVALID_PARAMS, NOT_FOUND,
    PASSWORD_REQUIRED,
};
use ipc::server::Notifier;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::images::{self, InUse};
use crate::jobs::{Emit, Jobs};
use crate::paths::{write_atomic, Paths};
use crate::scheduler::Scheduler;
use crate::store::{internal, Store};

/// A viBoot VM and the writable mount backing it, persisted so a restarted service can remove
/// VMs whose disks went away with the previous process.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VmRecord {
    pub name: String,
    pub image: PathBuf,
    pub mount_id: String,
    pub disk_number: u32,
}

pub struct App {
    pub store: Arc<Store>,
    pub jobs: Arc<Jobs>,
    pub mounts: MountManager,
    pub in_use: Arc<InUse>,
    /// The image disk and partition each mount was requested for; the engine only knows disks.
    requested: Mutex<HashMap<String, (u32, Option<u32>)>>,
    pub(crate) vms: Mutex<Vec<VmRecord>>,
    notifier: Arc<OnceLock<Notifier>>,
    scheduler: OnceLock<Scheduler>,
}

/// Maps an engine error onto the RPC error a client can act on.
pub fn engine_error(e: engine::Error) -> RpcError {
    match e {
        engine::Error::Image(irimg::Error::PasswordRequired) => {
            RpcError::new(PASSWORD_REQUIRED, "the image is encrypted")
        }
        engine::Error::Image(irimg::Error::WrongPassword) => RpcError::new(BAD_PASSWORD, "wrong password"),
        engine::Error::Invalid(msg) => RpcError::new(INVALID_PARAMS, msg),
        e => internal(e),
    }
}

/// What a mount is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MountFor {
    /// A user browses its files: drive letters are assigned.
    User,
    /// A viBoot VM boots it.
    Vm { boot_windows: bool },
}

impl App {
    pub fn new(paths: &Paths) -> Arc<App> {
        let store = Arc::new(Store::load(paths));
        let notifier: Arc<OnceLock<Notifier>> = Arc::new(OnceLock::new());
        let cell = notifier.clone();
        let emit: Emit = Arc::new(move |method: &str, params: Value| {
            if let Some(n) = cell.get() {
                let _ = n.broadcast(method, params);
            }
        });
        let vms = std::fs::read(paths.vms()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        Arc::new(App {
            jobs: Jobs::new(store.clone(), emit),
            mounts: MountManager::new(paths.cow()),
            store,
            in_use: Arc::new(InUse::default()),
            requested: Mutex::new(HashMap::new()),
            vms: Mutex::new(vms),
            notifier,
            scheduler: OnceLock::new(),
        })
    }

    /// Removes what a previous (crashed) process left behind: viBoot VMs whose disks are gone,
    /// iSCSI sessions and copy-on-write files.
    pub fn startup_cleanup(&self) {
        let stale = std::mem::take(&mut *self.vms.lock());
        for vm in &stale {
            if let Err(e) = hyperv::remove_vm(&vm.name) {
                tracing::warn!("could not remove stale viBoot VM {}: {e}", vm.name);
            } else {
                tracing::info!("removed stale viBoot VM {}", vm.name);
            }
        }
        if !stale.is_empty() {
            self.save_vms();
        }
        for problem in self.mounts.startup_cleanup() {
            tracing::warn!("startup cleanup: {problem}");
        }
    }

    /// Connects notifications to the pipe server and starts the scheduler.
    pub fn attach(self: &Arc<Self>, notifier: Notifier) {
        let _ = self.notifier.set(notifier);
        // Weak references: the scheduler lives inside the app.
        let weak: Weak<App> = Arc::downgrade(self);
        let run_weak = weak.clone();
        let run = Arc::new(move |id: Uuid| {
            let app = run_weak.upgrade().ok_or_else(|| RpcError::new(BUSY, "the service is stopping"))?;
            let job = crate::ops::submit_definition_backup(&app, id)?;
            app.notify(notifications::DEFINITIONS_CHANGED);
            Ok(job)
        });
        let active = Arc::new(move |id: Uuid| weak.upgrade().is_some_and(|app| app.jobs.is_definition_active(id)));
        let _ = self.scheduler.set(Scheduler::start(self.store.clone(), run, active));
    }

    /// Re-evaluates schedules after definitions changed.
    pub fn wake_scheduler(&self) {
        if let Some(s) = self.scheduler.get() {
            s.wake();
        }
    }

    pub fn notify(&self, method: &str) {
        if let Some(n) = self.notifier.get() {
            let _ = n.broadcast(method, json!({}));
        }
    }

    /// Stops scheduling, cancels jobs (interrupted backups keep their checkpoint), removes
    /// viBoot VMs and unmounts everything.
    pub fn shutdown(&self) {
        if let Some(s) = self.scheduler.get() {
            s.stop();
        }
        self.jobs.shutdown();
        let vms = std::mem::take(&mut *self.vms.lock());
        for vm in vms {
            if let Err(e) = hyperv::remove_vm(&vm.name) {
                tracing::warn!("could not remove viBoot VM {}: {e}", vm.name);
            }
        }
        self.save_vms();
        for problem in self.mounts.unmount_all() {
            tracing::warn!("unmount: {problem}");
        }
    }

    pub(crate) fn save_vms(&self) {
        let vms = self.vms.lock().clone();
        let path = self.store.paths().vms();
        let result =
            serde_json::to_vec_pretty(&vms).map_err(std::io::Error::other).and_then(|b| write_atomic(&path, &b));
        if let Err(e) = result {
            tracing::warn!("could not save {}: {e}", path.display());
        }
    }

    // ---- mounts ----

    /// Mounts an image disk (or one partition of it) through the loopback iSCSI target.
    pub fn mount(&self, params: &MountParams) -> Result<proto::Mount, RpcError> {
        let info = self.mount_raw(Path::new(&params.path), params.password.as_deref(), params, MountFor::User)?;
        let mount = self.to_proto(&info);
        self.notify(notifications::MOUNTS_CHANGED);
        Ok(mount)
    }

    pub(crate) fn mount_raw(
        &self,
        path: &Path,
        password: Option<&str>,
        params: &MountParams,
        purpose: MountFor,
    ) -> Result<MountInfo, RpcError> {
        let password = images::resolve_password(&self.store, path, password)?;
        let disk = params.disk.unwrap_or(0);
        let info = self
            .mounts
            .mount(&MountRequest {
                image: path.to_path_buf(),
                password,
                writable: params.writable,
                disk: Some(disk),
                partition: params.partition,
                assign_letters: purpose == MountFor::User,
                keep_permissions: params.keep_permissions.unwrap_or(false),
                vm_disk: purpose != MountFor::User,
                boot_windows: purpose == MountFor::Vm { boot_windows: true },
            })
            .map_err(engine_error)?;
        for w in &info.warnings {
            tracing::warn!("mount {}: {w}", info.id);
        }
        for n in &info.notes {
            tracing::info!("mount {}: {n}", info.id);
        }
        tracing::info!("mounted {} disk {disk} as {}", path.display(), info.id);
        self.requested.lock().insert(info.id.clone(), (disk, params.partition));
        Ok(info)
    }

    fn to_proto(&self, info: &MountInfo) -> proto::Mount {
        let (disk, partition) = self.requested.lock().get(&info.id).copied().unwrap_or((0, None));
        proto::Mount {
            id: Uuid::parse_str(&info.id).unwrap_or_default(),
            image: info.image.to_string_lossy().into_owned(),
            disk,
            partition,
            writable: info.writable,
            disk_number: info.disks.first().copied(),
            letters: info
                .volumes
                .iter()
                .flat_map(|v| v.mount_points.iter())
                .map(|m| m.trim_end_matches(['\\', ':']).to_string())
                .filter(|m| !m.is_empty())
                .collect(),
        }
    }

    /// Mounts clients may see and unmount: those backing a viBoot VM are listed by `viboot.list`.
    pub fn list_mounts(&self) -> Vec<proto::Mount> {
        let vm_mounts: Vec<String> = self.vms.lock().iter().map(|v| v.mount_id.clone()).collect();
        self.mounts.list().iter().filter(|m| !vm_mounts.contains(&m.id)).map(|m| self.to_proto(m)).collect()
    }

    pub fn unmount(&self, id: &MountId) -> Result<(), RpcError> {
        let vm_mounts: Vec<String> = self.vms.lock().iter().map(|v| v.mount_id.clone()).collect();
        let ids: Vec<String> = match id {
            MountId::Id(id) => {
                let key = id.simple().to_string();
                if vm_mounts.contains(&key) {
                    return Err(RpcError::new(BUSY, "the mount backs a viBoot VM; stop the VM instead"));
                }
                if !self.mounts.list().iter().any(|m| m.id == key) {
                    return Err(RpcError::new(NOT_FOUND, "no such mount"));
                }
                vec![key]
            }
            MountId::All(_) => {
                self.mounts.list().into_iter().map(|m| m.id).filter(|m| !vm_mounts.contains(m)).collect()
            }
        };
        let mut first_error = None;
        for id in ids {
            if let Err(e) = self.unmount_raw(&id) {
                first_error.get_or_insert(e);
            }
        }
        self.notify(notifications::MOUNTS_CHANGED);
        first_error.map_or(Ok(()), Err)
    }

    pub(crate) fn unmount_raw(&self, id: &str) -> Result<(), RpcError> {
        let warnings = self.mounts.unmount(id).map_err(engine_error)?;
        for w in warnings {
            tracing::warn!("unmount {id}: {w}");
        }
        self.requested.lock().remove(id);
        tracing::info!("unmounted {id}");
        Ok(())
    }

    /// Image paths with an active mount (including viBoot mounts and unmounted disks that a
    /// program still holds open).
    pub fn mounted_images(&self) -> Vec<PathBuf> {
        let mut images: Vec<PathBuf> = self.mounts.list().into_iter().map(|m| m.image).collect();
        images.extend(self.mounts.releasing_images());
        images
    }
}
