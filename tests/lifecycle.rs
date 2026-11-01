//! Inert launchctl mocks only. No mock executes the daemon or real launchctl.
use rustic_keys::{
    filter::Mode,
    input::permission::Access,
    ipc::{self, Response, StatusReport},
};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;

struct Lab {
    dir: tempfile::TempDir,
}
impl Lab {
    fn new(mode: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("Library/LaunchAgents")).unwrap();
        std::fs::create_dir(dir.path().join("app")).unwrap();
        let script = dir.path().join("launchctl");
        std::fs::write(&script, r#"#!/bin/sh
# Deliberately cannot call system launchctl or execute any product command.
case "$1" in
 print)
  if [ "$MODE" = query_error ]; then echo 'MOCK unknown query error' >&2; exit 1; fi
  if { [ "$MODE" = verify_error ] || [ "$MODE" = double_error ]; } && [ -e "$HOME/booted" ]; then echo 'MOCK verification error' >&2; exit 1; fi
  if [ -e "$HOME/loaded" ]; then exit 0; fi
  echo 'Could not find service "com.rustickeys.agent" in domain' >&2; exit 113;;
 bootout)
  if [ "$MODE" = boot_error ]; then echo 'MOCK bootout failed' >&2; exit 1; fi
  if [ "$MODE" = double_error ]; then touch "$HOME/booted"; echo 'MOCK bootout failed' >&2; exit 1; fi
  if [ "$MODE" = keep_loaded ]; then exit 0; fi
  touch "$HOME/booted"; rm -f "$HOME/loaded"; exit 0;;
 bootstrap) echo 'MOCK bootstrap failed; no process started' >&2; exit 1;;
 *) exit 99;;
esac
"#).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(dir.path().join("mode"), mode).unwrap();
        Self { dir }
    }
    fn plist(&self) -> std::path::PathBuf {
        self.dir.path().join("Library/LaunchAgents/com.rustickeys.agent.plist")
    }
    fn installed(&self) {
        std::fs::write(self.plist(), b"KEEP PLIST").unwrap();
        std::fs::write(self.dir.path().join("loaded"), b"yes").unwrap();
    }
    fn cmd(&self) -> assert_cmd::Command {
        let mut c = assert_cmd::cargo::cargo_bin_cmd!("rustic-keys");
        let mode = std::fs::read_to_string(self.dir.path().join("mode")).unwrap();
        c.env("HOME", self.dir.path())
            .env("RUSTIC_KEYS_HOME", self.dir.path().join("app"))
            .env("MODE", mode)
            .env("PATH", format!("{}:/usr/bin:/bin", self.dir.path().display()));
        c
    }
}
#[test]
fn bootout_and_query_error_never_claim_stopped() {
    let l = Lab::new("double_error");
    l.installed();
    l.cmd().arg("stop").assert().failure();
    assert!(l.plist().exists());
    assert!(l.dir.path().join("loaded").exists());
}
#[test]
fn successful_bootout_but_verification_error_retains_plist() {
    let l = Lab::new("verify_error");
    l.installed();
    l.cmd().arg("stop").assert().failure();
    assert!(l.plist().exists());
}
#[test]
fn query_error_is_not_absent_for_start_or_stop() {
    let l = Lab::new("query_error");
    l.installed();
    for cmd in ["start", "stop"] {
        l.cmd().arg(cmd).assert().failure();
    }
    assert!(l.plist().exists());
}
#[test]
fn successful_bootout_but_still_loaded_is_not_success() {
    let l = Lab::new("keep_loaded");
    l.installed();
    l.cmd().arg("stop").assert().failure();
    assert!(l.plist().exists());
}
#[test]
fn stale_socket_after_known_unload_is_success() {
    let l = Lab::new("normal");
    l.installed();
    drop(std::os::unix::net::UnixListener::bind(l.dir.path().join("app/agent.sock")).unwrap());
    l.cmd().arg("stop").assert().success();
    assert!(!l.plist().exists());
}
#[test]
fn unloaded_stop_and_successful_unload() {
    let l = Lab::new("normal");
    l.cmd().arg("stop").assert().success();
    l.installed();
    l.cmd().arg("stop").assert().success();
    assert!(!l.plist().exists());
}
#[test]
fn bootstrap_error_and_plist_symlink_refusal() {
    let l = Lab::new("normal");
    let victim = l.dir.path().join("victim");
    std::fs::write(&victim, b"KEEP").unwrap();
    symlink(&victim, l.plist()).unwrap();
    l.cmd().arg("start").assert().failure();
    assert_eq!(std::fs::read(&victim).unwrap(), b"KEEP");
    std::fs::remove_file(l.plist()).unwrap();
    l.cmd().arg("start").assert().failure();
    assert!(l.plist().exists(), "retain bootstrap error evidence");
}
fn stub(path: &Path) {
    ipc::serve(ipc::bind(path).unwrap(), |_| {
        Response::Status(StatusReport {
            style: "clicky".into(),
            volume: 70,
            mode: Mode::PerKey,
            permission: Access::Granted,
            external_keyboard_connected: false,
            audio_device: None,
        })
    });
}
#[test]
fn still_answering_after_bootout_is_error_and_retains_plist() {
    let l = Lab::new("normal");
    l.installed();
    stub(&l.dir.path().join("app/agent.sock"));
    l.cmd().arg("stop").assert().failure();
    assert!(l.plist().exists());
}
#[test]
fn duplicate_start_reloads_without_bootstrap() {
    let l = Lab::new("normal");
    l.installed();
    let exe = std::fs::canonicalize(assert_cmd::cargo::cargo_bin!("rustic-keys")).unwrap();
    std::fs::write(
        l.plist(),
        rustic_keys::launchd::plist_xml(&exe, Path::new("/dev/null"), None),
    )
    .unwrap();
    ipc::serve(ipc::bind(&l.dir.path().join("app/agent.sock")).unwrap(), |r| match r {
        ipc::Request::Reload => Response::Ok,
        _ => Response::Status(StatusReport {
            style: "clicky".into(),
            volume: 70,
            mode: Mode::PerKey,
            permission: Access::Granted,
            external_keyboard_connected: false,
            audio_device: None,
        }),
    });
    l.cmd()
        .arg("start")
        .assert()
        .success()
        .stdout(predicates::str::contains("already running"));
    assert!(!l.dir.path().join("booted").exists());
}
#[test]
fn stale_install_replacement_does_not_hide_bootout_failure() {
    let l = Lab::new("boot_error");
    l.installed();
    l.cmd().arg("start").assert().failure();
    assert!(l.plist().exists());
}
#[test]
fn malformed_socket_is_not_shutdown_evidence() {
    let l = Lab::new("normal");
    l.installed();
    let p = l.dir.path().join("app/agent.sock");
    std::fs::write(p, b"KEEP").unwrap();
    l.cmd().arg("stop").assert().failure();
    assert!(l.plist().exists());
}
