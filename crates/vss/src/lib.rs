//! Volume Shadow Copy Service requester. COM objects never leave their MTA worker.
#![cfg(windows)]
mod requester;

use serde::{Deserialize, Serialize};
use std::{sync::mpsc, thread::JoinHandle};
use uuid::Uuid;
use windows::{
    core::HRESULT,
    Win32::Foundation::{E_FAIL, E_INVALIDARG},
};

/// Contextual VSS result.
pub type Result<T> = std::result::Result<T, Error>;
/// A failed HRESULT and the operation that produced it.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{context}: HRESULT {hresult:?}")]
pub struct Error {
    /// Operation that failed.
    pub context: String,
    /// HRESULT returned by Windows, or a contextual local failure code.
    pub hresult: HRESULT,
}
fn error(context: impl Into<String>, hresult: HRESULT) -> Error {
    Error { context: context.into(), hresult }
}
fn check(context: &str, result: windows::core::Result<()>) -> Result<()> {
    result.map_err(|e| error(context, e.code()))
}

/// A VSS snapshot and its read-only shadow device.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    /// Snapshot identifier.
    pub id: Uuid,
    /// Snapshot set identifier.
    pub set_id: Uuid,
    /// Original GUID volume path with trailing backslash.
    pub original_volume: String,
    /// Read-only shadow device path, without trailing backslash.
    pub device_path: String,
}
enum Command {
    Complete(bool, mpsc::Sender<Result<()>>),
}
/// Owns a snapshot set through a dedicated MTA thread. Non-persistent sets are deleted on drop.
pub struct SnapshotSet {
    id: Uuid,
    snapshots: Vec<Snapshot>,
    commands: Option<mpsc::Sender<Command>>,
    worker: Option<JoinHandle<()>>,
}
impl SnapshotSet {
    /// Create writer-aware backup snapshots, or persistent client-accessible snapshots without writers.
    pub fn create(volumes: &[String], persistent: bool) -> Result<Self> {
        if volumes.is_empty() {
            return Err(error("snapshot set needs at least one volume", E_INVALIDARG));
        }
        let volumes = volumes.to_vec();
        let (commands, rx) = mpsc::channel();
        let (ready, result) = mpsc::channel();
        let worker = std::thread::Builder::new()
            .name("inocrium-vss".into())
            .spawn(move || {
                let created = requester::Session::create(&volumes, persistent);
                match created {
                    Err(e) => {
                        let _ = ready.send(Err(e));
                    }
                    Ok(mut session) => {
                        if ready.send(Ok((session.id(), session.snapshots().to_vec()))).is_err() {
                            return;
                        }
                        if let Ok(Command::Complete(success, reply)) = rx.recv() {
                            let _ = reply.send(session.complete(success));
                        }
                        // All COM releases, writer cleanup and snapshot deletion occur on this thread.
                    }
                }
            })
            .map_err(|e| error(format!("start VSS worker: {e}"), E_FAIL))?;
        match result.recv() {
            Ok(Ok((id, snapshots))) => Ok(Self { id, snapshots, commands: Some(commands), worker: Some(worker) }),
            Ok(Err(e)) => {
                let _ = worker.join();
                Err(e)
            }
            Err(e) => {
                let _ = worker.join();
                Err(error(format!("VSS worker stopped: {e}"), E_FAIL))
            }
        }
    }
    /// Snapshot set identifier.
    pub fn id(&self) -> Uuid {
        self.id
    }
    /// Snapshots belonging to this set.
    pub fn snapshots(&self) -> &[Snapshot] {
        &self.snapshots
    }
    /// Resolve the shadow device for a volume, ignoring case and trailing backslashes.
    pub fn device_for(&self, original_volume: &str) -> Option<&str> {
        let original = requester::normalize_volume(original_volume).ok()?;
        self.snapshots
            .iter()
            .find(|s| s.original_volume.trim_end_matches('\\').eq_ignore_ascii_case(original.trim_end_matches('\\')))
            .map(|s| s.device_path.as_str())
    }
    /// Report success to writers (BackupComplete) or abort a failed backup; persistent snapshots remain.
    pub fn complete(mut self, success: bool) -> Result<()> {
        let (reply, rx) = mpsc::channel();
        self.commands
            .take()
            .ok_or_else(|| error("VSS worker already stopped", E_FAIL))?
            .send(Command::Complete(success, reply))
            .map_err(|e| error(e.to_string(), E_FAIL))?;
        let result = rx.recv().map_err(|e| error(e.to_string(), E_FAIL))?;
        if let Some(worker) = self.worker.take() {
            worker.join().map_err(|_| error("VSS worker panicked", E_FAIL))?;
        }
        result
    }
}
impl Drop for SnapshotSet {
    fn drop(&mut self) {
        self.commands.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
fn on_worker<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    std::thread::Builder::new()
        .name("inocrium-vss-query".into())
        .spawn(f)
        .map_err(|e| error(e.to_string(), E_FAIL))?
        .join()
        .map_err(|_| error("VSS query worker panicked", E_FAIL))?
}
/// Locate persistent snapshots belonging to a set, including snapshots from a previous process.
pub fn find_snapshot_set(set_id: Uuid) -> Result<Option<Vec<Snapshot>>> {
    on_worker(move || {
        let v = requester::query()?
            .into_iter()
            .filter(|(s, p)| *p && s.set_id == set_id)
            .map(|(s, _)| s)
            .collect::<Vec<_>>();
        Ok(if v.is_empty() { None } else { Some(v) })
    })
}
/// Delete a snapshot set. A missing set is treated as already deleted.
pub fn delete_snapshot_set(set_id: Uuid) -> Result<()> {
    on_worker(move || requester::delete(set_id))
}
/// Enumerate snapshots across all VSS contexts.
pub fn list_snapshots() -> Result<Vec<Snapshot>> {
    on_worker(|| Ok(requester::query()?.into_iter().map(|(s, _)| s).collect()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn send_contract() {
        fn send<T: Send>() {}
        send::<SnapshotSet>();
    }
    #[test]
    fn rejects_empty_sets() {
        assert!(SnapshotSet::create(&[], false).is_err());
    }
    #[test]
    fn normalization() {
        assert_eq!(requester::normalize_volume(r"\\?\Volume{test}").unwrap(), r"\\?\Volume{test}\");
    }
    // Run from an elevated terminal: cargo test -p vss -- --ignored.
    #[test]
    #[ignore = "requires administrator: snapshots the Windows volume and reads its shadow sector"]
    fn snapshot_read_and_delete() {
        use blockdev::BlockDevice;
        let set = SnapshotSet::create(&[r"C:\".into()], false).unwrap();
        let path = set.device_for(r"C:\").unwrap();
        let volume = winio::Volume::open(path, false).unwrap();
        let mut sector = [0; 512];
        volume.read_at(0, &mut sector).unwrap();
        assert_eq!(&sector[510..], &[0x55, 0xaa]);
        let id = set.id();
        drop(volume);
        set.complete(true).unwrap();
        delete_snapshot_set(id).unwrap();
    }
    #[test]
    #[ignore = "requires administrator: persistent snapshot lifecycle"]
    fn persistent_snapshot_lifecycle() {
        let set = SnapshotSet::create(&[r"C:\".into()], true).unwrap();
        let id = set.id();
        set.complete(true).unwrap();
        let found = find_snapshot_set(id);
        let deleted = delete_snapshot_set(id);
        assert!(!found.unwrap().unwrap().is_empty());
        deleted.unwrap();
        assert!(find_snapshot_set(id).unwrap().is_none());
    }
}
