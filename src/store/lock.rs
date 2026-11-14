use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;

use anyhow::{Context, Result, bail};

const LOCK_FILE: &str = "recorder.lock";

/// The exclusive lock one recorder holds on its data folder while it runs:
/// an `fcntl` write lock over the whole file on Unix, `LockFileEx` on Windows.
/// The OS releases it when the recorder exits, however it exits.
#[derive(Debug)]
pub struct RecorderLock {
    _file: File,
}

impl RecorderLock {
    /// Takes the folder's lock, or fails when another recorder holds it.
    pub fn acquire(dir: &Path) -> Result<Self> {
        let path = dir.join(LOCK_FILE);
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        if !try_lock(&file).with_context(|| format!("locking {}", path.display()))? {
            bail!("another recorder is already writing to {}", dir.display());
        }
        Ok(Self { _file: file })
    }

    /// Whether a recorder holds the folder's lock right now.
    pub fn is_held(dir: &Path) -> bool {
        match OpenOptions::new().write(true).open(dir.join(LOCK_FILE)) {
            // Taking the lock and dropping it at once leaves the folder as it was.
            Ok(file) => matches!(try_lock(&file), Ok(false)),
            Err(_) => false,
        }
    }
}

/// Whether the lock was taken; `false` when another process holds it.
#[cfg(unix)]
fn try_lock(file: &File) -> io::Result<bool> {
    use std::os::fd::AsRawFd;
    // SAFETY: `flock` is plain data whose all-zero value is valid.
    let mut lock: libc::flock = unsafe { std::mem::zeroed() };
    lock.l_type = libc::F_WRLCK as libc::c_short;
    lock.l_whence = libc::SEEK_SET as libc::c_short;
    // SAFETY: `lock` is initialised and the descriptor is owned by `file` for the call.
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETLK, &raw const lock) } == 0 {
        return Ok(true);
    }
    let err = io::Error::last_os_error();
    match err.raw_os_error() {
        Some(libc::EACCES | libc::EAGAIN) => Ok(false),
        _ => Err(err),
    }
}

/// Whether the lock was taken; `false` when another process holds it.
#[cfg(windows)]
fn try_lock(file: &File) -> io::Result<bool> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::ERROR_LOCK_VIOLATION;
    use windows_sys::Win32::Storage::FileSystem::{
        LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY, LockFileEx,
    };
    use windows_sys::Win32::System::IO::OVERLAPPED;
    // SAFETY: OVERLAPPED is plain data whose all-zero value is valid.
    let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
    // SAFETY: the handle is owned by `file` and `overlapped` outlives the call.
    let ok = unsafe {
        LockFileEx(
            file.as_raw_handle(),
            LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
            0,
            u32::MAX,
            u32::MAX,
            &raw mut overlapped,
        )
    };
    if ok != 0 {
        return Ok(true);
    }
    let err = io::Error::last_os_error();
    if err.raw_os_error() == Some(ERROR_LOCK_VIOLATION as i32) {
        Ok(false)
    } else {
        Err(err)
    }
}
