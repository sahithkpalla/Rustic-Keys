//! Control protocol between the CLI and the agent: one JSON line per
//! request, one JSON line per response, over a Unix socket.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::filter::Mode;
use crate::input::permission::Access;

#[derive(Serialize, Deserialize, Debug, PartialEq)]
#[serde(tag = "cmd", rename_all = "snake_case")]
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

/// Bind the agent socket. A leftover socket file from a dead agent is
/// replaced; if a live agent answers, binding fails.
pub fn bind(path: &Path) -> Result<UnixListener> {
    if path.exists() {
        if UnixStream::connect(path).is_ok() {
            bail!("another agent is already listening on {}", path.display());
        }
        std::fs::remove_file(path).with_context(|| format!("removing stale {}", path.display()))?;
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    UnixListener::bind(path).with_context(|| format!("binding {}", path.display()))
}

/// Serve requests forever on a background thread, one connection at a time.
pub fn serve(listener: UnixListener, mut handler: impl FnMut(Request) -> Response + Send + 'static) -> JoinHandle<()> {
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
            let mut line = String::new();
            if BufReader::new(&stream).read_line(&mut line).is_err() {
                continue;
            }
            let response = match serde_json::from_str::<Request>(line.trim()) {
                Ok(req) => handler(req),
                Err(e) => Response::Error { message: format!("bad request: {e}") },
            };
            let mut stream = stream;
            if let Ok(mut text) = serde_json::to_string(&response) {
                text.push('\n');
                let _ = stream.write_all(text.as_bytes());
            }
        }
    })
}

/// Send one request to the agent and wait (up to `timeout`) for its reply.
pub fn send(path: &Path, req: &Request, timeout: Duration) -> Result<Response> {
    let mut stream = UnixStream::connect(path).with_context(|| format!("agent not reachable at {}", path.display()))?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    let mut text = serde_json::to_string(req)?;
    text.push('\n');
    stream.write_all(text.as_bytes())?;
    let mut line = String::new();
    BufReader::new(&stream).read_line(&mut line).context("reading agent reply")?;
    serde_json::from_str(line.trim()).context("parsing agent reply")
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: Duration = Duration::from_secs(2);

    fn report() -> StatusReport {
        StatusReport {
            style: "thocky".into(),
            volume: 55,
            mode: Mode::PerKey,
            permission: Access::Granted,
            external_keyboard_connected: true,
            audio_device: Some("MacBook Pro Speakers".into()),
        }
    }

    #[test]
    fn status_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.sock");
        serve(bind(&path).unwrap(), |req| match req {
            Request::Status => Response::Status(report()),
            Request::Reload => Response::Ok,
        });
        assert_eq!(send(&path, &Request::Status, T).unwrap(), Response::Status(report()));
        // Server keeps serving after the first connection.
        assert_eq!(send(&path, &Request::Status, T).unwrap(), Response::Status(report()));
    }

    #[test]
    fn reload_returns_ok() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.sock");
        serve(bind(&path).unwrap(), |_| Response::Ok);
        assert_eq!(send(&path, &Request::Reload, T).unwrap(), Response::Ok);
    }

    #[test]
    fn send_without_server_errors() {
        let dir = tempfile::tempdir().unwrap();
        assert!(send(&dir.path().join("none.sock"), &Request::Status, T).is_err());
    }

    #[test]
    fn bind_replaces_stale_socket() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.sock");
        std::fs::write(&path, b"stale").unwrap();
        assert!(bind(&path).is_ok());
    }

    #[test]
    fn bind_refuses_when_live() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.sock");
        serve(bind(&path).unwrap(), |_| Response::Ok);
        assert!(bind(&path).is_err());
    }
}
