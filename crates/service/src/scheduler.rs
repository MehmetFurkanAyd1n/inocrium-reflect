//! The scheduler thread: starts scheduled backups when their schedule fires.
//!
//! Every [`TICK`] (or when woken) each enabled, scheduled definition is checked against its
//! anchor. Occurrences missed by more than [`MISSED_GRACE`] (service stopped, machine off) run
//! only when the definition asks for it (`run_missed`); otherwise they are skipped.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use chrono::{DateTime, Local, TimeZone, Utc};
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use ipc::proto::RpcError;
use parking_lot::Mutex;
use uuid::Uuid;

use crate::schedule;
use crate::store::{Store, StoredDefinition};

/// How often definitions are re-evaluated without being woken.
const TICK: Duration = Duration::from_secs(15);
/// How late an occurrence may be noticed and still count as on time.
const MISSED_GRACE: chrono::Duration = chrono::Duration::minutes(5);

/// Submits a scheduled backup job of a definition; returns the job id.
pub type RunFn = Arc<dyn Fn(Uuid) -> Result<Uuid, RpcError> + Send + Sync>;
/// Whether a definition has a queued or running job.
pub type ActiveFn = Arc<dyn Fn(Uuid) -> bool + Send + Sync>;

/// What to do with a definition right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// An occurrence is due: start a backup.
    Run,
    /// An occurrence was missed and the definition does not run missed backups.
    SkipMissed,
    /// Nothing is due.
    Wait,
}

/// Decides what to do with `d` at `now`, returning the occurrence that is due (if any).
pub fn decide<Tz: TimeZone>(d: &StoredDefinition, now: &DateTime<Tz>) -> (Decision, Option<DateTime<Tz>>) {
    let (Some(s), Some(anchor)) = (d.def.schedule.as_ref().filter(|_| d.def.enabled), d.anchor) else {
        return (Decision::Wait, None);
    };
    let Some(occurrence) = schedule::previous_run(s, now) else { return (Decision::Wait, None) };
    if occurrence <= anchor.with_timezone(&now.timezone()) {
        return (Decision::Wait, None);
    }
    if d.def.run_missed || now.clone() - occurrence.clone() <= MISSED_GRACE {
        (Decision::Run, Some(occurrence))
    } else {
        (Decision::SkipMissed, Some(occurrence))
    }
}

enum Msg {
    Wake,
    Stop,
}

pub struct Scheduler {
    tx: Sender<Msg>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl Scheduler {
    /// Starts the scheduler thread; it evaluates definitions immediately and then every tick.
    pub fn start(store: Arc<Store>, run: RunFn, active: ActiveFn) -> Scheduler {
        let (tx, rx) = crossbeam_channel::unbounded();
        let thread = std::thread::Builder::new()
            .name("scheduler".into())
            .spawn(move || run_loop(&store, &run, &active, &rx))
            .expect("could not start the scheduler thread");
        Scheduler { tx, thread: Mutex::new(Some(thread)) }
    }

    /// Re-evaluates definitions now.
    pub fn wake(&self) {
        let _ = self.tx.send(Msg::Wake);
    }

    /// Stops the scheduler thread and waits for it. Idempotent.
    pub fn stop(&self) {
        let _ = self.tx.send(Msg::Stop);
        let Some(thread) = self.thread.lock().take() else { return };
        // The last owner may be dropped on the scheduler thread itself (via `run`).
        if thread.thread().id() != std::thread::current().id() {
            let _ = thread.join();
        }
    }
}

impl Drop for Scheduler {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run_loop(store: &Store, run: &RunFn, active: &ActiveFn, rx: &Receiver<Msg>) {
    loop {
        if catch_unwind(AssertUnwindSafe(|| tick(store, run, active, &Local::now()))).is_err() {
            tracing::error!("the scheduler panicked while evaluating definitions");
        }
        match rx.recv_timeout(TICK) {
            Ok(Msg::Stop) | Err(RecvTimeoutError::Disconnected) => return,
            Ok(Msg::Wake) | Err(RecvTimeoutError::Timeout) => {}
        }
        // Coalesce queued wakes, but never miss a stop.
        if rx.try_iter().any(|m| matches!(m, Msg::Stop)) {
            return;
        }
    }
}

fn tick<Tz: TimeZone>(store: &Store, run: &RunFn, active: &ActiveFn, now: &DateTime<Tz>) {
    for d in store.all_stored() {
        let id = d.def.id;
        match decide(&d, now) {
            (Decision::Wait, _) | (_, None) => {}
            (Decision::SkipMissed, Some(occurrence)) => {
                tracing::info!("missed occurrence skipped: definition {id} ({}) was due at {occurrence:?}", d.def.name);
                store.advance_anchor(id, occurrence.with_timezone(&Utc));
            }
            (Decision::Run, Some(_)) => {
                // Retried next tick once the current job has finished.
                if active(id) {
                    continue;
                }
                let result = catch_unwind(AssertUnwindSafe(|| run(id)))
                    .unwrap_or_else(|_| Err(RpcError::new(ipc::proto::INTERNAL_ERROR, "submitting the job panicked")));
                match result {
                    Ok(job) => {
                        tracing::info!("scheduled backup of definition {id} ({}) started as job {job}", d.def.name)
                    }
                    Err(e) => {
                        tracing::warn!(
                            "scheduled backup of definition {id} ({}) could not start: {}",
                            d.def.name,
                            e.message
                        )
                    }
                }
                // Even on failure, so a broken definition does not retry every tick.
                store.mark_run(id, Utc::now(), true);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::Paths;
    use chrono::FixedOffset;
    use ipc::proto::{BackupMode, Definition, DiskSelection, Schedule, ScheduleKind};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn def(run_missed: bool) -> Definition {
        Definition {
            id: Uuid::nil(),
            name: "System disk".into(),
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
            run_missed,
            last_run: None,
            next_run: None,
        }
    }

    fn stored(run_missed: bool, anchor: Option<DateTime<Utc>>) -> StoredDefinition {
        StoredDefinition { def: def(run_missed), password_blob: None, anchor }
    }

    fn tz() -> FixedOffset {
        FixedOffset::east_opt(3 * 3600).unwrap()
    }

    fn utc(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn disabled_or_unscheduled_waits() {
        let now = tz().with_ymd_and_hms(2026, 10, 14, 2, 1, 0).unwrap();
        let mut d = stored(true, Some(utc("2026-10-01T00:00:00Z")));
        d.def.enabled = false;
        assert_eq!(decide(&d, &now), (Decision::Wait, None));
        let mut d = stored(true, Some(utc("2026-10-01T00:00:00Z")));
        d.def.schedule = None;
        assert_eq!(decide(&d, &now), (Decision::Wait, None));
    }

    #[test]
    fn no_anchor_waits() {
        let now = tz().with_ymd_and_hms(2026, 10, 14, 2, 1, 0).unwrap();
        assert_eq!(decide(&stored(true, None), &now), (Decision::Wait, None));
    }

    #[test]
    fn not_yet_due_waits() {
        // 02:00 at +03:00 is 23:00Z the day before; the anchor already covers it.
        let now = tz().with_ymd_and_hms(2026, 10, 14, 2, 1, 0).unwrap();
        assert_eq!(decide(&stored(false, Some(utc("2026-10-13T23:00:00Z"))), &now), (Decision::Wait, None));
        assert_eq!(decide(&stored(false, Some(utc("2026-10-13T23:00:30Z"))), &now), (Decision::Wait, None));
        // Before today's occurrence, yesterday's is covered by an anchor from yesterday.
        let early = tz().with_ymd_and_hms(2026, 10, 14, 1, 59, 0).unwrap();
        assert_eq!(decide(&stored(false, Some(utc("2026-10-13T12:00:00Z"))), &early), (Decision::Wait, None));
    }

    #[test]
    fn due_within_grace_runs() {
        let occurrence = tz().with_ymd_and_hms(2026, 10, 14, 2, 0, 0).unwrap();
        let anchor = Some(utc("2026-10-13T12:00:00Z"));
        for minutes in [0, 1, 5] {
            let now = occurrence + chrono::Duration::minutes(minutes);
            assert_eq!(decide(&stored(false, anchor), &now), (Decision::Run, Some(occurrence)));
        }
        // The schedule's time is read in the clock's own zone.
        let now = utc("2026-10-14T02:03:00Z");
        assert_eq!(decide(&stored(false, anchor), &now), (Decision::Run, Some(utc("2026-10-14T02:00:00Z"))));
    }

    #[test]
    fn missed_occurrence_skips_or_runs() {
        let occurrence = tz().with_ymd_and_hms(2026, 10, 14, 2, 0, 0).unwrap();
        let now = occurrence + chrono::Duration::minutes(6);
        let anchor = Some(utc("2026-10-10T12:00:00Z"));
        assert_eq!(decide(&stored(false, anchor), &now), (Decision::SkipMissed, Some(occurrence)));
        assert_eq!(decide(&stored(true, anchor), &now), (Decision::Run, Some(occurrence)));
    }

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path());
        paths.create(false).unwrap();
        let store = Store::load(&paths);
        (dir, store)
    }

    /// Today's 02:00Z occurrence three days from now, so it is after the save-time anchor.
    fn future_occurrence() -> DateTime<Utc> {
        let day = (Utc::now() + chrono::Duration::days(3)).date_naive();
        day.and_hms_opt(2, 0, 0).unwrap().and_utc()
    }

    #[test]
    fn tick_runs_and_marks() {
        let (_dir, store) = store();
        let id = store.save_definition(def(false), None).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let run: RunFn = Arc::new(move |_| {
            c.fetch_add(1, Ordering::SeqCst);
            Err(RpcError::new(ipc::proto::INTERNAL_ERROR, "no disk"))
        });
        let busy: ActiveFn = Arc::new(|_| true);
        let idle: ActiveFn = Arc::new(|_| false);
        let now = future_occurrence() + chrono::Duration::minutes(1);

        tick(&store, &run, &busy, &now);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        tick(&store, &run, &idle, &now);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(store.stored(id).unwrap().def.last_run.is_some());
        // A failed submission still counts. `mark_run` stamps the real clock, which in this test
        // is before the simulated occurrence; move the anchor as the real clock would have.
        store.advance_anchor(id, now);
        tick(&store, &run, &idle, &now);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn tick_skips_missed_without_recording_a_run() {
        let (_dir, store) = store();
        let id = store.save_definition(def(false), None).unwrap();
        let run: RunFn = Arc::new(|_| panic!("must not run"));
        let idle: ActiveFn = Arc::new(|_| false);
        let now = future_occurrence() + chrono::Duration::hours(6);

        tick(&store, &run, &idle, &now);
        let d = store.stored(id).unwrap();
        assert_eq!(d.anchor, Some(future_occurrence()));
        assert!(d.def.last_run.is_none());
        assert_eq!(decide(&d, &now), (Decision::Wait, None));
    }

    #[test]
    fn panicking_run_is_contained() {
        let (_dir, store) = store();
        let id = store.save_definition(def(true), None).unwrap();
        let run: RunFn = Arc::new(|_| panic!("boom"));
        let idle: ActiveFn = Arc::new(|_| false);
        tick(&store, &run, &idle, &(future_occurrence() + chrono::Duration::minutes(1)));
        assert!(store.stored(id).unwrap().def.last_run.is_some());
    }

    #[test]
    fn start_wake_stop() {
        let (_dir, store) = store();
        let run: RunFn = Arc::new(|_| Ok(Uuid::new_v4()));
        let idle: ActiveFn = Arc::new(|_| false);
        let scheduler = Scheduler::start(Arc::new(store), run, idle);
        scheduler.wake();
        scheduler.stop();
        scheduler.stop();
    }
}
