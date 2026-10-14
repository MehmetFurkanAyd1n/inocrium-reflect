//! Progress reporting and cancellation shared by long-running operations.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

/// Cooperative cancellation flag; clones share the same flag.
#[derive(Debug, Clone, Default)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
    pub fn check(&self) -> Result<()> {
        if self.is_cancelled() {
            Err(Error::Cancelled)
        } else {
            Ok(())
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Progress {
    /// Short machine-readable step, e.g. `"backup"`, `"verify"`, `"restore"`.
    pub phase: String,
    /// Human-readable detail (current partition, ...).
    pub detail: String,
    pub bytes_done: u64,
    pub bytes_total: u64,
    /// Bytes written to the destination (stored image size for backups).
    pub bytes_written: u64,
    /// Average throughput since the start, bytes per second.
    pub rate: u64,
}

impl Progress {
    pub fn percent(&self) -> f64 {
        if self.bytes_total == 0 {
            100.0
        } else {
            self.bytes_done as f64 * 100.0 / self.bytes_total as f64
        }
    }
}

/// Progress callback. Called from the operation's coordinating thread.
pub type ProgressFn<'a> = &'a (dyn Fn(&Progress) + Sync);

/// No-op progress callback.
pub fn no_progress(_: &Progress) {}

/// Rate-limits progress callbacks and fills in the rate.
pub(crate) struct Reporter<'a> {
    f: ProgressFn<'a>,
    started: Instant,
    last: Option<Instant>,
    pub p: Progress,
}

impl<'a> Reporter<'a> {
    pub fn new(f: ProgressFn<'a>, phase: &str, bytes_total: u64) -> Self {
        let p = Progress { phase: phase.into(), bytes_total, ..Default::default() };
        Self { f, started: Instant::now(), last: None, p }
    }

    /// Reports if at least 250 ms passed since the last report.
    pub fn tick(&mut self) {
        let now = Instant::now();
        if self.last.is_none_or(|l| now - l >= Duration::from_millis(250)) {
            self.emit(now);
        }
    }

    pub fn force(&mut self) {
        self.emit(Instant::now());
    }

    fn emit(&mut self, now: Instant) {
        let secs = (now - self.started).as_secs_f64();
        self.p.rate = if secs > 0.0 { (self.p.bytes_done as f64 / secs) as u64 } else { 0 };
        self.last = Some(now);
        (self.f)(&self.p);
    }
}
