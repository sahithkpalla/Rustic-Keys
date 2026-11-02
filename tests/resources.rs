//! Isolated bounded resource stress; no HID/TCC, launchd or real input.
use rustic_keys::{
    config::Config,
    ipc::{self, Request},
};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn repeated_socket_config_and_timeout_operations_release_fds() {
    const CHILD: &str = "RK_FD_RESOURCE_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let home = std::path::PathBuf::from(std::env::var_os("RUSTIC_KEYS_HOME").unwrap());
        let path = home.join("a.sock");
        let cycle = || {
            let listener = ipc::bind(&path).unwrap();
            // No server handles this connection: exercise timed-out client cleanup.
            assert!(ipc::send(&path, &Request::Status, Duration::from_millis(1)).is_err());
            drop(listener);
            Config::default().save().unwrap();
            assert_eq!(Config::load().unwrap(), Config::default());
        };
        cycle(); // Warm lazy runtime/filesystem state before comparing counters.
        let count = || std::fs::read_dir("/dev/fd").unwrap().count();
        let before = count();
        for _ in 0..250 {
            cycle();
        }
        let after = count();
        println!("250 socket/config/timeout cycles: fds {before} -> {after}");
        assert_eq!(after, before, "no per-cycle file/socket/lock descriptor retention");
        return;
    }
    let home = tempfile::tempdir().unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "repeated_socket_config_and_timeout_operations_release_fds",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD, "1")
        .env("RUSTIC_KEYS_HOME", home.path())
        .env_remove("RUST_BACKTRACE")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let end = Instant::now() + Duration::from_secs(20);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= end {
            let _ = child.kill();
            let _ = child.wait();
            panic!("resource child exceeded deadline");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    println!("{}", String::from_utf8_lossy(&output.stdout));
}
