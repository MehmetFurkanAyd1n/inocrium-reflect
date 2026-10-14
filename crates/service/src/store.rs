//! Persistent state: backup definitions, settings, share credentials and job history.
//!
//! Every file lives under [`Paths::root`] and is replaced atomically. Passwords are kept only
//! as DPAPI blobs (hex) and never leave this module except to the job that needs them.

use std::collections::HashMap;
use std::path::Path;

use chrono::{DateTime, Local, Utc};
use ipc::proto::{
    Credential, Definition, HistoryEntry, RpcError, Settings, INVALID_PARAMS, NOT_FOUND, PASSWORD_REQUIRED,
};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::paths::{write_atomic, Paths};
use crate::schedule;

/// History entries kept at most, regardless of age.
const HISTORY_CAP: usize = 1000;

/// A definition as stored on disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredDefinition {
    #[serde(flatten)]
    pub def: Definition,
    /// DPAPI blob (hex) of the image password.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password_blob: Option<String>,
    /// Schedule occurrences at or before this instant have been handled (or predate the
    /// schedule) and never fire.
    #[serde(default)]
    pub anchor: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredCredential {
    target: String,
    user: String,
    password_blob: String,
}

pub struct Store {
    paths: Paths,
    defs: Mutex<HashMap<Uuid, StoredDefinition>>,
    settings: Mutex<Settings>,
    creds: Mutex<Vec<StoredCredential>>,
    history: Mutex<Vec<HistoryEntry>>,
}

pub fn default_settings() -> Settings {
    Settings { default_destination: String::new(), log_retention_days: 30, threads: 0, extra: Default::default() }
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Option<T> {
    match std::fs::read(path) {
        Ok(b) => match serde_json::from_slice(&b) {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::warn!("ignoring unreadable {}: {e}", path.display());
                None
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            tracing::warn!("could not read {}: {e}", path.display());
            None
        }
    }
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), RpcError> {
    let data = serde_json::to_vec_pretty(value).map_err(internal)?;
    write_atomic(path, &data).map_err(internal)
}

pub fn internal(e: impl std::fmt::Display) -> RpcError {
    RpcError::new(ipc::proto::INTERNAL_ERROR, e.to_string())
}

fn to_hex(data: &[u8]) -> String {
    data.iter().map(|b| format!("{b:02x}")).collect()
}

fn from_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

fn protect(secret: &str) -> Result<String, RpcError> {
    winio::dpapi_protect(secret.as_bytes(), false).map(|b| to_hex(&b)).map_err(internal)
}

fn unprotect(blob: &str) -> Option<String> {
    let data = from_hex(blob)?;
    match winio::dpapi_unprotect(&data) {
        Ok(plain) => String::from_utf8(plain).ok(),
        Err(e) => {
            tracing::warn!("could not decrypt a stored secret: {e}");
            None
        }
    }
}

/// `\\server\share` in lower case, the key credentials are stored under.
pub fn credential_target(target: &str) -> Option<String> {
    winio::share_root(Path::new(target.trim())).map(|s| s.to_lowercase())
}

/// Characters that cannot appear in an image file name prefix.
fn valid_prefix(prefix: &str) -> bool {
    !prefix.trim().is_empty()
        && prefix.len() <= 100
        && !prefix.chars().any(|c| c.is_control() || r#"\/:*?"<>|"#.contains(c))
        && !prefix.ends_with('.')
        && !prefix.ends_with(' ')
}

fn same_schedule(a: &Definition, b: &Definition) -> bool {
    serde_json::to_value(&a.schedule).ok() == serde_json::to_value(&b.schedule).ok()
}

/// Checks the parts of a definition `Definition::validate` does not.
pub fn validate_definition(def: &mut Definition) -> Result<(), RpcError> {
    def.validate()?;
    if def.name.trim().is_empty() {
        return Err(RpcError::invalid_params("the definition needs a name"));
    }
    if def.prefix.trim().is_empty() {
        def.prefix = def.name.chars().filter(|c| c.is_alphanumeric() || "-_ ".contains(*c)).collect();
        if def.prefix.trim().is_empty() {
            def.prefix = "backup".into();
        }
    }
    if !valid_prefix(&def.prefix) {
        return Err(RpcError::invalid_params("the image prefix contains characters not allowed in file names"));
    }
    if !Path::new(def.destination.trim()).is_absolute() {
        return Err(RpcError::invalid_params("the destination must be an absolute local or UNC path"));
    }
    def.destination = def.destination.trim().to_string();
    Ok(())
}

impl Store {
    pub fn load(paths: &Paths) -> Self {
        let now = Utc::now();
        let mut defs = HashMap::new();
        if let Ok(dir) = std::fs::read_dir(paths.definitions()) {
            for entry in dir.flatten() {
                let path = entry.path();
                if path.extension().is_none_or(|e| e != "json") {
                    continue;
                }
                if let Some(mut d) = read_json::<StoredDefinition>(&path) {
                    d.anchor.get_or_insert(now);
                    defs.insert(d.def.id, d);
                }
            }
        }
        let mut settings = read_json(&paths.settings()).unwrap_or_else(default_settings);
        if settings.log_retention_days == 0 {
            settings.log_retention_days = 30;
        }
        Self {
            paths: paths.clone(),
            defs: Mutex::new(defs),
            settings: Mutex::new(settings),
            creds: Mutex::new(read_json(&paths.credentials()).unwrap_or_default()),
            history: Mutex::new(read_json(&paths.history()).unwrap_or_default()),
        }
    }

    pub fn paths(&self) -> &Paths {
        &self.paths
    }

    // ---- definitions ----

    /// The definition as clients see it (computed `passwordSet` / `nextRun`).
    fn public(d: &StoredDefinition) -> Definition {
        let mut def = d.def.clone();
        def.password_set = d.password_blob.is_some();
        def.next_run = match (&def.schedule, def.enabled) {
            (Some(s), true) => schedule::next_run(s, &Local::now()).map(|t| t.with_timezone(&Utc)),
            _ => None,
        };
        def
    }

    pub fn definitions(&self) -> Vec<Definition> {
        let mut out: Vec<_> = self.defs.lock().values().map(Self::public).collect();
        out.sort_by_key(|d| d.name.to_lowercase());
        out
    }

    pub fn definition(&self, id: Uuid) -> Result<Definition, RpcError> {
        self.defs.lock().get(&id).map(Self::public).ok_or_else(|| not_found("definition"))
    }

    /// The stored definition including its schedule anchor.
    pub fn stored(&self, id: Uuid) -> Option<StoredDefinition> {
        self.defs.lock().get(&id).cloned()
    }

    pub fn all_stored(&self) -> Vec<StoredDefinition> {
        self.defs.lock().values().cloned().collect()
    }

    /// The decrypted image password of a definition.
    pub fn definition_password(&self, id: Uuid) -> Option<String> {
        let blob = self.defs.lock().get(&id)?.password_blob.clone()?;
        unprotect(&blob)
    }

    /// Creates or replaces a definition. A missing password keeps the stored one; turning
    /// encryption off forgets it.
    pub fn save_definition(&self, mut def: Definition, password: Option<String>) -> Result<Uuid, RpcError> {
        validate_definition(&mut def)?;
        let mut defs = self.defs.lock();
        if def.id.is_nil() {
            def.id = Uuid::new_v4();
        }
        let existing = defs.get(&def.id);
        let password_blob = if !def.encrypt {
            None
        } else if let Some(pw) = password.filter(|p| !p.is_empty()) {
            Some(protect(&pw)?)
        } else if let Some(blob) = existing.and_then(|e| e.password_blob.clone()) {
            Some(blob)
        } else {
            return Err(RpcError::new(PASSWORD_REQUIRED, "an encrypted definition needs a password"));
        };
        // A new or changed schedule (or re-enabling) must not fire for past occurrences.
        let anchor = match existing {
            Some(e) if same_schedule(&e.def, &def) && (e.def.enabled || !def.enabled) => e.anchor,
            _ => Some(Utc::now()),
        };
        def.last_run = existing.and_then(|e| e.def.last_run);
        def.password_set = password_blob.is_some();
        def.next_run = None;
        let stored = StoredDefinition { def, password_blob, anchor };
        write_json(&self.paths.definitions().join(format!("{}.json", stored.def.id)), &stored)?;
        let id = stored.def.id;
        defs.insert(id, stored);
        Ok(id)
    }

    pub fn delete_definition(&self, id: Uuid) -> Result<(), RpcError> {
        let mut defs = self.defs.lock();
        if defs.remove(&id).is_none() {
            return Err(not_found("definition"));
        }
        let path = self.paths.definitions().join(format!("{id}.json"));
        match std::fs::remove_file(path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(internal(e)),
            _ => Ok(()),
        }
    }

    /// Records a run: `last_run` for display and, for scheduled runs, the anchor.
    pub fn mark_run(&self, id: Uuid, at: DateTime<Utc>, scheduled: bool) {
        let mut defs = self.defs.lock();
        let Some(d) = defs.get_mut(&id) else { return };
        d.def.last_run = Some(at);
        if scheduled {
            d.anchor = Some(d.anchor.map_or(at, |a| a.max(at)));
        }
        if let Err(e) = write_json(&self.paths.definitions().join(format!("{id}.json")), &*d) {
            tracing::warn!("could not save definition {id}: {}", e.message);
        }
    }

    /// Moves the anchor forward to `at` without recording a run (a skipped occurrence).
    pub fn advance_anchor(&self, id: Uuid, at: DateTime<Utc>) {
        let mut defs = self.defs.lock();
        let Some(d) = defs.get_mut(&id) else { return };
        d.anchor = Some(d.anchor.map_or(at, |a| a.max(at)));
        if let Err(e) = write_json(&self.paths.definitions().join(format!("{id}.json")), &*d) {
            tracing::warn!("could not save definition {id}: {}", e.message);
        }
    }

    // ---- settings ----

    pub fn settings(&self) -> Settings {
        self.settings.lock().clone()
    }

    pub fn set_settings(&self, settings: Settings) -> Result<(), RpcError> {
        if settings.threads > 64 || settings.log_retention_days == 0 {
            return Err(RpcError::invalid_params("threads must be 0..64 and log retention at least one day"));
        }
        let dest = settings.default_destination.trim();
        if !dest.is_empty() && !Path::new(dest).is_absolute() {
            return Err(RpcError::invalid_params("the default destination must be an absolute path"));
        }
        if let Some(v) = settings.extra.get("imageFolders") {
            if !v.as_array().is_some_and(|a| a.iter().all(|f| f.as_str().is_some_and(|s| Path::new(s).is_absolute()))) {
                return Err(RpcError::invalid_params("imageFolders must be a list of absolute paths"));
            }
        }
        let mut current = self.settings.lock();
        write_json(&self.paths.settings(), &settings)?;
        *current = settings;
        Ok(())
    }

    /// Extra folders to look for images in (`imageFolders` setting).
    pub fn image_folders(&self) -> Vec<String> {
        let s = self.settings.lock();
        let mut out: Vec<String> = s
            .extra
            .get("imageFolders")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|f| f.as_str().map(String::from)).collect())
            .unwrap_or_default();
        if !s.default_destination.trim().is_empty() {
            out.push(s.default_destination.trim().to_string());
        }
        out
    }

    /// Adds `folder` to the `imageFolders` setting unless `images.list` already searches it,
    /// so images written by one-off backups show up without configuration.
    pub fn remember_image_folder(&self, folder: &str) -> Result<(), RpcError> {
        let folder = folder.trim();
        if folder.is_empty() || !Path::new(folder).is_absolute() {
            return Ok(());
        }
        let key = crate::images::path_key(Path::new(folder));
        let searched = self.image_folders().into_iter().chain(self.definitions().into_iter().map(|d| d.destination));
        if searched.into_iter().any(|f| !f.trim().is_empty() && crate::images::path_key(Path::new(f.trim())) == key) {
            return Ok(());
        }
        let mut settings = self.settings();
        let folders = settings.extra.entry("imageFolders").or_insert_with(|| serde_json::json!([]));
        if !folders.is_array() {
            *folders = serde_json::json!([]);
        }
        folders.as_array_mut().unwrap().push(serde_json::Value::String(folder.to_string()));
        self.set_settings(settings)
    }

    // ---- credentials ----

    pub fn credentials(&self) -> Vec<Credential> {
        self.creds.lock().iter().map(|c| Credential { target: c.target.clone(), user: c.user.clone() }).collect()
    }

    pub fn set_credential(&self, target: &str, user: &str, password: &str) -> Result<(), RpcError> {
        let target = credential_target(target)
            .ok_or_else(|| RpcError::new(INVALID_PARAMS, r"the target must be a share (\\server\share)"))?;
        if user.trim().is_empty() || user.chars().any(char::is_control) {
            return Err(RpcError::invalid_params("a user name is required"));
        }
        let blob = protect(password)?;
        let mut creds = self.creds.lock();
        let mut next = creds.clone();
        next.retain(|c| c.target != target);
        next.push(StoredCredential { target, user: user.trim().to_string(), password_blob: blob });
        next.sort_by(|a, b| a.target.cmp(&b.target));
        write_json(&self.paths.credentials(), &next)?;
        *creds = next;
        Ok(())
    }

    pub fn delete_credential(&self, target: &str) -> Result<(), RpcError> {
        let target = credential_target(target).unwrap_or_else(|| target.trim().to_lowercase());
        let mut creds = self.creds.lock();
        let mut next = creds.clone();
        next.retain(|c| c.target != target);
        if next.len() == creds.len() {
            return Err(not_found("credential"));
        }
        write_json(&self.paths.credentials(), &next)?;
        *creds = next;
        Ok(())
    }

    /// User and password for the share holding `path`, if one is stored.
    pub fn credential_for(&self, path: &str) -> Option<(String, String)> {
        let target = credential_target(path)?;
        let creds = self.creds.lock();
        let c = creds.iter().find(|c| c.target == target)?;
        Some((c.user.clone(), unprotect(&c.password_blob)?))
    }

    // ---- history ----

    pub fn history(&self, limit: usize) -> Vec<HistoryEntry> {
        let h = self.history.lock();
        h.iter().rev().take(limit).cloned().collect()
    }

    /// Appends an entry and drops entries (and their job logs) older than the log retention.
    pub fn append_history(&self, entry: HistoryEntry) {
        let days = i64::from(self.settings.lock().log_retention_days.max(1));
        let cutoff = Utc::now() - chrono::Duration::days(days);
        let mut h = self.history.lock();
        h.push(entry);
        let mut dropped = Vec::new();
        while h.len() > HISTORY_CAP || h.first().is_some_and(|e| e.started < cutoff) {
            dropped.push(h.remove(0).job_id);
        }
        if let Err(e) = write_json(&self.paths.history(), &*h) {
            tracing::warn!("could not save history: {}", e.message);
        }
        drop(h);
        for id in dropped {
            let _ = std::fs::remove_file(self.paths.job_log(id));
        }
    }
}

pub fn not_found(what: &str) -> RpcError {
    RpcError::new(NOT_FOUND, format!("{what} not found"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ipc::proto::{BackupMode, DiskSelection, JobKind, JobState, Schedule, ScheduleKind};

    fn def(name: &str) -> Definition {
        Definition {
            id: Uuid::nil(),
            name: name.into(),
            enabled: true,
            disks: vec![DiskSelection { number: 0, disk_id: String::new(), partitions: None }],
            destination: r"D:\Backups".into(),
            prefix: String::new(),
            mode: BackupMode::Smart,
            compression_level: 3,
            encrypt: false,
            password_set: false,
            verify: true,
            split_size: None,
            comment: String::new(),
            retention: None,
            schedule: Some(Schedule {
                kind: ScheduleKind::Daily,
                time: "02:00".into(),
                weekdays: vec![],
                day_of_month: None,
            }),
            run_missed: true,
            last_run: None,
            next_run: None,
        }
    }

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path());
        paths.create(false).unwrap();
        let store = Store::load(&paths);
        (dir, store)
    }

    #[test]
    fn hex_round_trip() {
        assert_eq!(from_hex(&to_hex(&[0, 1, 0xab, 0xff])).unwrap(), vec![0, 1, 0xab, 0xff]);
        assert!(from_hex("abc").is_none());
        assert!(from_hex("zz").is_none());
    }

    #[test]
    fn definitions_persist_and_keep_passwords() {
        let (_dir, store) = store();
        let id = store.save_definition(def("System disk"), None).unwrap();
        assert!(!id.is_nil());
        let saved = store.definition(id).unwrap();
        assert_eq!(saved.prefix, "System disk");
        assert!(saved.next_run.is_some());

        let mut enc = saved.clone();
        enc.encrypt = true;
        assert_eq!(store.save_definition(enc.clone(), None).unwrap_err().code, PASSWORD_REQUIRED);
        store.save_definition(enc.clone(), Some("secret".into())).unwrap();
        assert!(store.definition(id).unwrap().password_set);
        // Saving again without a password keeps it.
        store.save_definition(enc.clone(), None).unwrap();
        assert_eq!(store.definition_password(id).as_deref(), Some("secret"));
        // The blob on disk is not the password.
        let raw = std::fs::read_to_string(store.paths().definitions().join(format!("{id}.json"))).unwrap();
        assert!(!raw.contains("secret") && raw.contains("passwordBlob"));

        let reloaded = Store::load(store.paths());
        assert_eq!(reloaded.definitions().len(), 1);
        assert_eq!(reloaded.definition_password(id).as_deref(), Some("secret"));

        enc.encrypt = false;
        store.save_definition(enc, None).unwrap();
        assert!(!store.definition(id).unwrap().password_set);
        store.delete_definition(id).unwrap();
        assert_eq!(store.delete_definition(id).unwrap_err().code, NOT_FOUND);
        assert!(Store::load(store.paths()).definitions().is_empty());
    }

    #[test]
    fn invalid_definitions_are_rejected() {
        let (_dir, store) = store();
        let mut d = def("x");
        d.prefix = "a/b".into();
        assert_eq!(store.save_definition(d, None).unwrap_err().code, INVALID_PARAMS);
        let mut d = def("x");
        d.destination = "relative".into();
        assert_eq!(store.save_definition(d, None).unwrap_err().code, INVALID_PARAMS);
        let mut d = def(" ");
        d.prefix = "p".into();
        assert_eq!(store.save_definition(d, None).unwrap_err().code, INVALID_PARAMS);
    }

    #[test]
    fn schedule_change_resets_anchor() {
        let (_dir, store) = store();
        let id = store.save_definition(def("a"), None).unwrap();
        let old = Utc::now() - chrono::Duration::days(3);
        store.defs.lock().get_mut(&id).unwrap().anchor = Some(old);
        let same = store.definition(id).unwrap();
        store.save_definition(same.clone(), None).unwrap();
        assert_eq!(store.stored(id).unwrap().anchor, Some(old));
        let mut changed = same;
        changed.schedule.as_mut().unwrap().time = "03:00".into();
        store.save_definition(changed, None).unwrap();
        assert!(store.stored(id).unwrap().anchor.unwrap() > old);
    }

    #[test]
    fn credentials_and_history() {
        let (_dir, store) = store();
        store.set_credential(r"\\NAS\Backups\pc1", "nas\\user", "pw").unwrap();
        assert_eq!(store.credentials()[0].target, r"\\nas\backups");
        assert_eq!(store.credential_for(r"\\nas\BACKUPS\x").unwrap(), ("nas\\user".into(), "pw".into()));
        assert!(store.set_credential(r"C:\x", "u", "p").is_err());
        store.delete_credential(r"\\nas\backups").unwrap();
        assert!(store.credentials().is_empty());

        for i in 0..3 {
            store.append_history(HistoryEntry {
                job_id: Uuid::new_v4(),
                kind: JobKind::Verify,
                title: format!("job {i}"),
                state: JobState::Succeeded,
                started: Utc::now(),
                finished: Some(Utc::now()),
                bytes: 0,
                error: None,
            });
        }
        let h = store.history(2);
        assert_eq!(h.len(), 2);
        assert_eq!(h[0].title, "job 2");
        assert_eq!(Store::load(store.paths()).history(10).len(), 3);
    }

    #[test]
    fn settings_validation() {
        let (_dir, store) = store();
        let mut s = store.settings();
        assert_eq!(s.log_retention_days, 30);
        s.threads = 65;
        assert!(store.set_settings(s.clone()).is_err());
        s.threads = 4;
        s.extra.insert("imageFolders".into(), serde_json::json!([r"E:\Images"]));
        store.set_settings(s).unwrap();
        assert_eq!(Store::load(store.paths()).image_folders(), vec![r"E:\Images".to_string()]);
    }

    #[cfg(windows)]
    #[test]
    fn remembered_image_folders() {
        let (_dir, store) = store();
        store.remember_image_folder(r"D:\Backups").unwrap();
        // Already searched (case and trailing separator ignored), relative or empty: unchanged.
        for folder in [r"d:\backups\", "relative", "  "] {
            store.remember_image_folder(folder).unwrap();
        }
        assert_eq!(Store::load(store.paths()).image_folders(), vec![r"D:\Backups".to_string()]);
    }
}
