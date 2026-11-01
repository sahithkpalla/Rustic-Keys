//! Command-line definitions.

use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "rustic-keys", version, about = "Mechanical keyboard sounds for your MacBook's built-in keyboard")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Print every keyboard event with the device it came from (diagnostics).
    #[command(hide = true)]
    Probe,
}
