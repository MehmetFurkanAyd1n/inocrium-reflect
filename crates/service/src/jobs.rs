//! Job queue: long-running operations run on worker threads and report progress and state
//! changes as notifications. Exclusive jobs (backups, restores, redeploys) run one at a time;
//! the others (verify, VHDX export, viBoot) start immediately.

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use chrono::Utc;
use crossbeam_channel::{unbounded, Sender};
use ipc::proto::{
    notifications, HistoryEntry, Job, JobKind, JobProgress, JobState, JobStateChanged, Progress, RpcError,
};
use parking_lot::Mutex;
use serde_json::Value;
use uuid::Uuid;

use crate::store::{not_found, Store};

/// Finished jobs kept in `jobs.list`.
const FINISHED_KEPT: usize = 100;
/// Minimum interval between progress notifications of one job.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);

/// Sends a notification to every client.
pub type Emit = Arc<dyn Fn(&str, Value) + Send + Sync>;

pub struct JobOutcome {
    /// Bytes processed (written to the image or target), for the history.
    pub bytes: u64,
}

pub type JobBody = Box<dyn FnOnce(&JobCtx) -> anyhow::Result<JobOutcome> + Send>;

pub struct JobSpec {
    pub kind: JobKind,
    pub title: String,
    /// Run on the serial worker, after the exclusive jobs queued before it.
    pub exclusive: bool,
    /// Definition the job runs (backups), so a definition never runs twice at once.
    pub definition: Option<Uuid>,
    pub body: JobBody,
}

/// What a running job sees.
pub struct JobCtx {
    pub id: Uuid,
    pub cancel: engine::Cancel,
    log: Mutex<String>,
    last_progress: Mutex<Option<Instant>>,
    shared: Arc<Shared>,
}

impl JobCtx {
    /// Appends a line to the job log. Callers must never pass secrets.
    pub fn log(&self, line: &str) {
        tracing::info!(job = %self.id, "{line}");
        let mut log = self.log.lock();
        let _ = writeln!(log, "{} {line}", Utc::now().format("%Y-%m-%d %H:%M:%S"));
    }

    /// Records progress; notifications are rate limited, except for phase changes.
    pub fn progress(&self, p: &engine::Progress) {
        let progress = Progress {
            phase: p.phase.clone(),
            detail: p.detail.clone(),
            bytes_done: p.bytes_done,
            bytes_total: p.bytes_total,
            bytes_written: p.bytes_written,
            rate: p.rate,
        };
        let phase_changed = {
            let mut jobs = self.shared.jobs.lock();
            let Some(job) = jobs.iter_mut().find(|j| j.job.id == self.id) else { return };
            let changed = job.job.progress.phase != progress.phase;
            job.job.progress = progress.clone();
            changed
        };
        let now = Instant::now();
        {
            let mut last = self.last_progress.lock();
            if !phase_changed && last.is_some_and(|t| now.duration_since(t) < PROGRESS_INTERVAL) {
                return;
            }
            *last = Some(now);
        }
        self.shared.emit(notifications::JOB_PROGRESS, &JobProgress { job_id: self.id, progress });
    }

    /// Sets the phase without byte counts (e.g. "snapshot", "retention").
    pub fn phase(&self, phase: &str, detail: &str) {
        self.progress(&engine::Progress { phase: phase.into(), detail: detail.into(), ..Default::default() });
    }
}

struct Entry {
    job: Job,
    definition: Option<Uuid>,
    cancel: engine::Cancel,
}

struct Shared {
    jobs: Mutex<Vec<Entry>>,
    emit: Emit,
    store: Arc<Store>,
}

impl Shared {
    fn emit<P: serde::Serialize>(&self, method: &str, params: &P) {
        match serde_json::to_value(params) {
            Ok(v) => (self.emit)(method, v),
            Err(e) => tracing::warn!("could not serialize a {method} notification: {e}"),
        }
    }

    /// Updates a job and announces it.
    fn update(&self, id: Uuid, f: impl FnOnce(&mut Job)) -> Option<Job> {
        let job = {
            let mut jobs = self.jobs.lock();
            let entry = jobs.iter_mut().find(|e| e.job.id == id)?;
            f(&mut entry.job);
            entry.job.clone()
        };
        self.emit(notifications::JOB_STATE, &JobStateChanged { job: job.clone() });
        Some(job)
    }

    /// Drops the oldest finished jobs beyond [`FINISHED_KEPT`].
    fn prune(&self) {
        let mut jobs = self.jobs.lock();
        let finished = jobs.iter().filter(|e| is_finished(e.job.state)).count();
        let mut excess = finished.saturating_sub(FINISHED_KEPT);
        jobs.retain(|e| {
            if excess > 0 && is_finished(e.job.state) {
                excess -= 1;
                false
            } else {
                true
            }
        });
    }
}

fn is_finished(state: JobState) -> bool {
    matches!(state, JobState::Succeeded | JobState::Failed | JobState::Cancelled)
}

struct Queued {
    id: Uuid,
    body: JobBody,
}

pub struct Jobs {
    shared: Arc<Shared>,
    serial: Mutex<Option<Sender<Queued>>>,
    worker: Mutex<Option<JoinHandle<()>>>,
    threads: Mutex<VecDeque<JoinHandle<()>>>,
}

impl Jobs {
    pub fn new(store: Arc<Store>, emit: Emit) -> Arc<Self> {
        let shared = Arc::new(Shared { jobs: Mutex::new(Vec::new()), emit, store });
        let (tx, rx) = unbounded::<Queued>();
        let worker_shared = shared.clone();
        let worker = std::thread::Builder::new()
            .name("jobs-serial".into())
            .spawn(move || {
                for q in rx {
                    run(&worker_shared, q);
                }
            })
            .expect("spawn job worker");
        Arc::new(Self {
            shared,
            serial: Mutex::new(Some(tx)),
            worker: Mutex::new(Some(worker)),
            threads: Mutex::new(VecDeque::new()),
        })
    }

    /// Queues a job and returns its id.
    pub fn submit(&self, spec: JobSpec) -> Result<Uuid, RpcError> {
        let id = Uuid::new_v4();
        let cancel = engine::Cancel::new();
        let job = Job {
            id,
            kind: spec.kind,
            title: spec.title,
            state: JobState::Queued,
            progress: Progress {
                phase: "queued".into(),
                detail: String::new(),
                bytes_done: 0,
                bytes_total: 0,
                bytes_written: 0,
                rate: 0,
            },
            started: None,
            finished: None,
            error: None,
        };
        {
            let mut jobs = self.shared.jobs.lock();
            if let Some(def) = spec.definition {
                if jobs.iter().any(|e| e.definition == Some(def) && !is_finished(e.job.state)) {
                    return Err(RpcError::new(ipc::proto::BUSY, "this definition is already running"));
                }
            }
            jobs.push(Entry { job: job.clone(), definition: spec.definition, cancel });
        }
        self.shared.emit(notifications::JOB_STATE, &JobStateChanged { job });
        let queued = Queued { id, body: spec.body };
        if spec.exclusive {
            let serial = self.serial.lock();
            let Some(tx) = serial.as_ref() else {
                return Err(RpcError::new(ipc::proto::BUSY, "the service is stopping"));
            };
            tx.send(queued).map_err(|_| RpcError::new(ipc::proto::BUSY, "the service is stopping"))?;
        } else {
            let shared = self.shared.clone();
            let handle = std::thread::Builder::new()
                .name(format!("job-{id}"))
                .spawn(move || run(&shared, queued))
                .map_err(crate::store::internal)?;
            let mut threads = self.threads.lock();
            threads.retain(|t| !t.is_finished());
            threads.push_back(handle);
        }
        Ok(id)
    }

    pub fn list(&self) -> Vec<Job> {
        self.shared.jobs.lock().iter().map(|e| e.job.clone()).collect()
    }

    pub fn get(&self, id: Uuid) -> Option<Job> {
        self.shared.jobs.lock().iter().find(|e| e.job.id == id).map(|e| e.job.clone())
    }

    /// Requests cancellation; a queued job is cancelled before it starts.
    pub fn cancel(&self, id: Uuid) -> Result<(), RpcError> {
        let jobs = self.shared.jobs.lock();
        let entry = jobs.iter().find(|e| e.job.id == id).ok_or_else(|| not_found("job"))?;
        entry.cancel.cancel();
        Ok(())
    }

    pub fn is_definition_active(&self, def: Uuid) -> bool {
        self.shared.jobs.lock().iter().any(|e| e.definition == Some(def) && !is_finished(e.job.state))
    }

    /// The log of a running or finished job.
    pub fn log_of(&self, id: Uuid) -> Option<String> {
        let path = self.shared.store.paths().job_log(id);
        std::fs::read_to_string(path).ok()
    }

    /// Cancels every job and waits for the workers.
    pub fn shutdown(&self) {
        for e in self.shared.jobs.lock().iter() {
            e.cancel.cancel();
        }
        drop(self.serial.lock().take());
        if let Some(w) = self.worker.lock().take() {
            let _ = w.join();
        }
        let threads: Vec<_> = self.threads.lock().drain(..).collect();
        for t in threads {
            let _ = t.join();
        }
    }
}

/// True when an error chain ends in a cancellation.
pub fn is_cancelled(e: &anyhow::Error) -> bool {
    e.chain().any(|c| matches!(c.downcast_ref::<engine::Error>(), Some(engine::Error::Cancelled)))
}

fn run(shared: &Arc<Shared>, q: Queued) {
    let Some(cancel) = shared.jobs.lock().iter().find(|e| e.job.id == q.id).map(|e| e.cancel.clone()) else {
        return;
    };
    let ctx = JobCtx {
        id: q.id,
        cancel: cancel.clone(),
        log: Mutex::new(String::new()),
        last_progress: Mutex::new(None),
        shared: shared.clone(),
    };
    let started = Utc::now();
    let Some(job) = shared.update(q.id, |j| {
        j.state = JobState::Running;
        j.started = Some(started);
        j.progress.phase = "starting".into();
    }) else {
        return;
    };
    ctx.log(&format!("{} started", job.title));

    let (state, error, bytes) = if cancel.is_cancelled() {
        (JobState::Cancelled, None, 0)
    } else {
        let body = q.body;
        match catch_unwind(AssertUnwindSafe(|| body(&ctx))) {
            Ok(Ok(outcome)) if !cancel.is_cancelled() => (JobState::Succeeded, None, outcome.bytes),
            Ok(Ok(outcome)) => (JobState::Cancelled, None, outcome.bytes),
            Ok(Err(e)) if cancel.is_cancelled() || is_cancelled(&e) => (JobState::Cancelled, None, 0),
            Ok(Err(e)) => (JobState::Failed, Some(format!("{e:#}")), 0),
            Err(panic) => {
                let msg = panic
                    .downcast_ref::<&str>()
                    .map(|s| s.to_string())
                    .or_else(|| panic.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "unknown panic".into());
                (JobState::Failed, Some(format!("internal error: {msg}")), 0)
            }
        }
    };
    match (&state, &error) {
        (JobState::Failed, Some(e)) => ctx.log(&format!("failed: {e}")),
        (JobState::Cancelled, _) => ctx.log("cancelled"),
        _ => ctx.log("finished successfully"),
    }

    let finished = Utc::now();
    let log = std::mem::take(&mut *ctx.log.lock());
    let path = shared.store.paths().job_log(q.id);
    if let Err(e) = std::fs::write(&path, log) {
        tracing::warn!("could not write {}: {e}", path.display());
    }
    if let Some(job) = shared.update(q.id, |j| {
        j.state = state;
        j.finished = Some(finished);
        j.error = error.clone();
        if state == JobState::Succeeded {
            j.progress.phase = "done".into();
        }
    }) {
        shared.store.append_history(HistoryEntry {
            job_id: job.id,
            kind: job.kind,
            title: job.title,
            state,
            started,
            finished: Some(finished),
            bytes,
            error,
        });
    }
    shared.prune();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::Paths;

    type Events = Arc<Mutex<Vec<(String, Value)>>>;

    fn setup() -> (tempfile::TempDir, Arc<Jobs>, Events) {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path());
        paths.create(false).unwrap();
        let store = Arc::new(Store::load(&paths));
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        let jobs = Jobs::new(store, Arc::new(move |m: &str, v: Value| sink.lock().push((m.to_string(), v))));
        (dir, jobs, events)
    }

    fn wait(jobs: &Jobs, id: Uuid) -> Job {
        for _ in 0..500 {
            let j = jobs.get(id).unwrap();
            if is_finished(j.state) {
                return j;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("job did not finish");
    }

    fn spec(exclusive: bool, body: impl FnOnce(&JobCtx) -> anyhow::Result<JobOutcome> + Send + 'static) -> JobSpec {
        JobSpec { kind: JobKind::Verify, title: "t".into(), exclusive, definition: None, body: Box::new(body) }
    }

    #[test]
    fn states_history_and_logs() {
        let (_dir, jobs, events) = setup();
        let ok = jobs
            .submit(spec(true, |ctx| {
                ctx.phase("work", "x");
                ctx.log("hello");
                Ok(JobOutcome { bytes: 42 })
            }))
            .unwrap();
        let failed = jobs.submit(spec(false, |_| anyhow::bail!("boom"))).unwrap();
        let panicked = jobs.submit(spec(false, |_| panic!("kaboom"))).unwrap();
        assert_eq!(wait(&jobs, ok).state, JobState::Succeeded);
        let f = wait(&jobs, failed);
        assert_eq!(f.state, JobState::Failed);
        assert_eq!(f.error.as_deref(), Some("boom"));
        let p = wait(&jobs, panicked);
        assert!(p.error.unwrap().contains("kaboom"));
        assert!(jobs.log_of(ok).unwrap().contains("hello"));
        let h = jobs.shared.store.history(10);
        assert_eq!(h.len(), 3);
        assert!(h.iter().any(|e| e.job_id == ok && e.bytes == 42));
        let ev = events.lock();
        assert!(ev.iter().any(|(m, _)| m == notifications::JOB_PROGRESS));
        assert!(ev.iter().filter(|(m, _)| m == notifications::JOB_STATE).count() >= 9);
        drop(ev);
        jobs.shutdown();
    }

    #[test]
    fn cancellation_and_definition_lock() {
        let (_dir, jobs, _) = setup();
        let def = Uuid::new_v4();
        let mut s = spec(true, |ctx| {
            while !ctx.cancel.is_cancelled() {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(engine::Error::Cancelled.into())
        });
        s.definition = Some(def);
        let id = jobs.submit(s).unwrap();
        assert!(jobs.is_definition_active(def));
        let mut again = spec(true, |_| Ok(JobOutcome { bytes: 0 }));
        again.definition = Some(def);
        assert_eq!(jobs.submit(again).unwrap_err().code, ipc::proto::BUSY);
        // Queued behind the running exclusive job; cancelled before it starts.
        let queued = jobs.submit(spec(true, |_| Ok(JobOutcome { bytes: 0 }))).unwrap();
        jobs.cancel(queued).unwrap();
        jobs.cancel(id).unwrap();
        assert_eq!(wait(&jobs, id).state, JobState::Cancelled);
        assert_eq!(wait(&jobs, queued).state, JobState::Cancelled);
        assert!(!jobs.is_definition_active(def));
        assert_eq!(jobs.cancel(Uuid::new_v4()).unwrap_err().code, ipc::proto::NOT_FOUND);
        jobs.shutdown();
    }
}
