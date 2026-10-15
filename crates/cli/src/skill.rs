use crate::args::SkillTopic;
use anyhow::{Context, Result};
use std::path::PathBuf;

const GUIDE: &str = include_str!("../../../skill/restore-from-backup/SKILL.md");
const APPS: &str = include_str!("../../../skill/restore-from-backup/apps.md");

/// The skill folder the installer puts next to the CLI, if there is one.
fn installed_dir() -> Option<PathBuf> {
    let dir = std::env::current_exe().ok()?.parent()?.join("skill").join("restore-from-backup");
    dir.join("scripts").join("Invoke-InocriumElevated.ps1").is_file().then_some(dir)
}

/// Prints the restore guide with `<skill-dir>` filled in, so an assistant can follow it from the
/// console output alone. Needs neither the service nor elevation.
pub fn run(topic: SkillTopic, path: bool) -> Result<i32> {
    let dir = installed_dir();
    if path {
        let dir = dir.context("The skill is not installed next to this inocrium.exe; reinstall inocrium reflect")?;
        println!("{}", dir.display());
        return Ok(0);
    }
    let text = match topic {
        SkillTopic::Guide => GUIDE,
        SkillTopic::Apps => APPS,
    };
    match &dir {
        Some(dir) => print!("{}", text.replace("<skill-dir>", &dir.display().to_string())),
        None => {
            eprintln!(
                "Note: the skill's scripts are not installed next to this inocrium.exe; <skill-dir> is left as is."
            );
            print!("{text}");
        }
    }
    if topic == SkillTopic::Guide {
        println!("\n---\nRun `inocrium skill apps` for the per-app reference (apps.md) that this guide refers to.");
    }
    Ok(0)
}
