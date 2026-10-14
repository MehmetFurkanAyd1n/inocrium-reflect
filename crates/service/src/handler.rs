//! The JSON-RPC methods. Long operations are queued as jobs (see `ops`); the rest answer
//! directly.

use std::path::Path;
use std::sync::Arc;

use chrono::Utc;
use ipc::proto::*;
use ipc::server::ClientInfo;
use uuid::Uuid;

use crate::app::App;
use crate::store::not_found;
use crate::{disks, images, ops};

pub struct Handler(pub Arc<App>);

impl Handler {
    fn definitions_changed(&self) {
        self.0.wake_scheduler();
        self.0.notify(notifications::DEFINITIONS_CHANGED);
    }
}

impl Service for Handler {
    fn service_info(&self, _: EmptyParams, _: &ClientInfo) -> Result<ServiceInfo, RpcError> {
        Ok(ServiceInfo {
            version: env!("CARGO_PKG_VERSION").into(),
            protocol_version: PROTOCOL_VERSION,
            machine: winio::hostname(),
        })
    }

    fn disks_list(&self, _: EmptyParams, _: &ClientInfo) -> Result<Vec<Disk>, RpcError> {
        let mounted: Vec<u32> = self.0.mounts.list().iter().flat_map(|m| m.disks.clone()).collect();
        disks::list(&mounted)
    }

    fn definitions_list(&self, _: EmptyParams, _: &ClientInfo) -> Result<Vec<Definition>, RpcError> {
        Ok(self.0.store.definitions())
    }

    fn definitions_get(&self, p: IdParams, _: &ClientInfo) -> Result<Definition, RpcError> {
        self.0.store.definition(p.id)
    }

    fn definitions_save(&self, p: SaveDefinitionParams, _: &ClientInfo) -> Result<Uuid, RpcError> {
        let id = self.0.store.save_definition(p.definition, p.password)?;
        tracing::info!("definition {id} saved");
        self.definitions_changed();
        Ok(id)
    }

    fn definitions_delete(&self, p: IdParams, _: &ClientInfo) -> Result<(), RpcError> {
        if self.0.jobs.is_definition_active(p.id) {
            return Err(RpcError::new(BUSY, "the definition is running; cancel its job first"));
        }
        self.0.store.delete_definition(p.id)?;
        tracing::info!("definition {} deleted", p.id);
        self.definitions_changed();
        Ok(())
    }

    fn definitions_run(&self, p: IdParams, _: &ClientInfo) -> Result<Uuid, RpcError> {
        let job = ops::submit_definition_backup(&self.0, p.id)?;
        self.0.store.mark_run(p.id, Utc::now(), false);
        self.0.notify(notifications::DEFINITIONS_CHANGED);
        Ok(job)
    }

    fn backup_start(&self, p: BackupStartParams, _: &ClientInfo) -> Result<Uuid, RpcError> {
        ops::submit_adhoc_backup(&self.0, p.definition, p.password)
    }

    fn jobs_list(&self, _: EmptyParams, _: &ClientInfo) -> Result<Vec<Job>, RpcError> {
        Ok(self.0.jobs.list())
    }

    fn jobs_cancel(&self, p: IdParams, _: &ClientInfo) -> Result<(), RpcError> {
        self.0.jobs.cancel(p.id)
    }

    fn images_list(&self, p: ImagesListParams, _: &ClientInfo) -> Result<Vec<ImageSummary>, RpcError> {
        images::list(&self.0.store, p.folders)
    }

    fn images_inspect(&self, p: ImagePasswordParams, _: &ClientInfo) -> Result<ImageDetails, RpcError> {
        images::inspect(&self.0.store, Path::new(&p.path), p.password.as_deref())
    }

    fn images_verify(&self, p: ImagePasswordParams, _: &ClientInfo) -> Result<Uuid, RpcError> {
        ops::submit_verify(&self.0, &p.path, p.password.as_deref())
    }

    fn images_delete(&self, p: PathParams, _: &ClientInfo) -> Result<(), RpcError> {
        let mounted = self.0.mounted_images();
        images::delete(&self.0.store, Path::new(&p.path), &self.0.in_use, &mounted)
    }

    fn mounts_list(&self, _: EmptyParams, _: &ClientInfo) -> Result<Vec<Mount>, RpcError> {
        Ok(self.0.list_mounts())
    }

    fn mounts_mount(&self, p: MountParams, _: &ClientInfo) -> Result<Mount, RpcError> {
        self.0.mount(&p)
    }

    fn mounts_unmount(&self, p: UnmountParams, _: &ClientInfo) -> Result<(), RpcError> {
        self.0.unmount(&p.id)
    }

    fn restore_start(&self, p: RestoreParams, _: &ClientInfo) -> Result<Uuid, RpcError> {
        ops::submit_restore(&self.0, p)
    }

    fn vhdx_export(&self, p: ExportVhdxParams, _: &ClientInfo) -> Result<Uuid, RpcError> {
        ops::submit_export(&self.0, p)
    }

    fn viboot_start(&self, p: VibootStartParams, _: &ClientInfo) -> Result<Uuid, RpcError> {
        ops::submit_viboot(&self.0, p)
    }

    fn viboot_list(&self, _: EmptyParams, _: &ClientInfo) -> Result<Vec<VirtualMachine>, RpcError> {
        Ok(ops::list_viboot(&self.0))
    }

    fn viboot_stop(&self, p: VibootStopParams, _: &ClientInfo) -> Result<(), RpcError> {
        ops::stop_viboot(&self.0, &p.name)
    }

    fn redeploy_start(&self, p: RedeployParams, _: &ClientInfo) -> Result<Uuid, RpcError> {
        ops::submit_redeploy(&self.0, p)
    }

    fn settings_get(&self, _: EmptyParams, _: &ClientInfo) -> Result<Settings, RpcError> {
        Ok(self.0.store.settings())
    }

    fn settings_set(&self, p: SettingsSetParams, _: &ClientInfo) -> Result<(), RpcError> {
        self.0.store.set_settings(p.settings)
    }

    fn credentials_list(&self, _: EmptyParams, _: &ClientInfo) -> Result<Vec<Credential>, RpcError> {
        Ok(self.0.store.credentials())
    }

    fn credentials_set(&self, p: CredentialSetParams, _: &ClientInfo) -> Result<(), RpcError> {
        self.0.store.set_credential(&p.target, &p.user, &p.password)
    }

    fn credentials_delete(&self, p: CredentialDeleteParams, _: &ClientInfo) -> Result<(), RpcError> {
        self.0.store.delete_credential(&p.target)
    }

    fn history_list(&self, p: HistoryParams, _: &ClientInfo) -> Result<Vec<HistoryEntry>, RpcError> {
        let limit = if p.limit == 0 { 100 } else { p.limit.min(10_000) } as usize;
        Ok(self.0.store.history(limit))
    }

    fn logs_read(&self, p: LogsParams, _: &ClientInfo) -> Result<String, RpcError> {
        if let Some(log) = self.0.jobs.log_of(p.job_id) {
            return Ok(log);
        }
        match self.0.jobs.get(p.job_id) {
            Some(_) => Ok(String::new()),
            None => Err(not_found("job log")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::Paths;

    const CLIENT: ClientInfo = ClientInfo { process_id: 1, is_admin: true };

    fn handler() -> (tempfile::TempDir, Handler) {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path().join("data"));
        paths.create(false).unwrap();
        (dir, Handler(App::new(&paths)))
    }

    fn def(dest: &Path) -> Definition {
        Definition {
            id: Uuid::nil(),
            name: "System".into(),
            enabled: true,
            disks: vec![DiskSelection { number: 0, disk_id: String::new(), partitions: None }],
            destination: dest.display().to_string(),
            prefix: String::new(),
            mode: BackupMode::Smart,
            compression_level: 3,
            encrypt: false,
            password_set: false,
            verify: true,
            split_size: None,
            comment: String::new(),
            retention: None,
            schedule: None,
            run_missed: false,
            last_run: None,
            next_run: None,
        }
    }

    fn restore(partition: Option<u32>, target_partition: Option<u32>, target_offset: Option<u64>) -> RestoreParams {
        RestoreParams {
            path: r"C:\nonexistent\x.irimg".into(),
            password: None,
            disk: 0,
            partition,
            target_disk: 1,
            target_partition,
            target_offset,
            extend: false,
        }
    }

    #[test]
    fn definitions_round_trip() {
        let (dir, h) = handler();
        let id =
            h.definitions_save(SaveDefinitionParams { definition: def(dir.path()), password: None }, &CLIENT).unwrap();
        let listed = h.definitions_list(EmptyParams {}, &CLIENT).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].prefix, "System");
        assert_eq!(h.definitions_get(IdParams { id }, &CLIENT).unwrap().id, id);
        h.definitions_delete(IdParams { id }, &CLIENT).unwrap();
        assert_eq!(h.definitions_delete(IdParams { id }, &CLIENT).unwrap_err().code, NOT_FOUND);
        assert_eq!(h.definitions_run(IdParams { id }, &CLIENT).unwrap_err().code, NOT_FOUND);
    }

    #[test]
    fn invalid_requests_fail_before_queueing() {
        let (dir, h) = handler();
        for p in [restore(None, Some(1), None), restore(None, None, Some(0)), restore(Some(1), Some(1), Some(0))] {
            assert_eq!(h.restore_start(p, &CLIENT).unwrap_err().code, INVALID_PARAMS);
        }
        let export = |output: &str| ExportVhdxParams {
            path: r"C:\nonexistent\x.irimg".into(),
            password: None,
            disk: 0,
            output: output.into(),
        };
        assert_eq!(h.vhdx_export(export("relative.vhdx"), &CLIENT).unwrap_err().code, INVALID_PARAMS);
        let out = dir.path().join("out.vhd").display().to_string();
        assert_eq!(h.vhdx_export(export(&out), &CLIENT).unwrap_err().code, INVALID_PARAMS);

        let mut encrypted = def(dir.path());
        encrypted.encrypt = true;
        let params = BackupStartParams { definition: encrypted.clone(), password: None };
        assert_eq!(h.backup_start(params, &CLIENT).unwrap_err().code, PASSWORD_REQUIRED);
        let params = SaveDefinitionParams { definition: encrypted, password: Some(String::new()) };
        assert_eq!(h.definitions_save(params, &CLIENT).unwrap_err().code, PASSWORD_REQUIRED);

        let drivers = RedeployParams { disk: 0, drivers: Some(vec!["relative".into()]) };
        assert_eq!(h.redeploy_start(drivers, &CLIENT).unwrap_err().code, INVALID_PARAMS);
        assert!(h.jobs_list(EmptyParams {}, &CLIENT).unwrap().is_empty());
    }

    #[test]
    fn unknown_ids() {
        let (_dir, h) = handler();
        let id = Uuid::from_u128(5);
        assert_eq!(h.mounts_unmount(UnmountParams { id: MountId::Id(id) }, &CLIENT).unwrap_err().code, NOT_FOUND);
        h.mounts_unmount(UnmountParams { id: MountId::All(AllMounts::All) }, &CLIENT).unwrap();
        assert!(h.mounts_list(EmptyParams {}, &CLIENT).unwrap().is_empty());
        assert_eq!(h.logs_read(LogsParams { job_id: id }, &CLIENT).unwrap_err().code, NOT_FOUND);
        assert_eq!(h.jobs_cancel(IdParams { id }, &CLIENT).unwrap_err().code, NOT_FOUND);
        assert_eq!(h.viboot_stop(VibootStopParams { name: "x".into() }, &CLIENT).unwrap_err().code, NOT_FOUND);
        assert!(h.viboot_list(EmptyParams {}, &CLIENT).unwrap().is_empty());
        assert!(h.history_list(HistoryParams { limit: 0 }, &CLIENT).unwrap().is_empty());
    }
}
