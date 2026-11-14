use std::ffi::{CStr, c_int, c_void};
use std::io;
use std::mem::{MaybeUninit, size_of};
use std::time::{SystemTime, UNIX_EPOCH};

use super::{Proc, Snapshot};

#[repr(C)]
#[derive(Default)]
struct MachTimebaseInfo {
    numer: u32,
    denom: u32,
}

unsafe extern "C" {
    fn mach_timebase_info(info: *mut MachTimebaseInfo) -> c_int;
}

/// Reads processes through libproc, reusing one pid buffer across samples.
pub struct Lister {
    pids: Vec<libc::pid_t>,
    timebase: MachTimebaseInfo,
}

impl Lister {
    /// A lister that converts CPU times with this host's Mach timebase.
    pub fn new() -> Self {
        let mut timebase = MachTimebaseInfo::default();
        // SAFETY: `timebase` is a valid struct the call writes into.
        unsafe { mach_timebase_info(&raw mut timebase) };
        if timebase.denom == 0 {
            timebase = MachTimebaseInfo { numer: 1, denom: 1 };
        }
        Self {
            pids: Vec::new(),
            timebase,
        }
    }

    /// Every process on the host right now.
    pub fn take(&mut self) -> io::Result<Snapshot> {
        let now_ns = epoch_ns(SystemTime::now());
        // SAFETY: a null buffer asks only for the number of processes.
        let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
        if count <= 0 {
            return Err(io::Error::last_os_error());
        }
        // Room for processes started between the count and the listing.
        self.pids.resize(count as usize + 64, 0);
        let bytes = c_int::try_from(self.pids.len() * size_of::<libc::pid_t>())
            .map_err(|_| io::Error::other("too many processes"))?;
        // SAFETY: the buffer holds `bytes` bytes of pid_t slots.
        let listed =
            unsafe { libc::proc_listallpids(self.pids.as_mut_ptr().cast::<c_void>(), bytes) };
        if listed < 0 {
            return Err(io::Error::last_os_error());
        }
        let listed = (listed as usize).min(self.pids.len());
        let procs = self.pids[..listed]
            .iter()
            .filter_map(|&pid| self.read_proc(pid))
            .collect();
        Ok(Snapshot {
            procs,
            now_ns,
            resolution_ns: 1_000,
        })
    }

    fn read_proc(&self, pid: libc::pid_t) -> Option<Proc> {
        let mut info = MaybeUninit::<libc::proc_bsdinfo>::zeroed();
        let size = size_of::<libc::proc_bsdinfo>() as c_int;
        // SAFETY: `info` has room for one proc_bsdinfo, which the call fills.
        let filled = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                info.as_mut_ptr().cast(),
                size,
            )
        };
        if filled != size {
            return None;
        }
        // SAFETY: the call filled the whole struct.
        let info = unsafe { info.assume_init() };
        let name = if info.pbi_name[0] != 0 {
            &info.pbi_name[..]
        } else {
            &info.pbi_comm[..]
        };
        let mut proc = Proc {
            pid: i64::from(pid),
            ppid: i64::from(info.pbi_ppid),
            pgid: i64::from(info.pbi_pgid),
            start_ns: (info.pbi_start_tvsec as i64) * 1_000_000_000
                + (info.pbi_start_tvusec as i64) * 1_000,
            name: c_name(name),
            ..Proc::default()
        };
        let mut usage = MaybeUninit::<libc::rusage_info_v2>::zeroed();
        // SAFETY: `usage` has room for the RUSAGE_INFO_V2 record the call fills.
        let rc =
            unsafe { libc::proc_pid_rusage(pid, libc::RUSAGE_INFO_V2, usage.as_mut_ptr().cast()) };
        // Other users' usage is readable only by root; their processes count with none.
        if rc == 0 {
            // SAFETY: a zero return means the call filled the record.
            let usage = unsafe { usage.assume_init() };
            proc.cpu_ns = self.to_ns(usage.ri_user_time + usage.ri_system_time);
            proc.child_cpu_ns = self.to_ns(usage.ri_child_user_time + usage.ri_child_system_time);
            proc.mem_bytes = i64::try_from(usage.ri_phys_footprint).unwrap_or(i64::MAX);
        }
        Some(proc)
    }

    fn to_ns(&self, mach_ticks: u64) -> i64 {
        let ns = u128::from(mach_ticks) * u128::from(self.timebase.numer)
            / u128::from(self.timebase.denom);
        i64::try_from(ns).unwrap_or(i64::MAX)
    }
}

/// Logical CPUs online, the unit a 100% CPU reading is one of.
pub fn cpu_count() -> i64 {
    // SAFETY: sysconf reads a configuration value and has no preconditions.
    unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) }.max(1)
}

fn epoch_ns(at: SystemTime) -> i64 {
    at.duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX))
}

fn c_name(raw: &[libc::c_char]) -> String {
    // SAFETY: c_char and u8 have the same size and alignment.
    let bytes = unsafe { std::slice::from_raw_parts(raw.as_ptr().cast::<u8>(), raw.len()) };
    match CStr::from_bytes_until_nul(bytes) {
        Ok(name) => name.to_string_lossy().into_owned(),
        Err(_) => String::from_utf8_lossy(bytes).into_owned(),
    }
}
