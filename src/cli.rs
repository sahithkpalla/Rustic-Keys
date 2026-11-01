//! Command-line definitions.

use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "rustic-keys",
    version,
    about = "Mechanical keyboard sounds for your MacBook's built-in keyboard"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Start clicking now and at every login, until `stop`.
    Start {
        /// Switch style to use (see `styles`).
        #[arg(long)]
        style: Option<String>,
    },
    /// Stop clicking and stop starting at login.
    Stop,
    /// Change the switch style.
    Style { name: String },
    /// List the available switch styles.
    Styles,
    /// Set the volume (0-100).
    Volume {
        #[arg(value_parser = clap::value_parser!(u8).range(0..=100))]
        level: u8,
    },
    /// Show whether Rustic Keys is running and how it's set up.
    Status,
    /// Print every keyboard event with the device it came from (diagnostics).
    #[command(hide = true)]
    Probe,
    /// Play a style's sounds once (diagnostics).
    #[command(hide = true)]
    Demo { style: String },
    /// Exit 0 if this binary has Input Monitoring permission (used by the agent).
    #[command(hide = true)]
    CheckAccess,
    /// Run the background agent (launched by launchd).
    #[command(hide = true)]
    Daemon {
        /// Consecutive self-restarts so far (set by the agent itself).
        #[arg(long, default_value_t = 0, hide = true)]
        reexecs: u32,
    },
}
