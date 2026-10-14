//! inocrium reflect background service ("InocriumReflect", LocalSystem).
//!
//! Started by the service control manager without arguments. `--console [--data-dir DIR]`
//! runs the same service in the foreground (as an administrator) until Ctrl+C.

#![cfg_attr(not(windows), allow(dead_code))]
#[cfg(windows)]
mod app;
#[cfg(windows)]
mod disks;
#[cfg(windows)]
mod handler;
#[cfg(windows)]
mod images;
#[cfg(windows)]
mod jobs;
#[cfg(windows)]
mod ops;
#[cfg(windows)]
mod paths;
mod schedule;
#[cfg(windows)]
mod scheduler;
#[cfg(windows)]
mod store;

#[cfg(windows)]
fn main() {
    if let Err(e) = service::main() {
        eprintln!("inocrium-service: {e:#}");
        std::process::exit(1);
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("inocrium-service runs on Windows only");
    std::process::exit(1);
}

#[cfg(windows)]
mod service {
    use std::ffi::OsString;
    use std::fs::OpenOptions;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Duration;

    use anyhow::{bail, Context};
    use crossbeam_channel::{bounded, Receiver};
    use ipc::proto::PIPE_NAME;
    use ipc::server::{Server, ServiceHandler};
    use windows_service::service::{
        ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus, ServiceType,
    };
    use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
    use windows_service::{define_windows_service, service_dispatcher};

    use crate::app::App;
    use crate::handler::Handler;
    use crate::ops;
    use crate::paths::Paths;

    pub const SERVICE_NAME: &str = "InocriumReflect";
    /// The service log is renamed to `service.log.1` when it grows past this.
    const LOG_ROTATE_BYTES: u64 = 10 << 20;

    pub fn main() -> anyhow::Result<()> {
        let mut args = std::env::args().skip(1);
        let mut console = false;
        let mut data_dir = None;
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--console" => console = true,
                "--data-dir" => data_dir = Some(PathBuf::from(args.next().context("--data-dir needs a directory")?)),
                "--help" | "-h" => {
                    println!("usage: inocrium-service [--console [--data-dir DIR]]");
                    return Ok(());
                }
                other => bail!("unknown argument {other}"),
            }
        }
        if !console {
            if data_dir.is_some() {
                bail!("--data-dir requires --console");
            }
            return service_dispatcher::start(SERVICE_NAME, ffi_service_main)
                .context("not started by the service control manager (use --console to run in the foreground)");
        }
        let (tx, rx) = bounded(1);
        ctrlc::set_handler(move || {
            let _ = tx.try_send(());
        })
        .context("installing the Ctrl+C handler")?;
        serve(data_dir.unwrap_or_else(Paths::default_root), true, rx, || {
            eprintln!("inocrium-service is running on {PIPE_NAME}; press Ctrl+C to stop");
        })
    }

    define_windows_service!(ffi_service_main, service_main);

    fn service_main(_args: Vec<OsString>) {
        if let Err(e) = run_service() {
            tracing::error!("service failed: {e:#}");
        }
    }

    fn status(state: ServiceState, exit: u32, wait: Duration) -> ServiceStatus {
        ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: state,
            controls_accepted: if state == ServiceState::Running {
                ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN
            } else {
                ServiceControlAccept::empty()
            },
            exit_code: ServiceExitCode::Win32(exit),
            checkpoint: 0,
            wait_hint: wait,
            process_id: None,
        }
    }

    fn run_service() -> anyhow::Result<()> {
        let (tx, rx) = bounded(1);
        let handle = service_control_handler::register(SERVICE_NAME, move |event| match event {
            ServiceControl::Stop | ServiceControl::Shutdown => {
                let _ = tx.try_send(());
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        })?;
        handle.set_service_status(status(ServiceState::StartPending, 0, Duration::from_secs(30)))?;
        let result = serve(Paths::default_root(), false, rx, || {
            let _ = handle.set_service_status(status(ServiceState::Running, 0, Duration::ZERO));
        });
        let exit = if result.is_ok() { 0 } else { 1 };
        handle.set_service_status(status(ServiceState::Stopped, exit, Duration::ZERO))?;
        result
    }

    fn init_logging(paths: &Paths, console: bool) {
        let path = paths.logs().join("service.log");
        if std::fs::metadata(&path).is_ok_and(|m| m.len() > LOG_ROTATE_BYTES) {
            let _ = std::fs::rename(&path, paths.logs().join("service.log.1"));
        }
        let filter = tracing_subscriber::EnvFilter::try_from_env("INOCRIUM_LOG")
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
        let file = OpenOptions::new().create(true).append(true).open(&path);
        let builder = tracing_subscriber::fmt().with_env_filter(filter).with_ansi(false);
        let result = match (file, console) {
            (Ok(file), false) => builder.with_writer(std::sync::Mutex::new(file)).try_init(),
            _ => builder.with_writer(std::io::stderr).try_init(),
        };
        let _ = result;
    }

    /// Runs the service until `stop` fires; `running` is called once the pipe is listening.
    fn serve(root: PathBuf, console: bool, stop: Receiver<()>, running: impl FnOnce()) -> anyhow::Result<()> {
        let paths = Paths::new(root);
        paths.create(true).with_context(|| format!("creating {}", paths.root.display()))?;
        init_logging(&paths, console);
        tracing::info!("inocrium-service {} starting (data in {})", env!("CARGO_PKG_VERSION"), paths.root.display());

        let app = App::new(&paths);
        app.startup_cleanup();
        let server = Server::start(PIPE_NAME, Arc::new(ServiceHandler(Handler(app.clone()))))
            .with_context(|| format!("listening on {PIPE_NAME} (is another instance running?)"))?;
        app.attach(server.notifier());
        ops::resume_interrupted(&app);
        tracing::info!("listening on {PIPE_NAME}");
        running();

        let _ = stop.recv();
        tracing::info!("stopping");
        server.shutdown();
        app.shutdown();
        tracing::info!("stopped");
        Ok(())
    }
}
