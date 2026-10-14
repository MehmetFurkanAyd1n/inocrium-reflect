#![cfg(windows)]
use ipc::{
    proto::{self, Job, JobKind, JobState, JobStateChanged, Progress, RpcError},
    server::{ClientInfo, Handler, Notifier, Server, ServerOptions},
};
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::{
    process::{Command, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};

struct Fixture {
    notifier: Mutex<Option<Notifier>>,
    restart: crossbeam_channel::Sender<()>,
}
impl Handler for Fixture {
    fn call(&self, method: &str, params: Value, _: &ClientInfo) -> Result<Value, RpcError> {
        match method {
            "service.info" => Ok(json!({"version":"interop", "protocolVersion":1, "machine":"test"})),
            "echo" => Ok(params["value"].clone()),
            "fail" => Err(RpcError::new(proto::BAD_PASSWORD, "Bad password")),
            "notify" => {
                let job = Job {
                    id: uuid::Uuid::new_v4(),
                    kind: JobKind::Backup,
                    title: "test".into(),
                    state: JobState::Running,
                    progress: Progress {
                        phase: "copy".into(),
                        detail: "test".into(),
                        bytes_done: 123,
                        bytes_total: 456,
                        bytes_written: 100,
                        rate: 99,
                    },
                    started: Some(chrono::Utc::now()),
                    finished: None,
                    error: None,
                };
                let lock = self.notifier.lock();
                let notifier = lock.as_ref().unwrap();
                notifier.broadcast(
                    proto::notifications::JOB_PROGRESS,
                    proto::JobProgress { job_id: job.id, progress: job.progress.clone() },
                )?;
                notifier.broadcast(proto::notifications::JOB_STATE, JobStateChanged { job })?;
                notifier.broadcast(proto::notifications::MOUNTS_CHANGED, proto::EmptyParams {})?;
                notifier.broadcast(proto::notifications::DEFINITIONS_CHANGED, proto::EmptyParams {})?;
                Ok(Value::Null)
            }
            "restart" => {
                self.restart.try_send(()).unwrap();
                Ok(Value::Null)
            }
            _ => Err(RpcError::method_not_found()),
        }
    }
}

#[test]
#[ignore = "Requires prebuilt .NET smoke project; see PROTOCOL.md"]
fn stream_json_rpc_interoperability_and_reconnect() {
    let short_name = format!("inocrium-dotnet-test-{}", uuid::Uuid::new_v4());
    let name = format!(r"\\.\pipe\{short_name}");
    let (restart, receiver) = crossbeam_channel::bounded(1);
    let fixture = Arc::new(Fixture { notifier: Mutex::new(None), restart });
    let options = ServerOptions { require_admin: false, sddl: "D:P(A;;GA;;;OW)(A;;GA;;;SY)".into() };
    let server = Server::start_with_options(&name, fixture.clone(), options.clone()).unwrap();
    *fixture.notifier.lock() = Some(server.notifier());
    let project = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/dotnet/ProtocolSmoke.csproj");
    let mut child = Command::new("dotnet")
        .args(["run", "--no-build", "-c", "Release", "--project"])
        .arg(project)
        .args(["--", &short_name])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    if receiver.recv_timeout(Duration::from_secs(20)).is_err() {
        let _ = child.kill();
        let output = child.wait_with_output().unwrap();
        panic!(
            "Interop did not request restart: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    server.shutdown();
    let restarted = Server::start_with_options(&name, fixture.clone(), options).unwrap();
    *fixture.notifier.lock() = Some(restarted.notifier());
    let deadline = Instant::now() + Duration::from_secs(25);
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let output = child.wait_with_output().unwrap();
    println!("{}", String::from_utf8_lossy(&output.stdout));
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    restarted.shutdown();
}
