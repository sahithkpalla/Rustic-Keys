use clap::Parser;
use rustic_keys::cli::{Cli, Command};
use rustic_keys::input::{hid, permission, KeyAction, KeySource};

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    use rustic_keys::commands;
    match cli.command {
        Command::Start { style } => commands::start(style.as_deref()),
        Command::Stop => commands::stop(),
        Command::Style { name } => commands::style(&name),
        Command::Styles => commands::styles(),
        Command::Volume { level } => commands::volume(level),
        Command::Status => commands::status(),
        Command::Probe => probe(),
        Command::Demo { style } => demo(&style),
        Command::Daemon => rustic_keys::daemon::run(),
        Command::CheckAccess => std::process::exit(if permission::check() == permission::Access::Granted { 0 } else { 1 }),
    }
}

fn demo(name: &str) -> anyhow::Result<()> {
    use rustic_keys::audio::{output::AudioOutput, sample::load_style};
    use rustic_keys::filter::Sound;
    use std::time::Duration;
    let style = rustic_keys::styles::find(name).ok_or_else(|| anyhow::anyhow!("unknown style '{name}'"))?;
    let mut out = AudioOutput::start()?;
    println!("playing {} on {:?} @ {} Hz", style.name, out.device_name(), out.sample_rate());
    let loaded = load_style(style, out.sample_rate())?;
    for n in 0..loaded.press.len().max(loaded.release.len()) {
        out.play(loaded.pick(Sound::Press, n), 0.5);
        std::thread::sleep(Duration::from_millis(150));
        out.play(loaded.pick(Sound::Release, n), 0.5);
        std::thread::sleep(Duration::from_millis(350));
    }
    for n in 0..20 {
        out.play(loaded.pick(Sound::Press, n), 0.5);
        std::thread::sleep(Duration::from_millis(60));
    }
    std::thread::sleep(Duration::from_millis(400));
    let (req, frames, lat) = out.latency_report();
    println!("buffer requested {req}, {frames} frames/callback ({:.1} ms), callback→speaker {:.1} ms",
        frames as f64 * 1000.0 / out.sample_rate() as f64, lat as f64 / 1000.0);
    Ok(())
}

fn probe() -> anyhow::Result<()> {
    let access = permission::check();
    println!("permission: {access:?}");
    if access != permission::Access::Granted && !permission::request() {
        println!("Grant Input Monitoring to your terminal app in System Settings → Privacy & Security → Input Monitoring, then rerun.");
    }
    let (tx, rx) = std::sync::mpsc::channel();
    hid::spawn_listener(tx)?;
    println!("listening — press Ctrl-C to stop");
    for msg in rx {
        match msg {
            hid::InputMsg::Key(ev) => {
                let src = match ev.source { KeySource::BuiltIn => "built-in", KeySource::External => "external" };
                let act = match ev.action { KeyAction::Down => "down", KeyAction::Up => "up" };
                println!("{src} {act} 0x{:02X}", ev.usage);
            }
            hid::InputMsg::Devices { builtin, external } => println!("devices builtin={builtin} external={external}"),
            hid::InputMsg::DeviceSeen { info, source } => println!("device {source:?}: {info:?}"),
        }
    }
    Ok(())
}
