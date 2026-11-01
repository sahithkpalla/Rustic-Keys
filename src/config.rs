//! Where Rustic Keys keeps its files, and the persisted settings.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::filter::{DEFAULT_MODE, Mode};

/// `$RUSTIC_KEYS_HOME`, else `~/Library/Application Support/rustic-keys`.
pub fn home_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("RUSTIC_KEYS_HOME").filter(|d| !d.is_empty()) {
        return PathBuf::from(dir);
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    home.join("Library/Application Support/rustic-keys")
}

pub fn config_path() -> PathBuf {
    home_dir().join("config.toml")
}

pub fn socket_path() -> PathBuf {
    home_dir().join("agent.sock")
}

pub fn log_path() -> PathBuf {
    home_dir().join("agent.log")
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
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
        let path = config_path();
        match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text).with_context(|| format!("reading {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// Save atomically (temp file + rename) so the agent never reads half a file.
    pub fn save(&self) -> Result<()> {
        let dir = home_dir();
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let tmp = dir.join("config.toml.tmp");
        std::fs::write(&tmp, toml::to_string(self)?)?;
        std::fs::rename(&tmp, config_path())?;
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
