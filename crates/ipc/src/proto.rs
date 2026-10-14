use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

pub const PIPE_NAME: &str = r"\\.\pipe\inocrium-reflect";
pub const PROTOCOL_VERSION: u32 = 1;
pub const PARSE_ERROR: i32 = -32700;
pub const INVALID_REQUEST: i32 = -32600;
pub const METHOD_NOT_FOUND: i32 = -32601;
pub const INVALID_PARAMS: i32 = -32602;
pub const INTERNAL_ERROR: i32 = -32603;
pub const CANCELLED: i32 = -32001;
pub const NOT_FOUND: i32 = -32002;
pub const BUSY: i32 = -32003;
pub const PASSWORD_REQUIRED: i32 = -32004;
pub const BAD_PASSWORD: i32 = -32005;
pub const ACCESS_DENIED: i32 = -32006;

#[derive(Debug, Clone, Serialize, Deserialize, thiserror::Error)]
#[error("{message} (RPC {code})")]
pub struct RpcError {
    pub code: i32,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}
impl RpcError {
    pub fn new(code: i32, message: impl Into<String>) -> Self {
        Self { code, message: message.into(), data: None }
    }
    pub fn method_not_found() -> Self {
        Self::new(METHOD_NOT_FOUND, "Method not found")
    }
    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::new(INVALID_PARAMS, message)
    }
}

macro_rules! dto { ($name:ident { $($field:ident : $ty:ty),* $(,)? }) => {
    #[derive(Clone, Serialize, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct $name { $(pub $field: $ty),* }
    impl std::fmt::Debug for $name {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            let mut out = f.debug_struct(stringify!($name));
            $(if matches!(stringify!($field), "password" | "secret") {
                out.field(stringify!($field), &"<redacted>");
            } else {
                out.field(stringify!($field), &self.$field);
            })*
            out.finish()
        }
    }
}; }
macro_rules! wire_enum { ($name:ident { $($variant:ident),* $(,)? }) => {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub enum $name { $($variant),* }
}; }
wire_enum!(DiskStyle { Gpt, Mbr, Raw });
wire_enum!(BackupMode { Smart, Forensic });
wire_enum!(RetentionKind { KeepLast, KeepDays });
wire_enum!(ScheduleKind { Daily, Weekly, Monthly });
wire_enum!(JobKind { Backup, Verify, Restore, ExportVhdx, Redeploy, Viboot });
wire_enum!(JobState { Queued, Running, Succeeded, Failed, Cancelled });
dto!(ServiceInfo { version: String, protocol_version: u32, machine: String });
dto!(Partition { number: u32, offset: u64, length: u64, type_name: String, file_system: Option<String>, label: Option<String>, letter: Option<String>, used_bytes: Option<u64>, is_system: bool, is_boot: bool, is_esp: bool });
dto!(Disk { number: u32, model: String, serial: String, size: u64, sector_size: u32, style: DiskStyle, disk_id: String, is_system: bool, partitions: Vec<Partition> });
dto!(DiskSelection { number: u32, disk_id: String, partitions: Option<Vec<u32>> });
dto!(Retention { kind: RetentionKind, value: u32 });
dto!(Schedule { kind: ScheduleKind, time: String, weekdays: Vec<u8>, day_of_month: Option<u8> });
dto!(Definition { id: Uuid, name: String, enabled: bool, disks: Vec<DiskSelection>, destination: String, prefix: String, mode: BackupMode, compression_level: u8, encrypt: bool, password_set: bool, verify: bool, split_size: Option<u64>, comment: String, retention: Option<Retention>, schedule: Option<Schedule>, run_missed: bool, last_run: Option<DateTime<Utc>>, next_run: Option<DateTime<Utc>> });
impl Definition {
    pub fn validate(&self) -> Result<(), RpcError> {
        if self.compression_level > 19
            || self.disks.is_empty()
            || self.destination.trim().is_empty()
            || self.split_size == Some(0)
            || self.retention.as_ref().is_some_and(|r| r.value == 0)
        {
            return Err(RpcError::invalid_params(
                "Disks, destination, compression (0..19), split size or retention invalid",
            ));
        }
        if let Some(s) = &self.schedule {
            if s.time.len() != 5
                || chrono::NaiveTime::parse_from_str(&s.time, "%H:%M").is_err()
                || s.weekdays.iter().any(|d| *d > 6)
                || (s.kind == ScheduleKind::Weekly && s.weekdays.is_empty())
                || (s.kind == ScheduleKind::Monthly && !s.day_of_month.is_some_and(|d| (1..=31).contains(&d)))
            {
                return Err(RpcError::invalid_params("Invalid schedule"));
            }
        }
        Ok(())
    }
}
dto!(Progress { phase: String, detail: String, bytes_done: u64, bytes_total: u64, bytes_written: u64, rate: u64 });
dto!(Job { id: Uuid, kind: JobKind, title: String, state: JobState, progress: Progress, started: Option<DateTime<Utc>>, finished: Option<DateTime<Utc>>, error: Option<String> });
dto!(ImagePartition { number: u32, length: u64, file_system: Option<String>, label: Option<String>, captured: bool, mode: BackupMode });
dto!(ImageDisk { index: u32, size: u64, style: DiskStyle, partitions: Vec<ImagePartition> });
dto!(ImageSummary { path: String, created: DateTime<Utc>, comment: String, size_on_disk: u64, encrypted: bool, definition_id: Option<Uuid>, disks: Vec<ImageDisk>, incomplete: bool });
pub type ImageDetails = ImageSummary;
dto!(Mount { id: Uuid, image: String, disk: u32, partition: Option<u32>, writable: bool, disk_number: Option<u32>, letters: Vec<String> });
dto!(VirtualMachine { name: String, state: String, image: String });
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    pub default_destination: String,
    pub log_retention_days: u32,
    pub threads: u32,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}
dto!(Credential { target: String, user: String });
dto!(HistoryEntry { job_id: Uuid, kind: JobKind, title: String, state: JobState, started: DateTime<Utc>, finished: Option<DateTime<Utc>>, bytes: u64, error: Option<String> });
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EmptyParams {}
dto!(IdParams { id: Uuid });
dto!(SaveDefinitionParams { definition: Definition, password: Option<String> });
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupStartParams {
    #[serde(flatten)]
    pub definition: Definition,
    pub password: Option<String>,
}
impl std::fmt::Debug for BackupStartParams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BackupStartParams")
            .field("definition", &self.definition)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ImagesListParams {
    pub folders: Option<Vec<String>>,
}
dto!(PathParams { path: String });
dto!(ImagePasswordParams { path: String, password: Option<String> });
dto!(MountParams { path: String, password: Option<String>, disk: Option<u32>, partition: Option<u32>, writable: bool });
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MountId {
    Id(Uuid),
    All(AllMounts),
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AllMounts {
    #[serde(rename = "all")]
    All,
}
dto!(UnmountParams { id: MountId });
dto!(RestoreParams { path: String, password: Option<String>, disk: u32, partition: Option<u32>, target_disk: u32, target_partition: Option<u32>, target_offset: Option<u64>, extend: bool });
dto!(ExportVhdxParams { path: String, password: Option<String>, disk: u32, output: String });
dto!(VibootStartParams { path: String, password: Option<String>, disk: u32, vm_name: Option<String>, memory_mb: u32, cpus: u32 });
dto!(VibootStopParams { name: String });
dto!(RedeployParams { disk: u32, drivers: Option<Vec<String>> });
dto!(SettingsSetParams { settings: Settings });
dto!(CredentialSetParams { target: String, user: String, password: String });
dto!(CredentialDeleteParams { target: String });
dto!(HistoryParams { limit: u32 });
dto!(LogsParams { job_id: Uuid });
dto!(JobProgress { job_id: Uuid, progress: Progress });
dto!(JobStateChanged { job: Job });

pub mod notifications {
    pub const JOB_PROGRESS: &str = "job.progress";
    pub const JOB_STATE: &str = "job.state";
    pub const MOUNTS_CHANGED: &str = "mounts.changed";
    pub const DEFINITIONS_CHANGED: &str = "definitions.changed";
}

macro_rules! methods {
    ($($constant:ident, $method:ident, $wire:literal, $params:ty, $result:ty;)+) => {
        pub mod methods { $(pub const $constant: &str = $wire;)+ }
        /// Implement only supported methods; defaults return MethodNotFound.
        pub trait Service: Send + Sync {
            $(fn $method(&self, _params: $params, _client: &crate::server::ClientInfo) -> Result<$result, RpcError> { Err(RpcError::method_not_found()) })+
        }
        pub fn dispatch(svc: &(impl Service + ?Sized), method: &str, params: Value, client: &crate::server::ClientInfo) -> Result<Value, RpcError> {
            let params = if params.is_null() { serde_json::json!({}) } else { params };
            if !params.is_object() { return Err(RpcError::invalid_params("Expected named/object params")); }
            match method {
                $($wire => {
                    let parsed: $params = serde_json::from_value(params).map_err(|_| RpcError::invalid_params(concat!("Invalid params for ", $wire)))?;
                    serde_json::to_value(svc.$method(parsed, client)?).map_err(|_| RpcError::new(INTERNAL_ERROR, "Result serialization failed"))
                },)+
                _ => Err(RpcError::method_not_found()),
            }
        }
    };
}
methods! {
    SERVICE_INFO, service_info, "service.info", EmptyParams, ServiceInfo;
    DISKS_LIST, disks_list, "disks.list", EmptyParams, Vec<Disk>;
    DEFINITIONS_LIST, definitions_list, "definitions.list", EmptyParams, Vec<Definition>;
    DEFINITIONS_GET, definitions_get, "definitions.get", IdParams, Definition;
    DEFINITIONS_SAVE, definitions_save, "definitions.save", SaveDefinitionParams, Uuid;
    DEFINITIONS_DELETE, definitions_delete, "definitions.delete", IdParams, ();
    DEFINITIONS_RUN, definitions_run, "definitions.run", IdParams, Uuid;
    BACKUP_START, backup_start, "backup.start", BackupStartParams, Uuid;
    JOBS_LIST, jobs_list, "jobs.list", EmptyParams, Vec<Job>;
    JOBS_CANCEL, jobs_cancel, "jobs.cancel", IdParams, ();
    IMAGES_LIST, images_list, "images.list", ImagesListParams, Vec<ImageSummary>;
    IMAGES_INSPECT, images_inspect, "images.inspect", ImagePasswordParams, ImageDetails;
    IMAGES_VERIFY, images_verify, "images.verify", ImagePasswordParams, Uuid;
    IMAGES_DELETE, images_delete, "images.delete", PathParams, ();
    MOUNTS_LIST, mounts_list, "mounts.list", EmptyParams, Vec<Mount>;
    MOUNTS_MOUNT, mounts_mount, "mounts.mount", MountParams, Mount;
    MOUNTS_UNMOUNT, mounts_unmount, "mounts.unmount", UnmountParams, ();
    RESTORE_START, restore_start, "restore.start", RestoreParams, Uuid;
    VHDX_EXPORT, vhdx_export, "vhdx.export", ExportVhdxParams, Uuid;
    VIBOOT_START, viboot_start, "viboot.start", VibootStartParams, Uuid;
    VIBOOT_LIST, viboot_list, "viboot.list", EmptyParams, Vec<VirtualMachine>;
    VIBOOT_STOP, viboot_stop, "viboot.stop", VibootStopParams, ();
    REDEPLOY_START, redeploy_start, "redeploy.start", RedeployParams, Uuid;
    SETTINGS_GET, settings_get, "settings.get", EmptyParams, Settings;
    SETTINGS_SET, settings_set, "settings.set", SettingsSetParams, ();
    CREDENTIALS_LIST, credentials_list, "credentials.list", EmptyParams, Vec<Credential>;
    CREDENTIALS_SET, credentials_set, "credentials.set", CredentialSetParams, ();
    CREDENTIALS_DELETE, credentials_delete, "credentials.delete", CredentialDeleteParams, ();
    HISTORY_LIST, history_list, "history.list", HistoryParams, Vec<HistoryEntry>;
    LOGS_READ, logs_read, "logs.read", LogsParams, String;
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn password_dtos_redact_debug_without_changing_wire_data() {
        let password = "private IPC password";
        macro_rules! check {
            ($ty:ty, $json:expr) => {{
                let input = $json;
                let params: $ty = serde_json::from_value(input.clone()).unwrap();
                let debug = format!("{params:?}");
                assert!(!debug.contains(password));
                assert!(debug.contains("<redacted>"));
                assert_eq!(serde_json::to_value(params).unwrap(), input);
            }};
        }
        let definition = serde_json::json!({
            "id": Uuid::nil(), "name": "test", "enabled": false, "disks": [], "destination": "backup",
            "prefix": "test", "mode": "smart", "compressionLevel": 0, "encrypt": true, "passwordSet": true,
            "verify": false, "splitSize": null, "comment": "", "retention": null, "schedule": null,
            "runMissed": false, "lastRun": null, "nextRun": null,
        });
        check!(SaveDefinitionParams, serde_json::json!({ "definition": definition, "password": password }));
        let mut backup = definition.clone();
        backup["password"] = password.into();
        check!(BackupStartParams, backup);
        check!(ImagePasswordParams, serde_json::json!({ "path": "image", "password": password }));
        check!(
            MountParams,
            serde_json::json!({ "path": "image", "password": password, "disk": null,
            "partition": null, "writable": false })
        );
        check!(
            RestoreParams,
            serde_json::json!({ "path": "image", "password": password, "disk": 0,
            "partition": null, "targetDisk": 1, "targetPartition": null, "targetOffset": null, "extend": false })
        );
        check!(
            ExportVhdxParams,
            serde_json::json!({ "path": "image", "password": password, "disk": 0,
            "output": "out" })
        );
        check!(
            VibootStartParams,
            serde_json::json!({ "path": "image", "password": password, "disk": 0,
            "vmName": null, "memoryMb": 2048, "cpus": 2 })
        );
        check!(CredentialSetParams, serde_json::json!({ "target": "share", "user": "user", "password": password }));
    }
    struct TestService;
    impl Service for TestService {
        fn service_info(&self, _: EmptyParams, _: &crate::server::ClientInfo) -> Result<ServiceInfo, RpcError> {
            Ok(ServiceInfo { version: "test".into(), protocol_version: 1, machine: "test".into() })
        }
    }
    #[test]
    fn typed_dispatch() {
        let client = crate::server::ClientInfo { process_id: 1, is_admin: false };
        assert_eq!(dispatch(&TestService, "service.info", Value::Null, &client).unwrap()["protocolVersion"], 1);
        assert_eq!(
            dispatch(&TestService, "definitions.get", serde_json::json!({"id":"bad"}), &client).unwrap_err().code,
            INVALID_PARAMS
        );
        assert_eq!(dispatch(&TestService, "unknown", Value::Null, &client).unwrap_err().code, METHOD_NOT_FOUND);
        assert_eq!(dispatch(&TestService, "disks.list", Value::Null, &client).unwrap_err().code, METHOD_NOT_FOUND);
        assert!(serde_json::from_value::<UnmountParams>(serde_json::json!({"id":"everything"})).is_err());
        assert!(serde_json::from_value::<UnmountParams>(serde_json::json!({"id":"all"})).is_ok());
    }
}
