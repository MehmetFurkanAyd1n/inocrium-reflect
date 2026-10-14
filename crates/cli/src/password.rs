use anyhow::{ensure, Context, Result};
use std::io::{self, IsTerminal, Write};
use windows::Win32::{
    Foundation::HANDLE,
    System::Console::{
        GetConsoleMode, GetStdHandle, SetConsoleMode, CONSOLE_MODE, ENABLE_ECHO_INPUT, STD_INPUT_HANDLE,
    },
};

pub struct Passwords {
    value: Option<String>,
    supplied: bool,
}

impl Passwords {
    pub fn new(stdin: bool) -> Result<Self> {
        let value = if stdin { Some(read_line()?) } else { std::env::var("INOCRIUM_PASSWORD").ok() };
        Self::from_value(value)
    }
    pub(crate) fn from_value(value: Option<String>) -> Result<Self> {
        ensure!(value.as_ref().is_none_or(|p| !p.is_empty()), "Password must not be empty");
        let supplied = value.is_some();
        Ok(Self { value, supplied })
    }
    pub fn current(&self) -> Option<String> {
        self.value.clone()
    }
    pub fn required(&mut self, creating: bool) -> Result<String> {
        if let Some(value) = &self.value {
            return Ok(value.clone());
        }
        let value = prompt("Password: ")?;
        if creating {
            ensure!(value == prompt("Confirm password: ")?, "Passwords do not match");
        }
        self.value = Some(value.clone());
        Ok(value)
    }
    pub fn retry(&mut self) -> Result<String> {
        ensure!(
            !self.supplied || io::stdin().is_terminal(),
            "Supplied password was rejected; provide a valid password"
        );
        self.value = None;
        self.required(false)
    }
}

struct ConsoleMode {
    handle: HANDLE,
    original: CONSOLE_MODE,
}
impl Drop for ConsoleMode {
    fn drop(&mut self) {
        let _ = unsafe { SetConsoleMode(self.handle, self.original) };
    }
}

fn prompt(label: &str) -> Result<String> {
    ensure!(
        io::stdin().is_terminal(),
        "Password required: use --password-stdin or INOCRIUM_PASSWORD when no console is available"
    );
    let handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) }.context("Open console input")?;
    let mut mode = CONSOLE_MODE::default();
    unsafe { GetConsoleMode(handle, &mut mode) }.context("Read console mode")?;
    unsafe { SetConsoleMode(handle, CONSOLE_MODE(mode.0 & !ENABLE_ECHO_INPUT.0)) }.context("Disable password echo")?;
    let guard = ConsoleMode { handle, original: mode };
    eprint!("{label}");
    io::stderr().flush()?;
    let value = read_line();
    drop(guard);
    eprintln!();
    let value = value?;
    ensure!(!value.is_empty(), "Password must not be empty");
    Ok(value)
}

fn read_line() -> Result<String> {
    let mut value = String::new();
    ensure!(io::stdin().read_line(&mut value)? > 0, "No password received on stdin");
    while value.ends_with(['\r', '\n']) {
        value.pop();
    }
    Ok(value)
}

pub fn confirm_restore(target: u32, yes: bool) -> Result<()> {
    if yes {
        return Ok(());
    }
    eprint!("WARNING: restore will overwrite data on target disk {target}. Type {target} to confirm: ");
    io::stderr().flush()?;
    let mut value = String::new();
    io::stdin().read_line(&mut value)?;
    ensure!(value.trim() == target.to_string(), "Restore not confirmed");
    Ok(())
}
