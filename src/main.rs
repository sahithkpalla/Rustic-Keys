use clap::Parser;
use rustic_keys::cli::{Cli, Command};
use rustic_keys::input::{hid, permission, KeyAction, KeySource};

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Probe => probe(),
    }
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
