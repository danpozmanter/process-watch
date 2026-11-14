use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, Read};

use super::{Proc, Snapshot};

/// The kernel keeps at most this many bytes of a process's name in `comm`.
const COMM_LEN: usize = 15;

/// `PF_KTHREAD` in the flags field of /proc/pid/stat: a kernel thread, whose
/// `comm` is its whole name.
const PF_KTHREAD: i64 = 0x0020_0000;

/// A process's identity across samples: pid, start time, and `comm`, which
/// an exec changes along with the executable.
type NameKey = (i64, i64, String);

/// Reads processes from /proc, reusing one read buffer across samples and
/// resolving each long name once per process rather than once per sample.
pub struct Lister {
    ns_per_tick: i64,
    page_bytes: i64,
    buf: Vec<u8>,
    full_names: HashMap<NameKey, String>,
    next_full_names: HashMap<NameKey, String>,
}

impl Lister {
    /// A lister for this host's clock-tick and page sizes.
    pub fn new() -> Self {
        Self {
            ns_per_tick: 1_000_000_000 / sysconf(libc::_SC_CLK_TCK).max(1),
            page_bytes: sysconf(libc::_SC_PAGESIZE).max(1),
            buf: Vec::with_capacity(1024),
            full_names: HashMap::new(),
            next_full_names: HashMap::new(),
        }
    }

    /// Every process on the host right now.
    pub fn take(&mut self) -> io::Result<Snapshot> {
        let now_ns = boot_clock_ns();
        let mut procs = Vec::with_capacity(1024);
        for entry in fs::read_dir("/proc")? {
            let name = entry?.file_name();
            let Some(pid) = name
                .to_str()
                .filter(|s| s.bytes().all(|b| b.is_ascii_digit()))
            else {
                continue;
            };
            // A process that exits while the listing walks /proc is skipped.
            if let Some(proc) = self.read_proc(pid) {
                procs.push(proc);
            }
        }
        // Names of processes that have exited are dropped with them.
        std::mem::swap(&mut self.full_names, &mut self.next_full_names);
        self.next_full_names.clear();
        Ok(Snapshot {
            procs,
            now_ns,
            resolution_ns: self.ns_per_tick,
        })
    }

    fn read_proc(&mut self, pid: &str) -> Option<Proc> {
        read_into(&format!("/proc/{pid}/stat"), &mut self.buf).ok()?;
        let stat = Stat::parse(&self.buf)?;
        let mut proc = stat.to_proc(self.ns_per_tick);
        proc.name = self.name_of(pid, &proc, stat);
        if read_into(&format!("/proc/{pid}/statm"), &mut self.buf).is_ok() {
            proc.mem_bytes = private_resident_pages(&self.buf) * self.page_bytes;
        }
        Some(proc)
    }

    /// The process's full name: `comm`, unless the kernel cut it short.
    fn name_of(&mut self, pid: &str, proc: &Proc, stat: Stat) -> String {
        if stat.comm.len() < COMM_LEN || stat.kernel_thread {
            return stat.comm;
        }
        let key = (proc.pid, proc.start_ns, stat.comm);
        let name = match self.full_names.remove(&key) {
            Some(name) => name,
            None => full_name(pid, &key.2),
        };
        self.next_full_names.insert(key, name.clone());
        name
    }
}

/// The fields pw reads from /proc/pid/stat, in clock ticks where they are times.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Stat {
    pid: i64,
    comm: String,
    ppid: i64,
    pgid: i64,
    kernel_thread: bool,
    cpu_ticks: i64,
    child_cpu_ticks: i64,
    start_ticks: i64,
}

impl Stat {
    fn parse(stat: &[u8]) -> Option<Self> {
        // `comm` may hold spaces and parentheses, so it ends at the last ')'.
        let open = stat.iter().position(|&b| b == b'(')?;
        let close = stat.iter().rposition(|&b| b == b')')?;
        if close < open {
            return None;
        }
        let pid = std::str::from_utf8(&stat[..open])
            .ok()?
            .trim()
            .parse()
            .ok()?;
        // Fields after `comm`, numbered as in proc(5) from 3 (state) on.
        let fields: Vec<i64> = std::str::from_utf8(stat.get(close + 2..)?)
            .ok()?
            .split_ascii_whitespace()
            .take(20)
            .map(|f| f.parse().unwrap_or(0))
            .collect();
        let field = |n: usize| fields.get(n - 3).copied().unwrap_or(0);
        Some(Self {
            pid,
            comm: String::from_utf8_lossy(&stat[open + 1..close]).into_owned(),
            ppid: field(4),
            pgid: field(5),
            kernel_thread: field(9) & PF_KTHREAD != 0,
            cpu_ticks: field(14) + field(15),
            child_cpu_ticks: field(16) + field(17),
            start_ticks: field(22),
        })
    }

    /// The process with its times in nanoseconds, before its name and memory are read.
    fn to_proc(&self, ns_per_tick: i64) -> Proc {
        Proc {
            pid: self.pid,
            ppid: self.ppid,
            pgid: self.pgid,
            start_ns: self.start_ticks * ns_per_tick,
            cpu_ns: self.cpu_ticks * ns_per_tick,
            child_cpu_ns: self.child_cpu_ticks * ns_per_tick,
            mem_bytes: 0,
            name: String::new(),
        }
    }
}

/// Resident pages the process does not share, from /proc/pid/statm's
/// second (resident) and third (shared) fields.
fn private_resident_pages(statm: &[u8]) -> i64 {
    let mut pages = std::str::from_utf8(statm)
        .unwrap_or("")
        .split_ascii_whitespace()
        .skip(1)
        .map(|f| f.parse::<i64>().unwrap_or(0));
    let resident = pages.next().unwrap_or(0);
    let shared = pages.next().unwrap_or(0);
    (resident - shared).max(0)
}

/// Logical CPUs online, the unit a 100% CPU reading is one of.
pub fn cpu_count() -> i64 {
    sysconf(libc::_SC_NPROCESSORS_ONLN).max(1)
}

// `c_long` is 32 bits on 32-bit Linux targets, so the conversion is not
// redundant everywhere it compiles.
#[allow(clippy::useless_conversion)]
fn sysconf(name: libc::c_int) -> i64 {
    // SAFETY: sysconf reads a configuration value and has no preconditions.
    i64::from(unsafe { libc::sysconf(name) })
}

/// Reads a /proc file with as few system calls as it takes: procfs hands
/// over a whole record in one read when the buffer can hold it, so a read
/// that leaves room in the buffer has reached the end.
fn read_into(path: &str, buf: &mut Vec<u8>) -> io::Result<()> {
    let mut file = File::open(path)?;
    buf.clear();
    loop {
        let filled = buf.len();
        buf.resize(buf.capacity().max(filled + 1024), 0);
        let room = buf.len() - filled;
        let n = file.read(&mut buf[filled..])?;
        buf.truncate(filled + n);
        if n < room {
            return Ok(());
        }
    }
}

// /proc start times count from boot, suspended time included.
fn boot_clock_ns() -> i64 {
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `now` is a valid timespec the call writes into.
    unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &raw mut now) };
    now.tv_sec * 1_000_000_000 + now.tv_nsec
}

/// The kernel cuts `comm` short; the executable's file name, when it extends
/// that prefix, is the full name.
fn full_name(pid: &str, comm: &str) -> String {
    let named = match fs::read_link(format!("/proc/{pid}/exe")) {
        Ok(path) => extend_comm(comm, &path.to_string_lossy(), Source::ExeLink),
        // Another user's executable link is unreadable; argv[0] is world-readable.
        Err(_) => match fs::read(format!("/proc/{pid}/cmdline")) {
            Ok(cmdline) => {
                let argv0 = cmdline.split(|&b| b == 0).next().unwrap_or(&[]);
                extend_comm(comm, &String::from_utf8_lossy(argv0), Source::Argv0)
            }
            Err(_) => None,
        },
    };
    named.unwrap_or_else(|| comm.to_owned())
}

/// Where a candidate full name came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    /// /proc/pid/exe, the executable's path.
    ExeLink,
    /// argv[0], which a process may have rewritten.
    Argv0,
}

/// The base name of `path` when it extends the cut-short `comm`.
fn extend_comm(comm: &str, path: &str, source: Source) -> Option<String> {
    let path = path.strip_suffix(" (deleted)").unwrap_or(path);
    let base = path.rsplit('/').next().unwrap_or(path);
    if base.len() <= comm.len() || !base.starts_with(comm) {
        return None;
    }
    // A process that rewrote its argv may have run its arguments into argv[0].
    let end = match source {
        Source::ExeLink => base.len(),
        Source::Argv0 => base[comm.len()..]
            .find(' ')
            .map_or(base.len(), |i| comm.len() + i),
    };
    Some(base[..end].to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIREFOX_STAT: &[u8] = b"4242 (Isolated (Web) Co) S 4200 4200 4200 0 -1 4194560 \
        100 0 0 0 250 50 7 3 20 0 30 0 123456 1000 200 18446744073709551615";

    #[test]
    fn stat_fields_are_read_past_a_name_with_spaces_and_parentheses() {
        let stat = Stat::parse(FIREFOX_STAT).unwrap();
        assert_eq!(
            stat,
            Stat {
                pid: 4242,
                comm: "Isolated (Web) Co".to_owned(),
                ppid: 4200,
                pgid: 4200,
                kernel_thread: false,
                cpu_ticks: 300,
                child_cpu_ticks: 10,
                start_ticks: 123_456,
            }
        );
    }

    #[test]
    fn stat_times_become_nanoseconds() {
        let proc = Stat::parse(FIREFOX_STAT).unwrap().to_proc(10_000_000);
        assert_eq!(proc.cpu_ns, 3_000_000_000);
        assert_eq!(proc.child_cpu_ns, 100_000_000);
        assert_eq!(proc.start_ns, 1_234_560_000_000);
    }

    #[test]
    fn kernel_threads_are_marked() {
        let stat =
            Stat::parse(b"2 (kthreadd) S 0 0 0 0 -1 2129984 0 0 0 0 0 0 0 0 20 0 1 0 1").unwrap();
        assert!(stat.kernel_thread);
    }

    #[test]
    fn a_stat_line_without_a_name_is_rejected() {
        assert_eq!(Stat::parse(b"12 S 1 1"), None);
        assert_eq!(Stat::parse(b"12 )x( S 1 1"), None);
    }

    #[test]
    fn private_memory_is_resident_less_shared() {
        assert_eq!(private_resident_pages(b"5000 1200 200 10 0 900 0\n"), 1000);
        assert_eq!(private_resident_pages(b"5000 100 200 10 0 900 0\n"), 0);
        assert_eq!(private_resident_pages(b""), 0);
    }

    #[test]
    fn a_cut_short_name_is_completed_from_the_executable() {
        let comm = "rust-analyzer-p";
        assert_eq!(
            extend_comm(
                comm,
                "/usr/lib/rust-analyzer-proc-macro-srv",
                Source::ExeLink
            ),
            Some("rust-analyzer-proc-macro-srv".to_owned())
        );
        assert_eq!(
            extend_comm(
                comm,
                "/tmp/rust-analyzer-proc-macro-srv (deleted)",
                Source::ExeLink
            ),
            Some("rust-analyzer-proc-macro-srv".to_owned())
        );
    }

    #[test]
    fn an_argv0_completion_stops_at_its_arguments() {
        assert_eq!(
            extend_comm(
                "Web Content Wor",
                "Web Content Worker --type=x",
                Source::Argv0
            ),
            Some("Web Content Worker".to_owned())
        );
    }

    #[test]
    fn a_name_that_does_not_extend_comm_is_not_used() {
        assert_eq!(
            extend_comm("python3.12-thin", "/usr/bin/python3", Source::ExeLink),
            None
        );
        assert_eq!(
            extend_comm("abcdefghijklmno", "/bin/abcdefghijklmno", Source::ExeLink),
            None
        );
    }
}
