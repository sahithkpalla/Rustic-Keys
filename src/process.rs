//! Bounded helper execution: drain capped output, enforce a wall deadline, reap.
use anyhow::{Result, bail};
use std::io::Read;
use std::os::fd::AsRawFd;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

fn drain(reader: &mut impl Read, bytes: &mut Vec<u8>) -> Result<()> {
    let mut buf = [0; 1024];
    for _ in 0..17 {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => bytes.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        }
        if bytes.len() > 16384 {
            bail!("helper output exceeded 16 KiB")
        }
    }
    Ok(())
}
pub fn output(command: &mut Command, timeout: Duration) -> Result<Output> {
    let mut child = command.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    let mut out = child.stdout.take().unwrap();
    let mut err = child.stderr.take().unwrap();
    let result = (|| -> Result<Output> {
        for fd in [out.as_raw_fd(), err.as_raw_fd()] {
            let f = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            if f < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, f | libc::O_NONBLOCK) } < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
        }
        let deadline = Instant::now() + timeout;
        let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
        loop {
            drain(&mut out, &mut stdout)?;
            drain(&mut err, &mut stderr)?;
            if let Some(status) = child.try_wait()? {
                // Catch bytes written between our preceding drain and child exit.
                drain(&mut out, &mut stdout)?;
                drain(&mut err, &mut stderr)?;
                return Ok(Output { status, stdout, stderr });
            }
            if Instant::now() >= deadline {
                bail!("helper deadline exceeded")
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    })();
    if result.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn helper_timeout_is_bounded() {
        let t = Instant::now();
        assert!(output(Command::new("/bin/sleep").arg("5"), Duration::from_millis(40)).is_err());
        assert!(t.elapsed() < Duration::from_secs(1));
    }
    #[test]
    fn helper_output_is_capped_and_exit_bytes_retained() {
        let out = output(Command::new("/bin/echo").arg("last bytes"), Duration::from_secs(1)).unwrap();
        assert_eq!(out.stdout, b"last bytes\n");
        assert!(output(&mut Command::new("/usr/bin/yes"), Duration::from_secs(1)).is_err());
    }
}
