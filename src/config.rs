//! Where Rustic Keys keeps its files, and the persisted settings.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::filter::{DEFAULT_MODE, Mode};

/// `$RUSTIC_KEYS_HOME`, else `~/Library/Application Support/rustic-keys`.
pub fn home_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("RUSTIC_KEYS_HOME").filter(|d| !d.is_empty()) {
        anyhow::ensure!(PathBuf::from(&dir).is_absolute(), "RUSTIC_KEYS_HOME must be absolute");
        return Ok(PathBuf::from(dir));
    }
    let home = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .context("HOME is absent; set an absolute private RUSTIC_KEYS_HOME")?;
    anyhow::ensure!(PathBuf::from(&home).is_absolute(), "HOME must be absolute");
    Ok(PathBuf::from(home).join("Library/Application Support/rustic-keys"))
}

pub fn config_path() -> Result<PathBuf> {
    Ok(home_dir()?.join("config.toml"))
}
pub fn socket_path() -> Result<PathBuf> {
    Ok(home_dir()?.join("agent.sock"))
}
pub fn log_path() -> Result<PathBuf> {
    Ok(home_dir()?.join("agent.log"))
}

pub const MAX_CONFIG_BYTES: usize = 4096;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub style: String,
    pub volume: u8,
    pub mode: Mode,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            style: "clicky".into(),
            volume: 70,
            mode: DEFAULT_MODE,
        }
    }
}

impl Config {
    /// Load settings; a missing file means defaults.
    pub fn load() -> Result<Config> {
        let dir = crate::state_files::PrivateDir::open(&home_dir()?)?;
        let Some(bytes) = dir.read("config.toml", MAX_CONFIG_BYTES)? else {
            return Ok(Self::default());
        };
        let c: Self = toml::from_str(std::str::from_utf8(&bytes)?).context("reading config.toml")?;
        c.validate()?;
        Ok(c)
    }

    /// Save atomically (temp file + rename) so the agent never reads half a file.
    pub fn save(&self) -> Result<()> {
        self.validate()?;
        crate::state_files::PrivateDir::open(&home_dir()?)?
            .atomic_write("config.toml", toml::to_string(self)?.as_bytes())
    }

    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(self.volume <= 100, "volume must be 0–100");
        anyhow::ensure!(
            crate::styles::find(&self.style).is_some(),
            "unknown style in config: {:?}",
            self.style
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV: Mutex<()> = Mutex::new(());

    fn with_home<T>(f: impl FnOnce(&std::path::Path) -> T) -> T {
        let _guard = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        // SAFETY: tests touching this variable are serialized by ENV.
        unsafe { std::env::set_var("RUSTIC_KEYS_HOME", dir.path()) };
        let out = f(dir.path());
        unsafe { std::env::remove_var("RUSTIC_KEYS_HOME") };
        out
    }

    #[test]
    fn missing_file_gives_default() {
        with_home(|_| assert_eq!(Config::load().unwrap(), Config::default()));
    }

    #[test]
    fn save_then_load_roundtrips() {
        with_home(|_| {
            let c = Config {
                style: "thocky".into(),
                volume: 40,
                mode: Mode::Presence,
            };
            c.save().unwrap();
            assert_eq!(Config::load().unwrap(), c);
        });
    }

    #[test]
    fn partial_file_fills_defaults() {
        with_home(|home| {
            std::fs::write(home.join("config.toml"), "volume = 10\n").unwrap();
            let c = Config::load().unwrap();
            assert_eq!(c.volume, 10);
            assert_eq!(c.style, "clicky");
        });
    }
}
