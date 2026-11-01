//! Narrow, descriptor-relative operations for application state and the plist.
//! Never follow symlinks; validate ownership on opened descriptors, not path checks.
use anyhow::{Context, Result, bail, ensure};
use std::ffi::CString;
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path};

pub struct PrivateDir(File);
fn name(s: &std::ffi::OsStr) -> Result<CString> {
    Ok(CString::new(s.as_bytes())?)
}
fn cvt(fd: i32) -> std::io::Result<File> {
    if fd < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}
impl PrivateDir {
    pub fn open(path: &Path) -> Result<Self> {
        ensure!(
            unsafe { libc::geteuid() } != 0,
            "run Rustic Keys as your login user, not root"
        );
        ensure!(
            path.is_absolute(),
            "state directory must be absolute: {}",
            path.display()
        );
        let mut dir = File::open("/")?;
        for part in path.components() {
            let Component::Normal(part) = part else {
                ensure!(
                    matches!(part, Component::RootDir),
                    "invalid state directory {}",
                    path.display()
                );
                continue;
            };
            let n = name(part)?;
            let mut next = unsafe {
                libc::openat(
                    dir.as_raw_fd(),
                    n.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if next < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::NotFound {
                let r = unsafe { libc::mkdirat(dir.as_raw_fd(), n.as_ptr(), 0o700) };
                if r != 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::AlreadyExists {
                    return Err(std::io::Error::last_os_error().into());
                }
                next = unsafe {
                    libc::openat(
                        dir.as_raw_fd(),
                        n.as_ptr(),
                        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                    )
                };
            }
            dir = cvt(next).with_context(|| format!("opening directory {} without symlinks", path.display()))?;
            let m = dir.metadata()?;
            let uid = unsafe { libc::geteuid() };
            ensure!(
                m.uid() == uid || m.uid() == 0,
                "directory not owned by you or root: {}",
                path.display()
            );
            ensure!(
                m.mode() & 0o022 == 0 || (m.uid() == 0 && m.mode() & libc::S_ISVTX as u32 != 0),
                "directory writable by others: {}",
                path.display()
            );
        }
        let m = dir.metadata()?;
        ensure!(
            m.uid() == unsafe { libc::geteuid() },
            "application directory not owned by you: {}",
            path.display()
        );
        // Upgrade an owned, non-shared legacy 0755 directory without changing
        // its contents. Never chmod a foreign directory or a path-followed link.
        if m.mode() & 0o077 != 0 && unsafe { libc::fchmod(dir.as_raw_fd(), 0o700) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(Self(dir))
    }
    pub fn open_file(&self, file: &str, write: bool, create: bool) -> Result<File> {
        let n = name(file.as_ref())?;
        let fd = unsafe {
            libc::openat(
                self.0.as_raw_fd(),
                n.as_ptr(),
                (if write { libc::O_RDWR } else { libc::O_RDONLY })
                    | libc::O_NOFOLLOW
                    | libc::O_CLOEXEC
                    | libc::O_NONBLOCK
                    | if create { libc::O_CREAT } else { 0 },
                0o600,
            )
        };
        let f = cvt(fd).with_context(|| format!("opening {file} without symlinks"))?;
        let m = f.metadata()?;
        ensure!(
            m.is_file() && m.uid() == unsafe { libc::geteuid() } && m.nlink() <= 1 && m.mode() & 0o022 == 0,
            "{file} must be an owned regular file, not shared or linked"
        );
        Ok(f)
    }
    pub fn read(&self, file: &str, limit: usize) -> Result<Option<Vec<u8>>> {
        let f = match self.open_file(file, false, false) {
            Ok(f) => f,
            Err(e)
                if e.downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
            {
                return Ok(None);
            }
            Err(e) => return Err(e),
        };
        let mut bytes = Vec::new();
        f.take(limit as u64 + 1).read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= limit, "{file} exceeds {limit} bytes");
        Ok(Some(bytes))
    }
    pub fn atomic_write(&self, file: &str, bytes: &[u8]) -> Result<()> {
        // Refuse existing links/special files. Rename itself never opens the destination,
        // so a same-user replacement race cannot overwrite a symlink's referent.
        let _ = self.read(file, 65536)?;
        let tmp = format!(
            ".save-{:016x}{:016x}",
            unsafe { libc::arc4random() } as u64,
            unsafe { libc::arc4random() } as u64
        );
        let n = name(tmp.as_ref())?;
        let dst = name(file.as_ref())?;
        let mut f = cvt(unsafe {
            libc::openat(
                self.0.as_raw_fd(),
                n.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        })?;
        let result = (|| -> Result<()> {
            f.write_all(bytes)?;
            f.sync_all()?;
            if unsafe { libc::renameat(self.0.as_raw_fd(), n.as_ptr(), self.0.as_raw_fd(), dst.as_ptr()) } != 0 {
                bail!(std::io::Error::last_os_error());
            }
            self.0.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            unsafe { libc::unlinkat(self.0.as_raw_fd(), n.as_ptr(), 0) };
        }
        result
    }
    pub fn remove(&self, file: &str) -> Result<bool> {
        if self.read(file, 65536)?.is_none() {
            return Ok(false);
        }
        let n = name(file.as_ref())?;
        if unsafe { libc::unlinkat(self.0.as_raw_fd(), n.as_ptr(), 0) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(true)
    }
}

pub fn append_log(path: &Path, text: &[u8], max: u64) -> Result<()> {
    use std::io::{Seek, SeekFrom};
    let dir = PrivateDir::open(path.parent().context("log has no parent")?)?;
    let file = path.file_name().and_then(|n| n.to_str()).context("invalid log path")?;
    let mut f = dir.open_file(file, true, true)?;
    let bytes = &text[..text.len().min(4096).min(max as usize)];
    if f.metadata()?.len().saturating_add(bytes.len() as u64) > max {
        f.set_len(0)?;
    }
    f.seek(SeekFrom::End(0))?;
    f.write_all(bytes)?;
    Ok(())
}

pub fn trim_log(path: &Path, max: u64) -> Result<bool> {
    let dir = PrivateDir::open(path.parent().context("log has no parent")?)?;
    let Some(file) = path.file_name().and_then(|n| n.to_str()) else {
        bail!("invalid log path")
    };
    let f = match dir.open_file(file, true, false) {
        Ok(f) => f,
        Err(e)
            if e.downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
        {
            return Ok(false);
        }
        Err(e) => return Err(e),
    };
    if f.metadata()?.len() > max {
        f.set_len(0)?;
        Ok(true)
    } else {
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    #[test]
    fn runtime_log_stays_capped_and_refuses_links() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("agent.log");
        for _ in 0..100 {
            append_log(&p, &[b'x'; 300], 1024).unwrap();
            assert!(std::fs::metadata(&p).unwrap().len() <= 1024);
        }
        std::fs::remove_file(&p).unwrap();
        let victim = d.path().join("victim");
        std::fs::write(&victim, b"KEEP").unwrap();
        for hard in [false, true] {
            if hard {
                std::fs::hard_link(&victim, &p).unwrap();
            } else {
                symlink(&victim, &p).unwrap();
            }
            assert!(append_log(&p, &[b'x'; 2048], 1024).is_err());
            assert_eq!(std::fs::read(&victim).unwrap(), b"KEEP");
            std::fs::remove_file(&p).unwrap();
        }
    }
    #[test]
    fn atomic_write_refuses_special_files_and_symlinked_parent() {
        let d = tempfile::tempdir().unwrap();
        let dir = PrivateDir::open(d.path()).unwrap();
        let p = d.path().join("config.toml");
        let n = CString::new(p.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(n.as_ptr(), 0o600) }, 0);
        assert!(dir.atomic_write("config.toml", b"new").is_err());
        let link = d.path().join("alias");
        symlink(d.path(), &link).unwrap();
        assert!(PrivateDir::open(&link).is_err());
    }
}
