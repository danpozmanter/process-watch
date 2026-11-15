use std::ffi::{CStr, c_int};
use std::io;
use std::mem::size_of;
use std::time::{SystemTime, UNIX_EPOCH};

use super::{Proc, Snapshot};

/// Reads every process in one sysctl(KERN_PROC) call, reusing its buffer.
pub struct Lister {
    procs: Vec<libc::kinfo_proc>,
    page_bytes: i64,
}

impl Lister {
    /// A lister for this host's page size.
    pub fn new() -> Self {
        // SAFETY: getpagesize has no preconditions.
        let page = unsafe { libc::getpagesize() };
        Self {
            procs: Vec::new(),
            page_bytes: i64::from(page).max(1),
        }
    }

    /// Every process on the host right now.
    pub fn take(&mut self) -> io::Result<Snapshot> {
        let now_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX));
        let listed = self.query()?;
        let procs = self.procs[..listed]
            .iter()
            .filter(|kp| !is_idle(kp))
            .map(|kp| to_proc(kp, self.page_bytes))
            .collect();
        Ok(Snapshot {
            procs,
            now_ns,
            resolution_ns: 1_000,
            events: None,
        })
    }

    /// Fills the buffer with the process table and answers how many records it holds.
    fn query(&mut self) -> io::Result<usize> {
        let mut size = table_size()?;
        loop {
            // Room for processes started between the size query and the listing.
            let room = size / RECORD_SIZE + size / RECORD_SIZE / 8 + 16;
            // SAFETY: kinfo_proc is plain data whose all-zero value is valid.
            self.procs.resize(room, unsafe { std::mem::zeroed() });
            size = room * RECORD_SIZE;
            // SAFETY: the buffer holds `size` writable bytes.
            let rc = unsafe {
                libc::sysctl(
                    PROC_TABLE.as_ptr(),
                    4,
                    self.procs.as_mut_ptr().cast(),
                    &raw mut size,
                    std::ptr::null(),
                    0,
                )
            };
            if rc == 0 {
                break;
            }
            let err = io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::ENOMEM) {
                return Err(err);
            }
            // The table grew past the room; ask again with what it needs now.
            size = table_size()?;
        }
        let listed = size / RECORD_SIZE;
        match self.procs.first().filter(|_| listed > 0) {
            Some(kp) if kp.ki_structsize as usize != RECORD_SIZE => Err(io::Error::other(format!(
                "the kernel's process records are {} bytes, this build expects {RECORD_SIZE}",
                kp.ki_structsize
            ))),
            _ => Ok(listed),
        }
    }
}

/// The sysctl MIB naming every process.
const PROC_TABLE: [c_int; 4] = [libc::CTL_KERN, libc::KERN_PROC, libc::KERN_PROC_PROC, 0];

const RECORD_SIZE: usize = size_of::<libc::kinfo_proc>();

/// The bytes the process table takes right now.
fn table_size() -> io::Result<usize> {
    let mut size = 0usize;
    // SAFETY: a null buffer asks only for the table's size.
    let rc = unsafe {
        libc::sysctl(
            PROC_TABLE.as_ptr(),
            4,
            std::ptr::null_mut(),
            &raw mut size,
            std::ptr::null(),
            0,
        )
    };
    if rc == 0 {
        Ok(size)
    } else {
        Err(io::Error::last_os_error())
    }
}

/// The idle kernel process, whose CPU time is idle time.
fn is_idle(kp: &libc::kinfo_proc) -> bool {
    (kp.ki_flag & libc::P_KPROC as libc::c_long) != 0 && c_name(&kp.ki_comm) == "idle"
}

fn to_proc(kp: &libc::kinfo_proc, page_bytes: i64) -> Proc {
    Proc {
        pid: i64::from(kp.ki_pid),
        ppid: i64::from(kp.ki_ppid),
        pgid: i64::from(kp.ki_pgid),
        start_ns: timeval_ns(kp.ki_start),
        cpu_ns: i64::try_from(kp.ki_runtime)
            .unwrap_or(i64::MAX)
            .saturating_mul(1_000),
        child_cpu_ns: timeval_ns(kp.ki_childtime),
        mem_bytes: i64::try_from(kp.ki_rssize).unwrap_or(0) * page_bytes,
        name: c_name(&kp.ki_comm),
    }
}

/// Logical CPUs online, the unit a 100% CPU reading is one of.
// `c_long` is 32 bits on 32-bit FreeBSD targets, so the conversion is not
// redundant everywhere it compiles.
#[allow(clippy::useless_conversion)]
pub fn cpu_count() -> i64 {
    // SAFETY: sysconf reads a configuration value and has no preconditions.
    i64::from(unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) }).max(1)
}

// `time_t` and `suseconds_t` are 32 bits on FreeBSD/i386, so the conversions
// are not redundant everywhere it compiles.
#[allow(clippy::useless_conversion)]
fn timeval_ns(tv: libc::timeval) -> i64 {
    i64::from(tv.tv_sec) * 1_000_000_000 + i64::from(tv.tv_usec) * 1_000
}

fn c_name(raw: &[libc::c_char]) -> String {
    // SAFETY: c_char and u8 have the same size and alignment.
    let bytes = unsafe { std::slice::from_raw_parts(raw.as_ptr().cast::<u8>(), raw.len()) };
    match CStr::from_bytes_until_nul(bytes) {
        Ok(name) => name.to_string_lossy().into_owned(),
        Err(_) => String::from_utf8_lossy(bytes).into_owned(),
    }
}
