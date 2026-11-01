//! Per-user LaunchAgent: keeps the agent running across logins until `stop`.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

pub const LABEL: &str = "com.rustickeys.agent";

fn escape(p: &Path) -> String {
    p.to_string_lossy()
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
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

/// The executable path an installed plist launches (first ProgramArguments entry).
pub fn installed_program(plist: &str) -> Option<PathBuf> {
    let after = &plist[plist.find("<key>ProgramArguments</key>")?..];
    let start = after.find("<string>")? + "<string>".len();
    let end = start + after[start..].find("</string>")?;
    let raw = after[start..end]
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&");
    Some(PathBuf::from(raw))
}

/// Should `start` replace the running agent? Yes if it runs a different
/// binary, or the same path was rebuilt after the agent was installed.
pub fn needs_reinstall(installed: Option<&Path>, exe: &Path, exe_rebuilt_since_install: bool) -> bool {
    installed != Some(exe) || exe_rebuilt_since_install
}

/// Compare the installed plist with the running binary (see `needs_reinstall`).
pub fn installed_is_stale(exe: &Path) -> Result<bool> {
    let path = plist_path()?;
    let bytes = crate::state_files::PrivateDir::open(path.parent().unwrap())?.read(&format!("{LABEL}.plist"), 65536)?;
    let installed = bytes
        .and_then(|x| String::from_utf8(x).ok())
        .and_then(|x| installed_program(&x));
    let modified = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    let rebuilt = matches!((modified(exe), modified(&path)), (Some(e), Some(p)) if e > p);
    Ok(needs_reinstall(installed.as_deref(), exe, rebuilt))
}

/// `~/Library/LaunchAgents/com.rustickeys.agent.plist`
pub fn plist_path() -> Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .context("HOME is absent; cannot manage LaunchAgent")?;
    anyhow::ensure!(Path::new(&home).is_absolute(), "HOME must be absolute");
    Ok(PathBuf::from(home)
        .join("Library/LaunchAgents")
        .join(format!("{LABEL}.plist")))
}

fn domain() -> String {
    format!("gui/{}", unsafe { libc::getuid() })
}

fn launchctl(args: &[&str]) -> Result<std::process::Output> {
    crate::process::output(Command::new("launchctl").args(args), std::time::Duration::from_secs(2))
        .context("running launchctl")
}

/// Is the agent loaded in this login session?
pub fn is_loaded() -> Result<bool> {
    let out = launchctl(&["print", &format!("{}/{LABEL}", domain())])?;
    if out.status.success() {
        return Ok(true);
    }
    // launchctl's specific service-not-found result, NOT every nonzero query.
    // Unknown/localized errors are deliberately conservative and retain the plist.
    let err = String::from_utf8_lossy(&out.stderr);
    if out.status.code() == Some(113) && err.contains("Could not find service") {
        return Ok(false);
    }
    bail!("launchctl query failed (state unknown): {}", err.trim())
}

/// Write the plist and load it; launchd starts the agent now and at every login.
pub fn install(exe: &Path) -> Result<()> {
    let path = plist_path()?;
    let home = crate::config::home_dir()?;
    for p in [exe, home.as_path(), path.as_path()] {
        let text = p.to_str().context("LaunchAgent paths must be UTF-8")?;
        anyhow::ensure!(
            !text
                .chars()
                .any(|c| (c.is_control() && !matches!(c, '\t' | '\n' | '\r')) || matches!(c, '\u{fffe}' | '\u{ffff}')),
            "path contains invalid XML characters"
        );
    }
    let dir = crate::state_files::PrivateDir::open(path.parent().unwrap())?;
    let state = crate::state_files::PrivateDir::open(&home)?;
    // launchd must never open an attacker-preplaced log link. The daemon owns
    // its capped, no-follow log; launchd's inherited descriptors go to /dev/null.
    let _ = state.open_file("agent.log", true, true)?;
    dir.atomic_write(
        &format!("{LABEL}.plist"),
        plist_xml(exe, Path::new("/dev/null"), Some(&home)).as_bytes(),
    )
    .with_context(|| format!("writing {}", path.display()))?;
    let out = launchctl(&["bootstrap", &domain(), &path.to_string_lossy()])?;
    if !out.status.success() {
        bail!(
            "launchctl bootstrap failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Unload and positively verify absence. The caller removes the plist only
/// after socket shutdown verification, retaining evidence on any failure.
pub fn uninstall() -> Result<bool> {
    let path = plist_path()?;
    let dir = crate::state_files::PrivateDir::open(path.parent().unwrap())?;
    let existed = dir.read(&format!("{LABEL}.plist"), 65536)?.is_some();
    let loaded = is_loaded()?;
    if loaded {
        let out = launchctl(&["bootout", &format!("{}/{LABEL}", domain())])?;
        if !out.status.success() {
            bail!(
                "launchctl bootout failed; plist retained: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
    }
    // Verify absence positively before deleting recovery evidence, even after a
    // successful bootout. A silent socket is never proof of an unloaded job.
    anyhow::ensure!(!is_loaded()?, "agent is still loaded; plist retained");
    Ok(loaded || existed)
}

/// Called only after both launchd absence and socket shutdown are verified.
pub fn remove_plist() -> Result<()> {
    let path = plist_path()?;
    crate::state_files::PrivateDir::open(path.parent().unwrap())?.remove(&format!("{LABEL}.plist"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plist_contains_program_and_keepalive() {
        let xml = plist_xml(
            Path::new("/Users/me/.cargo/bin/rustic-keys"),
            Path::new("/tmp/agent.log"),
            None,
        );
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
    fn installed_program_roundtrips_plist() {
        let exe = Path::new("/Users/a&b/.cargo/bin/rustic-keys");
        let xml = plist_xml(exe, Path::new("/tmp/l"), None);
        assert_eq!(installed_program(&xml).as_deref(), Some(exe));
        assert_eq!(installed_program("<plist></plist>"), None);
    }

    #[test]
    fn reinstall_when_binary_differs_or_rebuilt() {
        let installed = Path::new("/proj/target/debug/rustic-keys");
        let cargo_bin = Path::new("/Users/me/.cargo/bin/rustic-keys");
        assert!(needs_reinstall(Some(installed), cargo_bin, false));
        assert!(needs_reinstall(Some(cargo_bin), cargo_bin, true));
        assert!(needs_reinstall(None, cargo_bin, false), "unreadable plist");
        assert!(!needs_reinstall(Some(cargo_bin), cargo_bin, false));
    }

    #[test]
    fn plist_escapes_xml_in_paths() {
        let xml = plist_xml(Path::new("/Users/a&b/<rk>"), Path::new("/tmp/l"), None);
        assert!(xml.contains("<string>/Users/a&amp;b/&lt;rk&gt;</string>"));
    }
}
