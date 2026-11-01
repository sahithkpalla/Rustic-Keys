//! Private fixtures only: never starts a daemon, requests TCC or calls real launchctl.
use rustic_keys::{
    daemon,
    ipc::{self, Request, Response},
};
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::{
    fs::symlink,
    net::{UnixListener, UnixStream},
};
use std::time::{Duration, Instant};

#[test]
fn partial_peer_does_not_block_status_and_recovers() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("a.sock");
    ipc::serve(ipc::bind(&p).unwrap(), |_| Response::Ok);
    let mut slow = UnixStream::connect(&p).unwrap();
    slow.write_all(b"{").unwrap();
    std::thread::sleep(Duration::from_millis(30));
    assert_eq!(
        ipc::send(&p, &Request::Status, Duration::from_millis(300)).unwrap(),
        Response::Ok
    );
    drop(slow);
    assert_eq!(
        ipc::send(&p, &Request::Status, Duration::from_secs(1)).unwrap(),
        Response::Ok
    );
}

#[test]
fn trickle_reply_has_overall_deadline() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("a.sock");
    let l = UnixListener::bind(&p).unwrap();
    let t = std::thread::spawn(move || {
        let (mut s, _) = l.accept().unwrap();
        let mut b = [0; 64];
        let _ = s.read(&mut b);
        for c in b"{\"result\":\"ok\"}\n" {
            if s.write_all(&[*c]).is_err() {
                break;
            }
            std::thread::sleep(Duration::from_millis(30));
        }
    });
    let now = Instant::now();
    assert!(ipc::send(&p, &Request::Status, Duration::from_millis(100)).is_err());
    assert!(now.elapsed() < Duration::from_millis(350));
    t.join().unwrap();
}

#[test]
fn oversized_request_is_rejected() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("a.sock");
    ipc::serve(ipc::bind(&p).unwrap(), |_| Response::Ok);
    let mut s = UnixStream::connect(&p).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let mut bytes = vec![b' '; 8192];
    bytes.extend_from_slice(b"{\"cmd\":\"status\"}\n");
    let _ = s.write_all(&bytes);
    let mut b = String::new();
    let _ = s.read_to_string(&mut b);
    assert!(!b.contains("\"result\":\"ok\""), "{b}");
    assert_eq!(
        ipc::send(&p, &Request::Status, Duration::from_secs(1)).unwrap(),
        Response::Ok
    );
}

#[test]
fn log_symlink_never_truncates_victim() {
    let d = tempfile::tempdir().unwrap();
    let victim = d.path().join("victim");
    let log = d.path().join("agent.log");
    std::fs::write(&victim, vec![b'x'; 2000]).unwrap();
    symlink(&victim, &log).unwrap();
    assert!(daemon::trim_log(&log, 1000).is_err());
    assert_eq!(std::fs::metadata(&victim).unwrap().len(), 2000);
}

#[test]
fn overload_is_bounded_and_recovers() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("a.sock");
    ipc::serve(ipc::bind(&p).unwrap(), |_| Response::Ok);
    let mut peers = Vec::new();
    for _ in 0..ipc::MAX_PEERS {
        let mut s = UnixStream::connect(&p).unwrap();
        s.write_all(b"{").unwrap();
        peers.push(s);
    }
    std::thread::sleep(Duration::from_millis(50));
    let start = Instant::now();
    assert!(ipc::send(&p, &Request::Status, Duration::from_millis(150)).is_err());
    assert!(start.elapsed() < Duration::from_millis(400));
    // Trickling cannot extend absolute server deadline.
    for _ in 0..5 {
        for s in &mut peers {
            let _ = s.write_all(b" ");
        }
        std::thread::sleep(Duration::from_millis(180));
    }
    assert_eq!(
        ipc::send(&p, &Request::Status, Duration::from_secs(1)).unwrap(),
        Response::Ok
    );
}

#[test]
fn oversized_reply_and_malformed_disconnected_peer() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("a.sock");
    ipc::serve(ipc::bind(&p).unwrap(), |_| Response::Error {
        message: "x".repeat(ipc::MAX_REPLY * 2),
    });
    let r = ipc::send(&p, &Request::Status, Duration::from_secs(1)).unwrap();
    assert!(matches!(r,Response::Error{message} if message=="reply too large"));
    drop(UnixStream::connect(&p).unwrap());
    let mut s = UnixStream::connect(&p).unwrap();
    s.write_all(b"not-json\n").unwrap();
    s.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
    let mut b = String::new();
    s.read_to_string(&mut b).unwrap();
    assert!(b.contains("malformed request"));
}

#[test]
fn hostile_server_oversized_reply_is_capped() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("a.sock");
    let l = UnixListener::bind(&p).unwrap();
    let t = std::thread::spawn(move || {
        let (mut s, _) = l.accept().unwrap();
        let mut b = [0; 64];
        let _ = s.read(&mut b);
        let _ = s.write_all(&vec![b' '; 16384]);
    });
    assert!(ipc::send(&p, &Request::Status, Duration::from_secs(1)).is_err());
    t.join().unwrap();
}

#[test]
fn full_connect_backlog_obeys_deadline() {
    use std::os::fd::AsRawFd;
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("a.sock");
    let l = UnixListener::bind(&p).unwrap();
    unsafe {
        libc::listen(l.as_raw_fd(), 1);
    }
    // Nonblocking fillers avoid blocking the test itself at connect.
    let mut peers = Vec::new();
    for _ in 0..16 {
        let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
        assert!(fd >= 0);
        unsafe {
            libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK);
        }
        use std::os::unix::ffi::OsStrExt;
        let mut a: libc::sockaddr_un = unsafe { std::mem::zeroed() };
        a.sun_family = libc::AF_UNIX as _;
        a.sun_len = std::mem::size_of_val(&a) as _;
        for (v, b) in a.sun_path.iter_mut().zip(p.as_os_str().as_bytes()) {
            *v = *b as _;
        }
        unsafe {
            libc::connect(
                fd,
                (&a as *const libc::sockaddr_un).cast(),
                std::mem::size_of_val(&a) as _,
            );
        }
        peers.push(fd);
    }
    let now = Instant::now();
    assert!(ipc::send(&p, &Request::Status, Duration::from_millis(100)).is_err());
    assert!(now.elapsed() < Duration::from_millis(350));
    for fd in peers {
        unsafe {
            libc::close(fd);
        }
    }
}

#[test]
fn config_bounds_final_symlink_and_missing_home() {
    let d = tempfile::tempdir().unwrap();
    let victim = d.path().join("victim");
    std::fs::write(&victim, b"KEEP").unwrap();
    let config = d.path().join("config.toml");
    symlink(&victim, &config).unwrap();
    assert_cmd::cargo::cargo_bin_cmd!("rustic-keys")
        .env("RUSTIC_KEYS_HOME", d.path())
        .args(["volume", "50"])
        .assert()
        .failure();
    assert_eq!(std::fs::read(&victim).unwrap(), b"KEEP");
    std::fs::remove_file(&config).unwrap();
    for text in ["volume = 101".to_string(), "style = 'bogus'".into(), " ".repeat(8192)] {
        std::fs::write(&config, text).unwrap();
        assert_cmd::cargo::cargo_bin_cmd!("rustic-keys")
            .env("RUSTIC_KEYS_HOME", d.path())
            .arg("styles")
            .assert()
            .failure();
    }
    assert_cmd::cargo::cargo_bin_cmd!("rustic-keys")
        .env_remove("HOME")
        .env_remove("RUSTIC_KEYS_HOME")
        .arg("styles")
        .assert()
        .failure()
        .stderr(predicates::str::contains("HOME is absent"));
}

#[test]
fn unsafe_directory_refused_without_chmod_and_socket_is_private() {
    let d = tempfile::tempdir().unwrap();
    std::fs::set_permissions(d.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
    assert!(ipc::bind(&d.path().join("a.sock")).is_err());
    assert_eq!(std::fs::metadata(d.path()).unwrap().permissions().mode() & 0o777, 0o777);
    std::fs::set_permissions(d.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let p = d.path().join("a.sock");
    let _l = ipc::bind(&p).unwrap();
    assert_eq!(std::fs::metadata(p).unwrap().permissions().mode() & 0o777, 0o600);
}

#[test]
fn concurrent_config_saves_remain_complete_and_private() {
    let d = tempfile::tempdir().unwrap();
    let mut children = Vec::new();
    for n in 0..12 {
        let c = std::process::Command::new(assert_cmd::cargo::cargo_bin!("rustic-keys"))
            .env("RUSTIC_KEYS_HOME", d.path())
            .args(["volume", &(n * 8).to_string()])
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        children.push(c);
    }
    for mut c in children {
        assert!(c.wait().unwrap().success());
    }
    let p = d.path().join("config.toml");
    let c: rustic_keys::config::Config = toml::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
    assert!(c.volume <= 100);
    assert_eq!(std::fs::metadata(p).unwrap().permissions().mode() & 0o777, 0o600);
}

#[test]
fn probe_requires_consent_before_any_input_operation() {
    assert_cmd::cargo::cargo_bin_cmd!("rustic-keys")
        .arg("probe")
        .assert()
        .failure()
        .stderr(predicates::str::contains("--acknowledge-key-logging"));
}

#[test]
fn config_predictable_temp_symlink_never_overwrites() {
    let d = tempfile::tempdir().unwrap();
    let victim = d.path().join("victim");
    std::fs::write(&victim, b"KEEP").unwrap();
    symlink(&victim, d.path().join("config.toml.tmp")).unwrap();
    assert_cmd::cargo::cargo_bin_cmd!("rustic-keys")
        .env("RUSTIC_KEYS_HOME", d.path())
        .args(["volume", "50"])
        .assert()
        .success();
    assert_eq!(std::fs::read(&victim).unwrap(), b"KEEP");
    assert!(
        !std::fs::symlink_metadata(d.path().join("config.toml"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
}
