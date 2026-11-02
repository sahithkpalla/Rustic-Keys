//! Fully interposed native daemon tests, never a real HID/TCC call. Preflight
//! verifies dyld replacements for EVERY reachable native API before daemon exec.
#![cfg(target_os = "macos")]
use rustic_keys::{
    input::permission::Access,
    ipc::{self, Request, Response},
};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
const SYMBOLS: &[&str] = &[
    "IOHIDCheckAccess",
    "IOHIDRequestAccess",
    "IOHIDManagerCreate",
    "IOHIDManagerSetDeviceMatching",
    "IOHIDManagerRegisterDeviceMatchingCallback",
    "IOHIDManagerRegisterDeviceRemovalCallback",
    "IOHIDManagerRegisterInputValueCallback",
    "IOHIDManagerScheduleWithRunLoop",
    "IOHIDManagerUnscheduleFromRunLoop",
    "IOHIDManagerOpen",
    "IOHIDManagerClose",
    "CFRunLoopRunInMode",
    "CFRunLoopRun",
    "IOHIDValueGetElement",
    "IOHIDElementGetUsagePage",
    "IOHIDElementGetUsage",
    "IOHIDElementGetDevice",
    "IOHIDDeviceGetProperty",
    "IOHIDValueGetIntegerValue",
    "IOHIDValueGetTimeStamp",
];
struct Lab {
    dir: tempfile::TempDir,
    lib: PathBuf,
}
struct MockProcess(Child);
impl Drop for MockProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
impl Lab {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let lib = dir.path().join("mock.dylib");
        let out = Command::new("/usr/bin/clang")
            .args([
                "-dynamiclib",
                "-framework",
                "IOKit",
                "-framework",
                "CoreFoundation",
                "-o",
            ])
            .arg(&lib)
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/hid_mock.c"))
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        std::fs::write(dir.path().join("permission"), "G").unwrap();
        std::fs::write(dir.path().join("native.log"), "").unwrap();
        std::fs::write(dir.path().join("config.toml"), "volume = 0\n").unwrap();
        let lab = Self { dir, lib };
        let out = lab
            .command()
            .env("DYLD_PRINT_INTERPOSING", "1")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .arg("--help")
            .output()
            .unwrap();
        assert!(out.status.success());
        let text = String::from_utf8_lossy(&out.stderr);
        for s in SYMBOLS {
            assert!(
                text.contains(&format!("has interposed '_{s}'")),
                "missing replacement {s}; refusing daemon execution\n{text}"
            );
        }
        lab
    }
    fn command(&self) -> Command {
        let mut c = Command::new(assert_cmd::cargo::cargo_bin!("rustic-keys"));
        c.env("DYLD_INSERT_LIBRARIES", &self.lib)
            .env("RK_MOCK_PERMISSION", self.dir.path().join("permission"))
            .env("RK_MOCK_LOG", self.dir.path().join("native.log"))
            .env("RUSTIC_KEYS_HOME", self.dir.path())
            .env_remove("RUSTIC_KEYS_TRACE")
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(self.dir.path().join("native.stderr")).unwrap());
        c
    }
    fn spawn(&self, extra: Option<&str>) -> MockProcess {
        let mut c = self.command();
        c.arg("daemon");
        if let Some(e) = extra {
            c.env(e, "1");
        }
        MockProcess(c.spawn().unwrap())
    }
    fn wait(&self, access: Access, seconds: u64) {
        let end = Instant::now() + Duration::from_secs(seconds);
        loop {
            if let Ok(Response::Status(s)) = ipc::send(
                &self.dir.path().join("agent.sock"),
                &Request::Status,
                Duration::from_millis(250),
            ) && s.permission == access
            {
                return;
            }
            assert!(
                Instant::now() < end,
                "didn't reach {access:?}; log {}; stderr {}",
                self.native_log(),
                std::fs::read_to_string(self.dir.path().join("native.stderr")).unwrap_or_default()
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    fn native_log(&self) -> String {
        std::fs::read_to_string(self.dir.path().join("native.log")).unwrap()
    }
}
#[test]
fn listener_create_stop_drop_releases_resources_repeatedly() {
    use rustic_keys::input::hid;
    const NAME: &str = "listener_create_stop_drop_releases_resources_repeatedly";
    if std::env::var_os("RK_LIFETIME_CHILD").is_some() {
        // An accidentally inherited child flag must never start real HID.
        // The parent also checks every actual dyld replacement on this binary.
        let marker = unsafe { libc::dlsym(libc::RTLD_DEFAULT, c"rustic_keys_test_mock_marker".as_ptr()) };
        assert!(!marker.is_null(), "missing native mock: refusing all HID calls");
        let marker: unsafe extern "C" fn() -> u64 = unsafe { std::mem::transmute(marker) };
        assert_eq!(unsafe { marker() }, 0x524b544553544f4e);
        for _ in 0..20 {
            let (tx, rx) = std::sync::mpsc::sync_channel(2);
            let listener = hid::spawn_listener(tx).unwrap();
            let end = Instant::now() + Duration::from_secs(2);
            while listener.ready().is_none() && !listener.is_finished() {
                assert!(Instant::now() < end);
                std::thread::sleep(Duration::from_millis(5));
            }
            listener.stop();
            while !listener.is_finished() {
                assert!(Instant::now() < end);
                std::thread::sleep(Duration::from_millis(5));
            }
            drop(listener);
            assert!(
                matches!(rx.try_recv(), Err(std::sync::mpsc::TryRecvError::Disconnected)),
                "callback context must drop its sender"
            );
        }
        return;
    }
    for flag in [None, Some("RK_MOCK_OPEN_FAIL"), Some("RK_MOCK_RETURN_LOOP")] {
        let lab = Lab::new();
        let make = || {
            let mut c = Command::new(std::env::current_exe().unwrap());
            c.env("DYLD_INSERT_LIBRARIES", &lab.lib)
                .env("RK_MOCK_PERMISSION", lab.dir.path().join("permission"))
                .env("RK_MOCK_LOG", lab.dir.path().join("native.log"))
                .env_remove("RUSTIC_KEYS_TRACE");
            c
        };
        // Verify the test executable too, before its child can reach any HID call.
        let preflight = make()
            .env("DYLD_PRINT_INTERPOSING", "1")
            .arg("--list")
            .output()
            .unwrap();
        assert!(preflight.status.success());
        let text = String::from_utf8_lossy(&preflight.stderr);
        for symbol in SYMBOLS {
            assert!(
                text.contains(&format!("has interposed '_{symbol}'")),
                "missing {symbol}: refusing listener test"
            );
        }
        let mut c = make();
        c.args(["--exact", NAME, "--test-threads=1"])
            .env("RK_LIFETIME_CHILD", "1")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(flag) = flag {
            c.env(flag, "1");
        }
        let mut process = MockProcess(c.spawn().unwrap());
        let end = Instant::now() + Duration::from_secs(15);
        let status = loop {
            if let Some(status) = process.0.try_wait().unwrap() {
                break status;
            }
            assert!(Instant::now() < end, "bounded native child exceeded deadline");
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(status.success());
        let log = lab.native_log();
        for marker in ["create\n", "unschedule\n", "close\n", "release manager\n"] {
            assert_eq!(log.matches(marker).count(), 20, "{flag:?}: {marker}");
        }
        assert!(!log.contains("FORBIDDEN") && !log.contains("request mocked"));
    }
}

#[test]
fn cached_grant_revoke_idle_regrant_has_no_duplicate_listener() {
    let l = Lab::new();
    let _p = l.spawn(Some("RK_MOCK_CACHE"));
    l.wait(Access::Granted, 8);
    std::fs::write(l.dir.path().join("permission"), "D").unwrap();
    l.wait(Access::Denied, 14);
    std::thread::sleep(Duration::from_millis(300));
    let text = l.native_log();
    assert_eq!(text.matches("create\n").count(), 1);
    assert_eq!(text.matches("release manager").count(), 1);
    std::fs::write(l.dir.path().join("permission"), "G").unwrap();
    l.wait(Access::Granted, 14);
    let text = l.native_log();
    assert_eq!(text.matches("create\n").count(), 2);
    assert!(!text.contains("FORBIDDEN"));
    // Hang only fresh-process checks, never native daemon check(). Status
    // remains bounded and the unknown result silences the existing listener.
    std::fs::write(l.dir.path().join("permission"), "H").unwrap();
    l.wait(Access::Unknown, 14);
    let before = l.native_log().matches("helper check").count();
    for _ in 0..20 {
        let start = Instant::now();
        assert!(
            ipc::send(
                &l.dir.path().join("agent.sock"),
                &Request::Status,
                Duration::from_millis(250)
            )
            .is_ok()
        );
        assert!(start.elapsed() < Duration::from_millis(350));
    }
    assert_eq!(
        l.native_log().matches("helper check").count(),
        before,
        "status must not spawn helpers"
    );
    assert!(before <= 5);
}
#[test]
fn initially_cached_denial_recovers_by_bounded_reexec() {
    let l = Lab::new();
    std::fs::write(l.dir.path().join("permission"), "D").unwrap();
    let _p = l.spawn(Some("RK_MOCK_CACHE"));
    l.wait(Access::Denied, 5);
    std::fs::write(l.dir.path().join("permission"), "G").unwrap();
    l.wait(Access::Granted, 20);
    let text = l.native_log();
    assert_eq!(
        text.matches("request mocked: NO PROMPT").count(),
        1,
        "only initial request, no prompt loop"
    );
    assert_eq!(text.matches("create\n").count(), 1);
    assert!(!text.contains("FORBIDDEN"));
}

#[test]
fn native_open_failure_and_returned_loop_release_every_resource() {
    for flag in ["RK_MOCK_OPEN_FAIL", "RK_MOCK_RETURN_LOOP"] {
        let l = Lab::new();
        let _p = l.spawn(Some(flag));
        let end = Instant::now() + Duration::from_secs(8);
        while !l.native_log().contains("release manager") {
            assert!(
                Instant::now() < end,
                "native stderr {}",
                std::fs::read_to_string(l.dir.path().join("native.stderr")).unwrap_or_default()
            );
            std::thread::sleep(Duration::from_millis(100));
        }
        let text = l.native_log();
        assert_eq!(text.matches("create\n").count(), 1);
        assert_eq!(text.matches("unschedule").count(), 1);
        assert_eq!(text.matches("close\n").count(), 1);
        assert_eq!(text.matches("release manager").count(), 1);
        assert!(!text.contains("FORBIDDEN"));
        l.wait(Access::Unknown, 5);
    }
}
