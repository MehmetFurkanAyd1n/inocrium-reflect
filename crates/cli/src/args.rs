use anyhow::{bail, ensure, Result};
use clap::{Args, Parser, Subcommand};
use ipc::proto::{BackupMode, Definition, Disk, DiskSelection, Retention, RetentionKind, Schedule, ScheduleKind};
use std::path::PathBuf;
use uuid::Uuid;

#[derive(Debug, Parser)]
#[command(name = "inocrium", version, about = "inocrium reflect disk imaging")]
pub struct Cli {
    #[arg(long, global = true)]
    pub direct: bool,
    #[arg(long, global = true)]
    pub json: bool,
    #[arg(long, global = true)]
    pub detach: bool,
    /// Read one password line from standard input.
    #[arg(long, global = true)]
    pub password_stdin: bool,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    Disks,
    Backup(BackupArgs),
    Verify {
        img: PathBuf,
    },
    Mount {
        img: PathBuf,
        #[arg(long)]
        writable: bool,
        /// Keep the image's NTFS permissions; by default everyone gets full access.
        #[arg(long)]
        keep_permissions: bool,
        #[arg(long)]
        disk: Option<u32>,
        #[arg(long, value_parser = positive_u32)]
        partition: Option<u32>,
    },
    Unmount {
        id: String,
    },
    Restore(RestoreArgs),
    ExportVhdx {
        img: PathBuf,
        out: PathBuf,
        #[arg(long, default_value_t = 0)]
        disk: u32,
    },
    Redeploy {
        #[arg(long)]
        disk: u32,
        #[arg(long, num_args = 1..)]
        drivers: Vec<PathBuf>,
    },
    Viboot {
        #[command(subcommand)]
        command: VibootCommand,
    },
    Def {
        #[command(subcommand)]
        command: DefinitionCommand,
    },
    Jobs {
        #[command(subcommand)]
        command: Option<JobsCommand>,
    },
    History {
        #[arg(long, default_value_t = 50, value_parser = positive_u32)]
        limit: u32,
    },
    Logs {
        job_id: Uuid,
    },
}

#[derive(Debug, Args)]
pub struct BackupArgs {
    #[arg(long, required = true, action = clap::ArgAction::Append)]
    pub disk: Vec<u32>,
    #[arg(long, value_parser = validate_partitions)]
    pub partitions: Option<String>,
    #[arg(long)]
    pub dest: PathBuf,
    #[arg(long)]
    pub prefix: Option<String>,
    #[arg(long, default_value_t = 3, value_parser = clap::value_parser!(u8).range(0..=19))]
    pub compress: u8,
    #[arg(long)]
    pub encrypt: bool,
    #[arg(long)]
    pub verify: bool,
    #[arg(long)]
    pub forensic: bool,
    #[arg(long, value_parser = parse_size)]
    pub split: Option<u64>,
    #[arg(long, default_value = "")]
    pub comment: String,
    #[arg(long, conflicts_with = "keep_days", value_parser = positive_u32)]
    pub keep_last: Option<u32>,
    #[arg(long, value_parser = positive_u32)]
    pub keep_days: Option<u32>,
}

#[derive(Debug, Args)]
pub struct RestoreArgs {
    pub img: PathBuf,
    #[arg(long)]
    pub target_disk: u32,
    #[arg(long, default_value_t = 0)]
    pub disk: u32,
    #[arg(long, value_parser = positive_u32)]
    pub partition: Option<u32>,
    #[arg(long, requires = "partition", conflicts_with = "target_offset", value_parser = positive_u32)]
    pub target_partition: Option<u32>,
    #[arg(long, requires = "partition")]
    pub target_offset: Option<u64>,
    #[arg(long)]
    pub extend: bool,
    #[arg(long)]
    pub yes: bool,
}

#[derive(Debug, Subcommand)]
pub enum VibootCommand {
    Start {
        img: PathBuf,
        #[arg(long, default_value_t = 0)]
        disk: u32,
        #[arg(long)]
        name: Option<String>,
        #[arg(long, default_value_t = 4096, value_parser = positive_u32)]
        memory: u32,
        #[arg(long, default_value_t = 2, value_parser = positive_u32)]
        cpus: u32,
        /// Boot the image's default UEFI boot loader even if it is not Windows Boot Manager
        /// (such as GRUB on a dual-boot disk).
        #[arg(long)]
        keep_boot_loader: bool,
    },
    List,
    Stop {
        name: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum DefinitionCommand {
    List,
    Run {
        id: Uuid,
    },
    Create {
        #[arg(long)]
        name: String,
        #[command(flatten)]
        backup: Box<BackupArgs>,
        #[command(flatten)]
        schedule: ScheduleArgs,
    },
    Delete {
        id: Uuid,
    },
}

#[derive(Debug, Subcommand)]
pub enum JobsCommand {
    Cancel { id: Uuid },
}

#[derive(Debug, Default, Args)]
pub struct ScheduleArgs {
    #[arg(long, conflicts_with_all = ["weekly", "monthly"])]
    pub daily: Option<String>,
    #[arg(long, conflicts_with = "monthly", requires = "days")]
    pub weekly: Option<String>,
    #[arg(long, requires = "day")]
    pub monthly: Option<String>,
    #[arg(long, requires = "weekly")]
    pub days: Option<String>,
    #[arg(long, requires = "monthly", value_parser = clap::value_parser!(u8).range(1..=31))]
    pub day: Option<u8>,
    #[arg(long)]
    pub run_missed: bool,
}

impl ScheduleArgs {
    pub fn schedule(&self) -> Result<Option<Schedule>> {
        let (kind, time) = match (&self.daily, &self.weekly, &self.monthly) {
            (Some(t), None, None) => (ScheduleKind::Daily, t),
            (None, Some(t), None) => (ScheduleKind::Weekly, t),
            (None, None, Some(t)) => (ScheduleKind::Monthly, t),
            (None, None, None) => return Ok(None),
            _ => bail!("Only one schedule may be selected"),
        };
        ensure!(
            time.len() == 5 && chrono::NaiveTime::parse_from_str(time, "%H:%M").is_ok(),
            "Use HH:MM (00:00..23:59)"
        );
        let weekdays = if kind == ScheduleKind::Weekly {
            let names = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];
            let mut days = Vec::new();
            for name in self.days.as_deref().unwrap_or("").split(',') {
                let day = names
                    .iter()
                    .position(|n| n.eq_ignore_ascii_case(name.trim()))
                    .ok_or_else(|| anyhow::anyhow!("Invalid weekday: {name}"))? as u8;
                if !days.contains(&day) {
                    days.push(day);
                }
            }
            days
        } else {
            Vec::new()
        };
        if kind == ScheduleKind::Monthly {
            ensure!(self.day.is_some_and(|d| (1..=31).contains(&d)), "Monthly schedule requires --day 1..31");
        }
        Ok(Some(Schedule { kind, time: time.clone(), weekdays, day_of_month: self.day }))
    }
}

impl BackupArgs {
    pub fn definition(
        &self,
        disks: &[Disk],
        hostname: &str,
        name: &str,
        schedule: Option<Schedule>,
        run_missed: bool,
    ) -> Result<Definition> {
        ensure!(self.partitions.is_none() || self.disk.len() == 1, "--partitions requires exactly one --disk");
        let partitions = self.partitions.as_deref().map(parse_partitions).transpose().map_err(anyhow::Error::msg)?;
        let prefix = self.prefix.as_deref().unwrap_or(hostname);
        ensure!(
            !prefix.is_empty()
                && !prefix.contains(['/', '\\', ':', '<', '>', '"', '|', '?', '*'])
                && !prefix.chars().any(char::is_control)
                && prefix != "."
                && prefix != "..",
            "Prefix must be a nonempty file name"
        );
        let mut selection = Vec::new();
        for &number in &self.disk {
            ensure!(!selection.iter().any(|d: &DiskSelection| d.number == number), "Disk {number} selected twice");
            let disk =
                disks.iter().find(|d| d.number == number).ok_or_else(|| anyhow::anyhow!("Disk {number} not found"))?;
            if let Some(parts) = &partitions {
                for p in parts {
                    ensure!(disk.partitions.iter().any(|v| v.number == *p), "Disk {number} has no partition {p}");
                }
            }
            selection.push(DiskSelection { number, disk_id: disk.disk_id.clone(), partitions: partitions.clone() });
        }
        let retention = self
            .keep_last
            .map(|value| Retention { kind: RetentionKind::KeepLast, value })
            .or_else(|| self.keep_days.map(|value| Retention { kind: RetentionKind::KeepDays, value }));
        ensure!(!name.trim().is_empty(), "Definition name must not be empty");
        ensure!(!self.dest.as_os_str().is_empty(), "Destination must not be empty");
        let definition = Definition {
            id: Uuid::new_v4(),
            name: name.into(),
            enabled: true,
            disks: selection,
            destination: std::path::absolute(&self.dest)?.to_string_lossy().into_owned(),
            prefix: prefix.into(),
            mode: if self.forensic { BackupMode::Forensic } else { BackupMode::Smart },
            compression_level: self.compress,
            encrypt: self.encrypt,
            password_set: false,
            verify: self.verify,
            split_size: self.split,
            comment: self.comment.clone(),
            retention,
            schedule,
            run_missed,
            last_run: None,
            next_run: None,
        };
        definition.validate()?;
        Ok(definition)
    }
}

pub fn parse_size(value: &str) -> Result<u64, String> {
    let value = value.trim().to_ascii_uppercase();
    let (digits, multiplier) = match value.as_bytes().last() {
        Some(b'K') => (&value[..value.len() - 1], 1u64 << 10),
        Some(b'M') => (&value[..value.len() - 1], 1u64 << 20),
        Some(b'G') => (&value[..value.len() - 1], 1u64 << 30),
        Some(b'T') => (&value[..value.len() - 1], 1u64 << 40),
        _ => (value.as_str(), 1),
    };
    digits
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(multiplier))
        .filter(|n| *n > 0)
        .ok_or_else(|| "Size must be positive bytes or an integer with K/M/G/T suffix (e.g. 4G)".into())
}

pub fn parse_partitions(value: &str) -> Result<Vec<u32>, String> {
    let mut parts = Vec::new();
    for part in value.split(',') {
        let n = positive_u32(part.trim())?;
        if parts.contains(&n) {
            return Err("Partition numbers must be unique".into());
        }
        parts.push(n);
    }
    Ok(parts)
}

fn validate_partitions(value: &str) -> Result<String, String> {
    parse_partitions(value)?;
    Ok(value.into())
}

fn positive_u32(value: &str) -> Result<u32, String> {
    value.parse::<u32>().ok().filter(|n| *n > 0).ok_or_else(|| "Expected a positive integer".into())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    #[test]
    fn sizes() {
        for (s, n) in [("4G", 4 << 30), ("700M", 700 << 20), ("1T", 1 << 40), ("512", 512), ("2k", 2048)] {
            assert_eq!(parse_size(s).unwrap(), n);
        }
        for s in ["", "0", "0G", "-1", "1.5G", "18446744073709551615T", "4GB"] {
            assert!(parse_size(s).is_err());
        }
    }
    #[test]
    fn partitions() {
        assert_eq!(parse_partitions("1, 3").unwrap(), vec![1, 3]);
        for s in ["", "0", "1,", "1,1", "-1", "a"] {
            assert!(parse_partitions(s).is_err());
        }
    }
    #[test]
    fn schedules() {
        let daily = ScheduleArgs { daily: Some("09:30".into()), ..Default::default() }.schedule().unwrap().unwrap();
        assert_eq!(daily.kind, ScheduleKind::Daily);
        let weekly =
            ScheduleArgs { weekly: Some("23:59".into()), days: Some("mon,fri,SUN,sat".into()), ..Default::default() }
                .schedule()
                .unwrap()
                .unwrap();
        assert_eq!(weekly.weekdays, [1, 5, 0, 6]);
        let monthly = ScheduleArgs { monthly: Some("00:00".into()), day: Some(15), ..Default::default() }
            .schedule()
            .unwrap()
            .unwrap();
        assert_eq!(monthly.day_of_month, Some(15));
        assert!(ScheduleArgs { daily: Some("24:00".into()), ..Default::default() }.schedule().is_err());
        assert!(ScheduleArgs { weekly: Some("12:00".into()), days: Some("nope".into()), ..Default::default() }
            .schedule()
            .is_err());
    }
    #[test]
    fn command_surface() {
        let id = "00000000-0000-0000-0000-000000000001";
        for args in [
            vec!["disks"],
            vec!["backup", "--disk", "1", "--disk", "2", "--dest", "D:\\Backups", "--split", "4G"],
            vec!["backup", "--disk", "1", "--partitions", "1,3", "--dest", "D:\\Backups"],
            vec!["verify", "a.mrimgx"],
            vec!["mount", "a.irimg", "--writable"],
            vec!["unmount", "all"],
            vec!["restore", "a.irimg", "--target-disk", "2", "--partition", "1", "--target-offset", "1048576"],
            vec!["export-vhdx", "a.irimg", "a.vhdx"],
            vec!["redeploy", "--disk", "2", "--drivers", "C:\\Drivers", "D:\\Drivers"],
            vec!["viboot", "start", "a.irimg"],
            vec!["viboot", "list"],
            vec!["viboot", "stop", "VM"],
            vec!["def", "list"],
            vec!["def", "run", id],
            vec!["def", "delete", id],
            vec![
                "def", "create", "--name", "daily", "--disk", "0", "--dest", "D:\\", "--weekly", "09:00", "--days",
                "mon,fri",
            ],
            vec!["jobs"],
            vec!["jobs", "cancel", id],
            vec!["history", "--limit", "10"],
            vec!["logs", id],
        ] {
            Cli::try_parse_from(std::iter::once("inocrium").chain(args)).unwrap();
        }
        let cli = Cli::try_parse_from(["inocrium", "disks", "--json", "--direct"]).unwrap();
        assert!(cli.json && cli.direct);
    }
    #[test]
    fn invalid_flag_combinations() {
        for tail in [vec!["--keep-last", "1", "--keep-days", "2"], vec!["--compress", "20"], vec!["--split", "0"]] {
            assert!(Cli::try_parse_from(
                ["inocrium", "backup", "--disk", "0", "--dest", "D:\\"].into_iter().chain(tail)
            )
            .is_err());
        }
        assert!(
            Cli::try_parse_from(["inocrium", "restore", "a", "--target-disk", "1", "--target-partition", "1"]).is_err()
        );
        assert!(Cli::try_parse_from([
            "inocrium", "def", "create", "--name", "x", "--disk", "1", "--dest", "D:\\", "--weekly", "10:00"
        ])
        .is_err());
    }

    #[test]
    fn parsed_schedule_flags() {
        for (flags, kind, weekdays, day) in [
            (vec!["--daily", "09:30"], ScheduleKind::Daily, vec![], None),
            (
                vec!["--weekly", "09:30", "--days", "sun,mon,tue,wed,thu,fri,sat,mon"],
                ScheduleKind::Weekly,
                vec![0, 1, 2, 3, 4, 5, 6],
                None,
            ),
            (vec!["--monthly", "09:30", "--day", "15"], ScheduleKind::Monthly, vec![], Some(15)),
        ] {
            let cli = Cli::try_parse_from(
                [
                    "inocrium",
                    "def",
                    "create",
                    "--name",
                    "scheduled",
                    "--disk",
                    "0",
                    "--dest",
                    "backups",
                    "--run-missed",
                ]
                .into_iter()
                .chain(flags),
            )
            .unwrap();
            let Command::Def { command: DefinitionCommand::Create { schedule, .. } } = cli.command else {
                panic!("expected def create");
            };
            assert!(schedule.run_missed);
            let schedule = schedule.schedule().unwrap().unwrap();
            assert_eq!(schedule.kind, kind);
            assert_eq!(schedule.time, "09:30");
            assert_eq!(schedule.weekdays, weekdays);
            assert_eq!(schedule.day_of_month, day);
        }
    }

    pub(crate) fn definition() -> Definition {
        let Command::Backup(args) =
            Cli::try_parse_from(["inocrium", "backup", "--disk", "1", "--dest", "backups"]).unwrap().command
        else {
            panic!("expected backup")
        };
        args.definition(&[disk()], "test-host", "Test", None, false).unwrap()
    }

    fn disk() -> Disk {
        Disk {
            number: 1,
            model: "test".into(),
            serial: String::new(),
            size: 4096,
            sector_size: 512,
            style: ipc::proto::DiskStyle::Raw,
            disk_id: "test-id".into(),
            is_system: false,
            partitions: vec![],
        }
    }

    #[test]
    fn definitions_validate_disk_selections_and_paths() {
        let definition = definition();
        assert_eq!(definition.prefix, "test-host");
        assert!(std::path::Path::new(&definition.destination).is_absolute());
        assert_eq!(definition.disks[0].disk_id, "test-id");
        for flags in [
            vec!["--disk", "2"],
            vec!["--disk", "1"],
            vec!["--partitions", "1"],
            vec!["--prefix", "bad/name"],
            vec!["--prefix", "bad*name"],
        ] {
            let Command::Backup(args) = Cli::try_parse_from(
                ["inocrium", "backup", "--disk", "1", "--dest", "backups"].into_iter().chain(flags),
            )
            .unwrap()
            .command
            else {
                panic!("expected backup")
            };
            assert!(args.definition(&[disk()], "host", "Test", None, false).is_err());
        }
    }
}
