//! Bounded newline JSON control protocol. One nonblocking server thread, at most
//! eight peers, 1 KiB requests / 8 KiB replies, and absolute I/O deadlines.
use crate::filter::Mode;
use crate::input::permission::Access;
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub const MAX_REQUEST: usize = 1024;
pub const MAX_REPLY: usize = 8192;
pub const MAX_PEERS: usize = 8;
pub const PEER_TIMEOUT: Duration = Duration::from_millis(750);

#[derive(Serialize, Deserialize, Debug, PartialEq)]
#[serde(tag = "cmd", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    Reload,
    Status,
}
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct StatusReport {
    pub style: String,
    pub volume: u8,
    pub mode: Mode,
    pub permission: Access,
    pub external_keyboard_connected: bool,
    pub audio_device: Option<String>,
}
#[derive(Serialize, Deserialize, Debug, PartialEq)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Response {
    Ok,
    Status(StatusReport),
    Error { message: String },
}
#[derive(Debug)]
pub struct AlreadyRunning(pub std::path::PathBuf);
impl std::fmt::Display for AlreadyRunning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "another agent is starting or listening on {}", self.0.display())
    }
}
impl std::error::Error for AlreadyRunning {}

fn same_user(s: &UnixStream) -> Result<()> {
    let (mut uid, mut gid) = (0, 0);
    if unsafe { libc::getpeereid(s.as_raw_fd(), &mut uid, &mut gid) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    ensure!(
        uid == unsafe { libc::geteuid() },
        "control peer is not owned by this user"
    );
    Ok(())
}
fn socket_metadata(path: &Path) -> Result<Option<std::fs::Metadata>> {
    match std::fs::symlink_metadata(path) {
        Ok(m) => {
            ensure!(
                m.file_type().is_socket() && m.uid() == unsafe { libc::geteuid() },
                "refusing non-socket, symlink or foreign socket at {}",
                path.display()
            );
            Ok(Some(m))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn wait(fd: i32, events: i16, deadline: Instant) -> Result<()> {
    loop {
        let left = deadline
            .checked_duration_since(Instant::now())
            .context("IPC overall deadline exceeded")?;
        let mut p = libc::pollfd { fd, events, revents: 0 };
        let n = unsafe { libc::poll(&mut p, 1, left.as_millis().max(1).min(i32::MAX as u128) as i32) };
        if n > 0 {
            return Ok(());
        }
        if n == 0 {
            bail!("IPC overall deadline exceeded")
        }
        let e = std::io::Error::last_os_error();
        if e.kind() != std::io::ErrorKind::Interrupted {
            return Err(e.into());
        }
    }
}
/// Nonblocking connect includes full-backlog stalls in the caller's deadline.
fn connect(path: &Path, deadline: Instant) -> Result<UnixStream> {
    use std::os::unix::ffi::OsStrExt;
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as _;
    let bytes = path.as_os_str().as_bytes();
    ensure!(
        bytes.len() < addr.sun_path.len() && !bytes.contains(&0),
        "invalid or overlong socket path"
    );
    for (a, b) in addr.sun_path.iter_mut().zip(bytes) {
        *a = *b as _
    }
    addr.sun_len = std::mem::size_of_val(&addr) as _;
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let s = unsafe { UnixStream::from_raw_fd(fd) };
    s.set_nonblocking(true)?;
    unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    if unsafe {
        libc::connect(
            fd,
            (&addr as *const libc::sockaddr_un).cast(),
            std::mem::size_of_val(&addr) as _,
        )
    } != 0
    {
        let e = std::io::Error::last_os_error();
        if !matches!(e.raw_os_error(), Some(libc::EINPROGRESS) | Some(libc::EAGAIN)) {
            return Err(e.into());
        }
        wait(fd, libc::POLLOUT, deadline)?;
        let mut error: i32 = 0;
        let mut len = std::mem::size_of_val(&error) as libc::socklen_t;
        if unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_ERROR,
                (&mut error as *mut i32).cast(),
                &mut len,
            )
        } != 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        if error != 0 {
            return Err(std::io::Error::from_raw_os_error(error).into());
        }
    }
    same_user(&s)?;
    Ok(s)
}

/// The startup lock lives exactly as long as the socket, including after it
/// moves to the serving thread. Never unlink the lock file while an agent runs.
#[derive(Debug)]
pub struct BoundListener {
    listener: UnixListener,
    _startup_lock: File,
}
impl AsRawFd for BoundListener {
    fn as_raw_fd(&self) -> std::os::fd::RawFd {
        self.listener.as_raw_fd()
    }
}
impl BoundListener {
    fn accept(&self) -> std::io::Result<(UnixStream, std::os::unix::net::SocketAddr)> {
        self.listener.accept()
    }
}

pub fn bind(path: &Path) -> Result<BoundListener> {
    bind_inner(path, || {})
}

// The seam exposes the exact stale-check/unlink interleaving to a regression,
// without any native service or input listener.
fn bind_inner(path: &Path, after_stale_check: impl FnOnce()) -> Result<BoundListener> {
    let dir = crate::state_files::PrivateDir::open(path.parent().context("socket has no parent")?)?;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .context("invalid socket filename")?;
    let startup_lock = dir.open_file(&format!("{name}.lock"), true, true)?;
    // Check+unlink cannot be atomic by pathname. Serialize cooperating starts
    // and retain ownership for the full listening lifetime, not only bind().
    if unsafe { libc::flock(startup_lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let e = std::io::Error::last_os_error();
        if e.kind() == std::io::ErrorKind::WouldBlock {
            return Err(AlreadyRunning(path.into()).into());
        }
        return Err(e.into());
    }
    if let Some(m) = socket_metadata(path)? {
        match connect(path, Instant::now() + Duration::from_millis(150)) {
            Ok(_) => return Err(AlreadyRunning(path.into()).into()),
            Err(e)
                if e.downcast_ref::<std::io::Error>()
                    .is_some_and(|e| matches!(e.raw_os_error(), Some(libc::ECONNREFUSED) | Some(libc::ENOENT))) =>
            {
                let now = socket_metadata(path)?.context("socket changed during stale check")?;
                ensure!(
                    (m.dev(), m.ino()) == (now.dev(), now.ino()),
                    "socket changed during stale check"
                );
                after_stale_check();
                std::fs::remove_file(path)?;
            }
            Err(e) => return Err(e.context("cannot prove existing socket stale")),
        }
    }
    let l = UnixListener::bind(path).with_context(|| format!("binding {}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    l.set_nonblocking(true)?;
    Ok(BoundListener {
        listener: l,
        _startup_lock: startup_lock,
    })
}

struct Peer {
    stream: UnixStream,
    deadline: Instant,
    input: Vec<u8>,
    output: Option<Vec<u8>>,
    sent: usize,
}
fn reply(r: Response) -> Vec<u8> {
    let mut b = serde_json::to_vec(&r).unwrap_or_default();
    if b.len() + 1 > MAX_REPLY {
        b = br#"{"result":"error","message":"reply too large"}"#.to_vec()
    }
    b.push(b'\n');
    b
}
fn read_frame(s: &mut UnixStream, bytes: &mut Vec<u8>, limit: usize) -> Result<bool> {
    let mut buf = [0; 1024];
    loop {
        match s.read(&mut buf) {
            Ok(0) => bail!("disconnected before complete frame"),
            Ok(n) => {
                let end = buf[..n].iter().position(|b| *b == b'\n');
                let n = end.map_or(n, |i| i + 1);
                ensure!(bytes.len() + n <= limit, "frame too large");
                bytes.extend_from_slice(&buf[..n]);
                if end.is_some() {
                    return Ok(true);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(false),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.into()),
        }
    }
}
/// The handler must itself be bounded; the daemon uses a bounded queue and a
/// 100 ms reply wait. Saturation closes excess peers, without allocating tasks.
pub fn serve(listener: BoundListener, mut handler: impl FnMut(Request) -> Response + Send + 'static) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let mut peers: Vec<Peer> = Vec::with_capacity(MAX_PEERS);
        loop {
            let mut polls = Vec::with_capacity(MAX_PEERS + 1);
            polls.push(libc::pollfd {
                fd: listener.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            });
            for p in &peers {
                polls.push(libc::pollfd {
                    fd: p.stream.as_raw_fd(),
                    events: if p.output.is_some() {
                        libc::POLLOUT
                    } else {
                        libc::POLLIN
                    },
                    revents: 0,
                })
            }
            let timeout = peers
                .iter()
                .map(|p| p.deadline.saturating_duration_since(Instant::now()).as_millis().max(1) as i32)
                .min()
                .unwrap_or(-1);
            if unsafe { libc::poll(polls.as_mut_ptr(), polls.len() as _, timeout) } < 0 {
                continue;
            }
            // Bound accept work too, so a connection flood cannot starve existing peers.
            if polls[0].revents != 0 {
                for _ in 0..MAX_PEERS {
                    match listener.accept() {
                        Ok((s, _)) => {
                            if peers.len() < MAX_PEERS && same_user(&s).is_ok() && s.set_nonblocking(true).is_ok() {
                                peers.push(Peer {
                                    stream: s,
                                    deadline: Instant::now() + PEER_TIMEOUT,
                                    input: Vec::new(),
                                    output: None,
                                    sent: 0,
                                })
                            }
                        }
                        Err(_) => break,
                    }
                }
            }
            peers.retain_mut(|p| {
                if Instant::now() >= p.deadline {
                    return false;
                }
                if p.output.is_none() {
                    match read_frame(&mut p.stream, &mut p.input, MAX_REQUEST) {
                        Ok(false) => return true,
                        Ok(true) => {
                            p.output = Some(reply(match serde_json::from_slice(&p.input) {
                                Ok(r) => handler(r),
                                Err(_) => Response::Error {
                                    message: "malformed request".into(),
                                },
                            }))
                        }
                        Err(_) => return false,
                    }
                }
                if Instant::now() >= p.deadline {
                    return false;
                }
                let b = p.output.as_ref().unwrap();
                match p.stream.write(&b[p.sent..]) {
                    Ok(0) => false,
                    Ok(n) => {
                        p.sent += n;
                        p.sent < b.len()
                    }
                    Err(e) => e.kind() == std::io::ErrorKind::WouldBlock,
                }
            });
        }
    })
}

fn write_frame(s: &mut UnixStream, bytes: &[u8], deadline: Instant) -> Result<()> {
    let mut sent = 0;
    while sent < bytes.len() {
        wait(s.as_raw_fd(), libc::POLLOUT, deadline)?;
        match s.write(&bytes[sent..]) {
            Ok(0) => bail!("peer disconnected"),
            Ok(n) => sent += n,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

pub fn send(path: &Path, req: &Request, timeout: Duration) -> Result<Response> {
    let deadline = Instant::now() + timeout;
    let _dir = crate::state_files::PrivateDir::open(path.parent().context("socket has no parent")?)?;
    socket_metadata(path)?.context("agent socket absent")?;
    let mut s = connect(path, deadline).with_context(|| format!("agent not reachable at {}", path.display()))?;
    let mut b = serde_json::to_vec(req)?;
    b.push(b'\n');
    ensure!(b.len() <= MAX_REQUEST, "request too large");
    write_frame(&mut s, &b, deadline)?;
    let mut bytes = Vec::new();
    loop {
        wait(s.as_raw_fd(), libc::POLLIN, deadline)?;
        if read_frame(&mut s, &mut bytes, MAX_REPLY)? {
            break;
        }
    }
    ensure!(Instant::now() <= deadline, "IPC overall deadline exceeded");
    serde_json::from_slice(&bytes).context("parsing agent reply")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stalled_write_obeys_the_same_absolute_deadline() {
        let (mut writer, _not_reading) = UnixStream::pair().unwrap();
        writer.set_nonblocking(true).unwrap();
        loop {
            match writer.write(&[b'x'; 8192]) {
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => panic!("{e}"),
            }
        }
        let now = Instant::now();
        assert!(write_frame(&mut writer, b"x", now + Duration::from_millis(40)).is_err());
        assert!(now.elapsed() < Duration::from_millis(250));
    }
    #[test]
    fn roundtrip_and_duplicate() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("a.sock");
        serve(bind(&p).unwrap(), |_| Response::Ok);
        assert_eq!(
            send(&p, &Request::Status, Duration::from_secs(1)).unwrap(),
            Response::Ok
        );
        assert!(bind(&p).unwrap_err().downcast_ref::<AlreadyRunning>().is_some());
    }
    #[test]
    fn concurrent_stale_replacement_cannot_unlink_a_new_live_listener() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("a.sock");
        drop(UnixListener::bind(&path).unwrap());
        let (checked_tx, checked_rx) = std::sync::mpsc::channel();
        let (resume_tx, resume_rx) = std::sync::mpsc::channel();
        let other_path = path.clone();
        let contender = std::thread::spawn(move || {
            bind_inner(&other_path, || {
                checked_tx.send(()).unwrap();
                resume_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            })
        });
        checked_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        // The first contender has checked the stale inode but not unlinked it.
        // A racing start must not be able to replace it and then be unlinked.
        let racing = bind(&path);
        resume_tx.send(()).unwrap();
        let paused = contender.join().unwrap();
        assert_eq!(
            usize::from(racing.is_ok()) + usize::from(paused.is_ok()),
            1,
            "two live listeners must never be created for one pathname"
        );
        assert!(UnixStream::connect(&path).is_ok());
    }
    #[test]
    fn startup_lock_is_owned_and_released_with_listener() {
        use std::os::unix::fs::symlink;
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("a.sock");
        let first = bind(&path).unwrap();
        assert!(bind(&path).unwrap_err().downcast_ref::<AlreadyRunning>().is_some());
        drop(first);
        drop(bind(&path).unwrap());
        std::fs::remove_file(d.path().join("a.sock.lock")).unwrap();
        let victim = d.path().join("victim");
        std::fs::write(&victim, b"KEEP").unwrap();
        symlink(&victim, d.path().join("a.sock.lock")).unwrap();
        assert!(bind(&path).is_err());
        assert_eq!(std::fs::read(victim).unwrap(), b"KEEP");
    }
    #[test]
    fn stale_socket_and_malformed_path() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("a.sock");
        drop(UnixListener::bind(&p).unwrap());
        assert!(bind(&p).is_ok());
        let p = d.path().join("file");
        std::fs::write(&p, b"KEEP").unwrap();
        assert!(bind(&p).is_err());
        assert_eq!(std::fs::read(p).unwrap(), b"KEEP");
    }
}
