#[cfg(any(windows, test))]
mod args;
#[cfg(windows)]
mod direct;
#[cfg(windows)]
mod output;
#[cfg(windows)]
mod password;
#[cfg(windows)]
mod progress;
#[cfg(windows)]
mod remote;
#[cfg(windows)]
mod skill;
#[cfg(all(windows, test))]
mod test_support;

#[cfg(windows)]
fn main() {
    use clap::Parser;
    let cli = args::Cli::parse();
    let code = match run(&cli) {
        Ok(code) => code,
        Err(error) => {
            if cli.json {
                eprintln!("{}", serde_json::json!({"error": format!("{error:#}")}));
            } else {
                eprintln!("Error: {error:#}");
            }
            1
        }
    };
    std::process::exit(code);
}

#[cfg(windows)]
fn run(cli: &args::Cli) -> anyhow::Result<i32> {
    use std::time::Duration;
    if let args::Command::Skill { topic, path } = &cli.command {
        return skill::run(*topic, *path);
    }
    let cancel = engine::Cancel::new();
    let signal = cancel.clone();
    ctrlc::set_handler(move || signal.cancel())?;
    let mut passwords = password::Passwords::new(cli.password_stdin)?;
    if !cli.direct {
        let timeout = if ipc::client::is_service_running() { 3 } else { 1 };
        match ipc::client::Client::connect(ipc::proto::PIPE_NAME, Duration::from_secs(timeout)) {
            Ok(client) => return remote::run(cli, &client, &mut passwords, &cancel),
            Err(error) => eprintln!("Service unavailable ({error}); trying direct mode."),
        }
    }
    if matches!(&cli.command, args::Command::Viboot { .. }) {
        anyhow::bail!("viBoot requires the background service to maintain a persistent VM and writable mount");
    }
    if matches!(&cli.command, args::Command::Jobs { .. } | args::Command::History { .. } | args::Command::Logs { .. }) {
        anyhow::bail!("This command requires the background service");
    }
    anyhow::ensure!(
        winio::is_elevated() || matches!(&cli.command, args::Command::Info { .. }),
        "Direct mode requires administrator privileges. Start the service or run an elevated console."
    );
    anyhow::ensure!(!cli.detach, "--detach requires the service; direct operations run in this process");
    match direct::run(cli, &mut passwords, &cancel) {
        Err(error) if error.downcast_ref::<engine::Error>().is_some_and(|e| matches!(e, engine::Error::Cancelled)) => {
            Ok(130)
        }
        result => result,
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("inocrium reflect requires Windows");
    std::process::exit(1);
}
