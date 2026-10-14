use anyhow::{bail, Result};
use ipc::{
    client::Client,
    proto::{self, methods, notifications, EmptyParams, IdParams, Job, JobState},
};
use std::{
    io::{self, Write},
    time::{Duration, Instant},
};
use uuid::Uuid;

pub fn render(p: &engine::Progress, json: bool) {
    render_percent(p, p.percent().min(100.0), json);
}

fn render_percent(p: &engine::Progress, percent: f64, json: bool) {
    if json {
        eprintln!("{}", serde_json::json!({"type":"progress", "progress":p, "percent":percent}));
    } else {
        eprint!(
            "\r{:<16} {:6.2}%  {} / {} bytes  {} bytes/s  {}\x1b[K",
            p.phase, percent, p.bytes_done, p.bytes_total, p.rate, p.detail
        );
        let _ = io::stderr().flush();
    }
}

pub fn render_blocks(done: u64, total: u64, json: bool) {
    let percent = if total == 0 { 100.0 } else { done as f64 * 100.0 / total as f64 };
    render_percent(
        &engine::Progress { phase: "verify".into(), detail: format!("{done} / {total} blocks"), ..Default::default() },
        percent.min(100.0),
        json,
    );
}

pub fn finish(json: bool) {
    if !json {
        eprintln!();
    }
}

fn render_remote(p: &proto::Progress, json: bool) {
    render(
        &engine::Progress {
            phase: p.phase.clone(),
            detail: p.detail.clone(),
            bytes_done: p.bytes_done,
            bytes_total: p.bytes_total,
            bytes_written: p.bytes_written,
            rate: p.rate,
        },
        json,
    );
}

fn terminal(job: &Job) -> Option<i32> {
    match job.state {
        JobState::Succeeded => Some(0),
        JobState::Failed => Some(1),
        JobState::Cancelled => Some(130),
        _ => None,
    }
}

pub fn follow(client: &Client, id: Uuid, json: bool, cancel: &engine::Cancel) -> Result<i32> {
    let notifications_rx = client.notifications();
    let mut refreshed = Instant::now() - Duration::from_secs(5);
    let mut cancel_sent = false;
    loop {
        // Refresh immediately for jobs that finished before subscription, then periodically.
        if refreshed.elapsed() >= Duration::from_secs(5) {
            let jobs: Vec<Job> = client.call(methods::JOBS_LIST, EmptyParams {})?;
            let job = jobs
                .iter()
                .find(|job| job.id == id)
                .ok_or_else(|| anyhow::anyhow!("Job {id} is no longer available"))?;
            render_remote(&job.progress, json);
            if let Some(code) = terminal(job) {
                return complete(job, code, json);
            }
            refreshed = Instant::now();
        }
        if cancel.is_cancelled() && !cancel_sent {
            client.call::<_, ()>(methods::JOBS_CANCEL, IdParams { id })?;
            cancel_sent = true;
        }
        if let Ok(notification) = notifications_rx.recv_timeout(Duration::from_millis(200)) {
            match notification.method.as_str() {
                notifications::JOB_PROGRESS => {
                    let update: proto::JobProgress = serde_json::from_value(notification.params)?;
                    if update.job_id == id {
                        render_remote(&update.progress, json);
                    }
                }
                notifications::JOB_STATE => {
                    let update: proto::JobStateChanged = serde_json::from_value(notification.params)?;
                    if update.job.id == id {
                        if let Some(code) = terminal(&update.job) {
                            return complete(&update.job, code, json);
                        }
                    }
                }
                _ => {}
            }
        }
        if !client.is_connected() {
            bail!("Service disconnected while following job {id}");
        }
    }
}

fn complete(job: &Job, code: i32, json: bool) -> Result<i32> {
    finish(json);
    if let Some(error) = &job.error {
        eprintln!("Job {} failed: {error}", job.id);
    }
    if json {
        crate::output::value(job, true)?;
    } else {
        eprintln!("Job {}: {:?}", job.id, job.state);
    }
    Ok(code)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ipc::server::{ClientInfo, Handler, Notifier};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    };

    fn job(state: JobState) -> Job {
        Job {
            id: Uuid::new_v4(),
            kind: proto::JobKind::Verify,
            title: "Test".into(),
            state,
            progress: proto::Progress {
                phase: "verify".into(),
                detail: String::new(),
                bytes_done: 0,
                bytes_total: 512,
                bytes_written: 0,
                rate: 0,
            },
            started: None,
            finished: None,
            error: None,
        }
    }

    struct Jobs {
        job: Mutex<Job>,
        notifier: Mutex<Option<Notifier>>,
        cancels: AtomicUsize,
    }
    impl Handler for Jobs {
        fn call(
            &self,
            method: &str,
            params: serde_json::Value,
            _: &ClientInfo,
        ) -> std::result::Result<serde_json::Value, proto::RpcError> {
            let mut job = self.job.lock().unwrap();
            match method {
                methods::JOBS_LIST => Ok(serde_json::json!([*job])),
                methods::JOBS_CANCEL => {
                    assert_eq!(params["id"], job.id.to_string());
                    self.cancels.fetch_add(1, Ordering::SeqCst);
                    job.state = JobState::Cancelled;
                    let notifier = self.notifier.lock().unwrap();
                    let notifier = notifier.as_ref().unwrap();
                    notifier.broadcast(
                        notifications::JOB_PROGRESS,
                        proto::JobProgress { job_id: job.id, progress: job.progress.clone() },
                    )?;
                    notifier.broadcast(notifications::JOB_STATE, proto::JobStateChanged { job: job.clone() })?;
                    Ok(serde_json::Value::Null)
                }
                _ => Err(proto::RpcError::method_not_found()),
            }
        }
    }

    #[test]
    fn completed_jobs_exit_without_cancelling() {
        for (state, expected) in [(JobState::Succeeded, 0), (JobState::Failed, 1), (JobState::Cancelled, 130)] {
            let job = job(state);
            let id = job.id;
            let handler =
                Arc::new(Jobs { job: Mutex::new(job), notifier: Mutex::new(None), cancels: AtomicUsize::new(0) });
            let (server, client) = crate::test_support::loopback(handler.clone());
            let cancel = engine::Cancel::new();
            cancel.cancel(); // A late Ctrl+C must preserve a job that has already completed.
            let code = follow(&client, id, false, &cancel).unwrap();
            server.shutdown();
            assert_eq!(code, expected);
            assert_eq!(handler.cancels.load(Ordering::SeqCst), 0);
        }
    }

    #[test]
    fn cancelled_follow_sends_cancel_and_consumes_terminal_notification() {
        let job = job(JobState::Running);
        let id = job.id;
        let handler = Arc::new(Jobs { job: Mutex::new(job), notifier: Mutex::new(None), cancels: AtomicUsize::new(0) });
        let (server, client) = crate::test_support::loopback(handler.clone());
        *handler.notifier.lock().unwrap() = Some(server.notifier());
        let cancel = engine::Cancel::new();
        cancel.cancel();
        let code = follow(&client, id, false, &cancel).unwrap();
        server.shutdown();
        assert_eq!(code, 130);
        assert_eq!(handler.cancels.load(Ordering::SeqCst), 1);
    }
}
