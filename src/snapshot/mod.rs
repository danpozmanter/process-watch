#[cfg(target_os = "freebsd")]
mod freebsd;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
mod windows;

#[cfg(target_os = "freebsd")]
pub use freebsd::{Lister, cpu_count};
#[cfg(target_os = "linux")]
pub use linux::{Lister, cpu_count};
#[cfg(target_os = "macos")]
pub use macos::{Lister, cpu_count};
#[cfg(windows)]
pub use windows::{Lister, cpu_count};

#[cfg(not(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "freebsd",
    windows
)))]
compile_error!("process-watch supports Linux, macOS, Windows, and FreeBSD");

/// One process as a snapshot saw it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Proc {
    pub pid: i64,
    pub ppid: i64,
    /// Process group; 0 on Windows, which has none.
    pub pgid: i64,
    /// Creation time in nanoseconds on the clock `Snapshot::now_ns` reads,
    /// so a parent started after its child holds a reused pid.
    pub start_ns: i64,
    pub cpu_ns: i64,
    /// CPU of descendants this process has reaped; 0 where the OS keeps no counter.
    pub child_cpu_ns: i64,
    /// Memory the process alone holds resident: private anonymous pages on
    /// Linux, the physical footprint on macOS, the private working set on
    /// Windows, and the resident set on FreeBSD.
    pub mem_bytes: i64,
    pub name: String,
}

/// Every process on the host, with the clock reading its start times compare against.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Snapshot {
    pub procs: Vec<Proc>,
    pub now_ns: i64,
    /// Granularity the OS reports start times and CPU counters in, each
    /// rounded down to it: a clock tick on Linux, finer elsewhere.
    pub resolution_ns: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn own_entry(snap: &Snapshot) -> &Proc {
        let pid = i64::from(std::process::id());
        let found = snap.procs.iter().find(|p| p.pid == pid);
        found.unwrap_or_else(|| panic!("pid {pid} is missing from the snapshot"))
    }

    #[test]
    fn a_snapshot_lists_this_process_under_its_living_parent() {
        let snap = Lister::new().take().unwrap();
        let me = own_entry(&snap);
        assert!(me.name.starts_with("pw"), "named {:?}", me.name);
        assert!(me.start_ns <= snap.now_ns);
        assert!(me.mem_bytes > 0);
        assert!(snap.resolution_ns > 0);
        let parent = snap.procs.iter().find(|p| p.pid == me.ppid);
        assert!(parent.is_some_and(|p| p.start_ns <= me.start_ns));
        #[cfg(unix)]
        assert_eq!(me.ppid, i64::from(std::os::unix::process::parent_id()));
    }

    #[test]
    fn cpu_counters_never_run_backwards() {
        let mut lister = Lister::new();
        let first = lister.take().unwrap();
        let second = lister.take().unwrap();
        assert!(second.now_ns >= first.now_ns);
        assert!(own_entry(&second).cpu_ns >= own_entry(&first).cpu_ns);
    }

    #[test]
    fn there_is_at_least_one_cpu() {
        assert!(cpu_count() >= 1);
    }
}
