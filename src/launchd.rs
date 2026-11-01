//! Per-user LaunchAgent: keeps the agent running across logins until `stop`.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

pub const LABEL: &str = "com.rustickeys.agent";

fn escape(p: &Path) -> String {
    p.to_string_lossy().replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

pub fn plist_xml(exe: &Path, log: &Path, home_override: Option<&Path>) -> String {
    let env = home_override
        .map(|h| {
            format!(
                "\t<key>EnvironmentVariables</key>\n\t<dict>\n\t\t<key>RUSTIC_KEYS_HOME</key>\n\t\t<string>{}</string>\n\t</dict>\n",
                escape(h)
            )
        })
        .unwrap_or_default();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{LABEL}</string>
	<key>ProgramArguments</key>
	<array>
		<string>{exe}</string>
		<string>daemon</string>
	</array>
	<key>RunAtLoad</key>
	<true/>
	<key>KeepAlive</key>
	<dict>
		<key>SuccessfulExit</key>
		<false/>
	</dict>
	<key>ProcessType</key>
	<string>Interactive</string>
	<key>StandardOutPath</key>
	<string>{log}</string>
	<key>StandardErrorPath</key>
	<string>{log}</string>
{env}</dict>
</plist>
"#,
        exe = escape(exe),
        log = escape(log),
    )
}

/// `~/Library/LaunchAgents/com.rustickeys.agent.plist`
pub fn plist_path() -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"));
    home.join("Library/LaunchAgents").join(format!("{LABEL}.plist"))
}

fn domain() -> String {
    format!("gui/{}", unsafe { libc::getuid() })
}

fn launchctl(args: &[&str]) -> Result<std::process::Output> {
    Command::new("launchctl").args(args).output().context("running launchctl")
}

/// Is the agent loaded in this login session?
pub fn is_loaded() -> bool {
    launchctl(&["print", &format!("{}/{LABEL}", domain())]).is_ok_and(|o| o.status.success())
}

/// Write the plist and load it; launchd starts the agent now and at every login.
pub fn install(exe: &Path) -> Result<()> {
    let path = plist_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let home = std::env::var_os("RUSTIC_KEYS_HOME").map(PathBuf::from);
    std::fs::create_dir_all(crate::config::home_dir())?;
    std::fs::write(&path, plist_xml(exe, &crate::config::log_path(), home.as_deref()))
        .with_context(|| format!("writing {}", path.display()))?;
    let out = launchctl(&["bootstrap", &domain(), &path.to_string_lossy()])?;
    if !out.status.success() {
        bail!("launchctl bootstrap failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

/// Unload the agent and delete the plist. Returns `false` if nothing was installed.
pub fn uninstall() -> Result<bool> {
    let path = plist_path();
    let loaded = is_loaded();
    if !loaded && !path.exists() {
        return Ok(false);
    }
    if loaded {
        let out = launchctl(&["bootout", &format!("{}/{LABEL}", domain())])?;
        if !out.status.success() && is_loaded() {
            bail!("launchctl bootout failed: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
    }
    if path.exists() {
        std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plist_contains_program_and_keepalive() {
        let xml = plist_xml(Path::new("/Users/me/.cargo/bin/rustic-keys"), Path::new("/tmp/agent.log"), None);
        assert!(xml.contains("<string>com.rustickeys.agent</string>"));
        assert!(xml.contains("<string>/Users/me/.cargo/bin/rustic-keys</string>"));
        assert!(xml.contains("<string>daemon</string>"));
        assert!(xml.contains("<key>RunAtLoad</key>\n\t<true/>"));
        // Restart after crashes/kills, but not after a deliberate clean exit.
        assert!(xml.contains("<key>KeepAlive</key>\n\t<dict>\n\t\t<key>SuccessfulExit</key>\n\t\t<false/>\n\t</dict>"));
        assert!(xml.contains("<key>ProcessType</key>\n\t<string>Interactive</string>"));
        assert!(xml.contains("<key>StandardOutPath</key>\n\t<string>/tmp/agent.log</string>"));
        assert!(xml.contains("<key>StandardErrorPath</key>\n\t<string>/tmp/agent.log</string>"));
    }

    #[test]
    fn plist_includes_home_override_only_when_given() {
        let exe = Path::new("/bin/rk");
        let log = Path::new("/tmp/l");
        assert!(!plist_xml(exe, log, None).contains("RUSTIC_KEYS_HOME"));
        let xml = plist_xml(exe, log, Some(Path::new("/tmp/rkhome")));
        assert!(xml.contains("<key>RUSTIC_KEYS_HOME</key>\n\t\t<string>/tmp/rkhome</string>"));
    }

    #[test]
    fn plist_escapes_xml_in_paths() {
        let xml = plist_xml(Path::new("/Users/a&b/<rk>"), Path::new("/tmp/l"), None);
        assert!(xml.contains("<string>/Users/a&amp;b/&lt;rk&gt;</string>"));
    }
}
