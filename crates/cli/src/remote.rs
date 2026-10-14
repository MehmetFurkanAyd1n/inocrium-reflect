use crate::{
    args::{Cli, Command, DefinitionCommand, JobsCommand, VibootCommand},
    output,
    password::{self, Passwords},
    progress,
};
use anyhow::Result;
use ipc::{
    client::{Client, Error},
    proto::{self, methods, *},
};
use serde::{de::DeserializeOwned, Serialize};
use std::time::Duration;
use uuid::Uuid;

fn request<P: Serialize, R: DeserializeOwned>(
    client: &Client,
    method: &str,
    params: P,
    passwords: &mut Passwords,
    timeout: Duration,
) -> Result<R> {
    let mut params = serde_json::to_value(params)?;
    for attempt in 0..3 {
        match client.call_with_timeout(method, &params, timeout) {
            Ok(value) => return Ok(value),
            Err(Error::Rpc(error))
                if matches!(error.code, proto::PASSWORD_REQUIRED | proto::BAD_PASSWORD) && attempt < 2 =>
            {
                let password =
                    if error.code == proto::BAD_PASSWORD { passwords.retry()? } else { passwords.required(false)? };
                params["password"] = serde_json::Value::String(password);
            }
            Err(error) => return Err(error.into()),
        }
    }
    unreachable!()
}

pub fn run(cli: &Cli, client: &Client, passwords: &mut Passwords, cancel: &engine::Cancel) -> Result<i32> {
    let timeout = Duration::from_secs(30);
    let job: Option<Uuid> = match &cli.command {
        Command::Disks => {
            let disks: Vec<Disk> = client.call(methods::DISKS_LIST, EmptyParams {})?;
            output::disks(&disks, cli.json)?;
            None
        }
        Command::Backup(args) => {
            let disks: Vec<Disk> = client.call(methods::DISKS_LIST, EmptyParams {})?;
            let definition = args.definition(&disks, &winio::hostname(), "Backup", None, false)?;
            let password = if args.encrypt { Some(passwords.required(true)?) } else { None };
            Some(request(
                client,
                methods::BACKUP_START,
                BackupStartParams { definition, password },
                passwords,
                timeout,
            )?)
        }
        Command::Verify { img } => Some(request(
            client,
            methods::IMAGES_VERIFY,
            ImagePasswordParams { path: path(img)?, password: passwords.current() },
            passwords,
            timeout,
        )?),
        Command::Mount { img, writable, disk, partition } => {
            let mount: Mount = request(
                client,
                methods::MOUNTS_MOUNT,
                MountParams {
                    path: path(img)?,
                    password: passwords.current(),
                    disk: *disk,
                    partition: *partition,
                    writable: *writable,
                },
                passwords,
                Duration::from_secs(180),
            )?;
            output::mount(&mount, cli.json)?;
            None
        }
        Command::Unmount { id } => {
            let id = if id == "all" { MountId::All(AllMounts::All) } else { MountId::Id(id.parse()?) };
            client.call::<_, ()>(methods::MOUNTS_UNMOUNT, UnmountParams { id })?;
            output::value(&serde_json::json!({"unmounted":true}), cli.json)?;
            None
        }
        Command::Restore(args) => {
            password::confirm_restore(args.target_disk, args.yes)?;
            Some(request(
                client,
                methods::RESTORE_START,
                RestoreParams {
                    path: path(&args.img)?,
                    password: passwords.current(),
                    disk: args.disk,
                    partition: args.partition,
                    target_disk: args.target_disk,
                    target_partition: args.target_partition,
                    target_offset: args.target_offset,
                    extend: args.extend,
                },
                passwords,
                timeout,
            )?)
        }
        Command::ExportVhdx { img, out, disk } => Some(request(
            client,
            methods::VHDX_EXPORT,
            ExportVhdxParams { path: path(img)?, password: passwords.current(), disk: *disk, output: path(out)? },
            passwords,
            timeout,
        )?),
        Command::Redeploy { disk, drivers } => Some(client.call(
            methods::REDEPLOY_START,
            RedeployParams {
                disk: *disk,
                drivers: if drivers.is_empty() {
                    None
                } else {
                    Some(drivers.iter().map(|d| path(d)).collect::<Result<_>>()?)
                },
            },
        )?),
        Command::Viboot { command } => match command {
            VibootCommand::Start { img, disk, name, memory, cpus } => Some(request(
                client,
                methods::VIBOOT_START,
                VibootStartParams {
                    path: path(img)?,
                    password: passwords.current(),
                    disk: *disk,
                    vm_name: name.clone(),
                    memory_mb: *memory,
                    cpus: *cpus,
                },
                passwords,
                timeout,
            )?),
            VibootCommand::List => {
                let vms: Vec<VirtualMachine> = client.call(methods::VIBOOT_LIST, EmptyParams {})?;
                output::value(&vms, cli.json)?;
                None
            }
            VibootCommand::Stop { name } => {
                client.call::<_, ()>(methods::VIBOOT_STOP, VibootStopParams { name: name.clone() })?;
                output::value(&serde_json::json!({"stopped":name}), cli.json)?;
                None
            }
        },
        Command::Def { command } => match command {
            DefinitionCommand::List => {
                let definitions: Vec<Definition> = client.call(methods::DEFINITIONS_LIST, EmptyParams {})?;
                output::value(&definitions, cli.json)?;
                None
            }
            DefinitionCommand::Run { id } => Some(client.call(methods::DEFINITIONS_RUN, IdParams { id: *id })?),
            DefinitionCommand::Create { name, backup, schedule } => {
                let disks: Vec<Disk> = client.call(methods::DISKS_LIST, EmptyParams {})?;
                let definition =
                    backup.definition(&disks, &winio::hostname(), name, schedule.schedule()?, schedule.run_missed)?;
                let password = if definition.encrypt { Some(passwords.required(true)?) } else { None };
                let id: Uuid = request(
                    client,
                    methods::DEFINITIONS_SAVE,
                    SaveDefinitionParams { definition, password },
                    passwords,
                    timeout,
                )?;
                output::value(&id, cli.json)?;
                None
            }
            DefinitionCommand::Delete { id } => {
                client.call::<_, ()>(methods::DEFINITIONS_DELETE, IdParams { id: *id })?;
                output::value(&serde_json::json!({"deleted":id}), cli.json)?;
                None
            }
        },
        Command::Jobs { command } => {
            match command {
                None => {
                    let jobs: Vec<Job> = client.call(methods::JOBS_LIST, EmptyParams {})?;
                    output::value(&jobs, cli.json)?;
                }
                Some(JobsCommand::Cancel { id }) => {
                    client.call::<_, ()>(methods::JOBS_CANCEL, IdParams { id: *id })?;
                    output::value(&serde_json::json!({"cancelRequested":id}), cli.json)?;
                }
            }
            None
        }
        Command::History { limit } => {
            let history: Vec<HistoryEntry> = client.call(methods::HISTORY_LIST, HistoryParams { limit: *limit })?;
            output::value(&history, cli.json)?;
            None
        }
        Command::Logs { job_id } => {
            let logs: String = client.call(methods::LOGS_READ, LogsParams { job_id: *job_id })?;
            output::value(&logs, cli.json)?;
            None
        }
    };
    if let Some(id) = job {
        if cli.json {
            output::value(&serde_json::json!({"jobId":id}), true)?;
        } else {
            output::value(&id, false)?;
        }
        if !cli.detach {
            return progress::follow(client, id, cli.json, cancel);
        }
    }
    Ok(0)
}

fn path(path: &std::path::Path) -> Result<String> {
    Ok(std::path::absolute(path)?.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ipc::server::{ClientInfo, Handler};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    struct PasswordChallenge {
        calls: AtomicUsize,
        succeed: bool,
    }
    impl Handler for PasswordChallenge {
        fn call(
            &self,
            method: &str,
            params: serde_json::Value,
            _: &ClientInfo,
        ) -> std::result::Result<serde_json::Value, RpcError> {
            assert_eq!(method, methods::IMAGES_VERIFY);
            let attempt = self.calls.fetch_add(1, Ordering::SeqCst);
            if attempt > 0 {
                assert_eq!(params["password"], "test-only");
            }
            if self.succeed && attempt == 2 {
                Ok(serde_json::json!(Uuid::nil()))
            } else {
                Err(RpcError::new(PASSWORD_REQUIRED, "password required"))
            }
        }
    }

    #[test]
    fn password_rpc_retries_are_bounded_and_preserve_parameters() {
        for succeed in [true, false] {
            let handler = Arc::new(PasswordChallenge { calls: AtomicUsize::new(0), succeed });
            let (server, client) = crate::test_support::loopback(handler.clone());
            let mut passwords = Passwords::from_value(Some("test-only".into())).unwrap();
            let result: Result<Uuid> = request(
                &client,
                methods::IMAGES_VERIFY,
                ImagePasswordParams { path: "test.irimg".into(), password: None },
                &mut passwords,
                Duration::from_secs(5),
            );
            server.shutdown();
            assert_eq!(handler.calls.load(Ordering::SeqCst), 3);
            assert_eq!(result.is_ok(), succeed);
        }
    }

    #[test]
    fn service_paths_are_absolute() {
        let converted = path(std::path::Path::new("images/test.irimg")).unwrap();
        assert!(std::path::Path::new(&converted).is_absolute());
        assert!(converted.ends_with(r"images\test.irimg"));
    }

    struct DefinitionRun {
        id: Uuid,
        calls: AtomicUsize,
        succeed: bool,
    }

    impl Handler for DefinitionRun {
        fn call(
            &self,
            method: &str,
            params: serde_json::Value,
            _: &ClientInfo,
        ) -> std::result::Result<serde_json::Value, RpcError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            assert_eq!(method, methods::DEFINITIONS_RUN);
            assert_eq!(params, serde_json::json!({"id":self.id}));
            if self.succeed {
                Ok(serde_json::json!(Uuid::nil()))
            } else {
                Err(RpcError::new(PASSWORD_REQUIRED, "stored password unavailable"))
            }
        }
    }

    #[test]
    fn definition_run_uses_only_id_and_never_challenges_for_a_password() {
        use clap::Parser;
        for succeed in [true, false] {
            let id = Uuid::new_v4();
            let handler = Arc::new(DefinitionRun { id, calls: AtomicUsize::new(0), succeed });
            let (server, client) = crate::test_support::loopback(handler.clone());
            let cli = Cli::try_parse_from(["inocrium", "--json", "--detach", "def", "run", &id.to_string()]).unwrap();
            let mut passwords = Passwords::from_value(None).unwrap();
            let result = run(&cli, &client, &mut passwords, &engine::Cancel::new());
            server.shutdown();
            assert_eq!(result.is_ok(), succeed);
            assert_eq!(handler.calls.load(Ordering::SeqCst), 1);
            assert!(passwords.current().is_none());
        }
    }
}
