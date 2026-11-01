//! User-facing subcommands.

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::config::{self, Config};
use crate::input::permission::Access;
use crate::ipc::{self, Request, Response, StatusReport};
use crate::{launchd, styles};

const PERMISSION_HELP: &str = "Rustic Keys needs Input Monitoring permission.
Open System Settings → Privacy & Security → Input Monitoring and turn on \"rustic-keys\".
If it's already on (common after reinstalling), remove it with − and add it again with +
(press Cmd-Shift-G and enter ~/.cargo/bin/rustic-keys).
It will start clicking automatically once allowed (no need to run start again).";

fn validate_style(name: &str) -> Result<()> {
    if styles::find(name).is_none() {
        let names: Vec<_> = styles::all().iter().map(|s| s.name).collect();
        bail!("unknown style '{name}'; available: {}", names.join(", "));
    }
    Ok(())
}

/// Ask a running agent to re-read the config. Not running is fine.
fn reload_if_running() {
    let _ = ipc::send(&config::socket_path(), &Request::Reload, Duration::from_secs(2));
}

fn query_status() -> Option<StatusReport> {
    match ipc::send(&config::socket_path(), &Request::Status, Duration::from_secs(2)) {
        Ok(Response::Status(s)) => Some(s),
        _ => None,
    }
}

fn print_status(s: &StatusReport) {
    let mode = match s.mode {
        crate::filter::Mode::PerKey => "per-key (built-in keyboard only)",
        crate::filter::Mode::Presence => "presence (silent while an external keyboard is connected)",
    };
    println!("running");
    println!("style: {}", s.style);
    println!("volume: {}", s.volume);
    println!("mode: {mode}");
    println!(
        "permission: {}",
        match s.permission {
            Access::Granted => "granted",
            Access::Denied => "denied",
            Access::Unknown => "not yet granted",
        }
    );
    println!(
        "external keyboard: {}",
        if s.external_keyboard_connected {
            "connected"
        } else {
            "not connected"
        }
    );
    println!("audio device: {}", s.audio_device.as_deref().unwrap_or("none"));
    if s.permission != Access::Granted {
        println!();
        println!("{PERMISSION_HELP}");
    }
}

pub fn start(style: Option<&str>) -> Result<()> {
    if let Some(name) = style {
        validate_style(name)?;
        let mut c = Config::load()?;
        c.style = name.to_string();
        c.save()?;
    }
    let exe = std::env::current_exe()
        .and_then(|p| p.canonicalize())
        .context("finding the rustic-keys executable")?;
    if launchd::is_loaded() && !launchd::installed_is_stale(&exe) {
        reload_if_running();
        println!("already running");
    } else {
        if launchd::is_loaded() {
            println!("switching the agent to {}", exe.display());
            launchd::uninstall()?;
            wait_until_stopped()?;
        }
        launchd::install(&exe)?;
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Some(s) = query_status() {
            print_status(&s);
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    bail!("the agent didn't respond; see {}", config::log_path().display())
}

pub fn stop() -> Result<()> {
    if !launchd::uninstall()? {
        println!("not running");
        return Ok(());
    }
    wait_until_stopped()?;
    println!("stopped");
    Ok(())
}

fn wait_until_stopped() -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(3);
    while query_status().is_some() {
        if Instant::now() > deadline {
            bail!(
                "the agent is still answering after stop; see {}",
                config::log_path().display()
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

pub fn style(name: &str) -> Result<()> {
    validate_style(name)?;
    let mut c = Config::load()?;
    c.style = name.to_string();
    c.save()?;
    reload_if_running();
    println!("style: {name}");
    Ok(())
}

pub fn styles() -> Result<()> {
    let current = Config::load()?.style;
    for s in styles::all() {
        let mark = if s.name == current { '*' } else { ' ' };
        println!("{mark} {} — {}", s.name, s.description);
    }
    Ok(())
}

pub fn volume(level: u8) -> Result<()> {
    let mut c = Config::load()?;
    c.volume = level;
    c.save()?;
    reload_if_running();
    println!("volume: {level}");
    Ok(())
}

pub fn status() -> Result<()> {
    match query_status() {
        Some(s) => print_status(&s),
        None => println!("not running"),
    }
    Ok(())
}
