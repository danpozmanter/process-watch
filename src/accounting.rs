use std::collections::{BTreeMap, HashMap, HashSet};

use crate::snapshot::{Proc, Snapshot};

/// What one program used over one sampling interval.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Tally {
    pub cpu_ns: i64,
    pub mem_bytes: i64,
    pub procs: i64,
}

/// What a sample remembers about a process for the next one.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Seen {
    ppid: i64,
    cpu_ns: i64,
    child_cpu_ns: i64,
    program: String,
    /// CPU the process and the children it reaped used over its latest interval.
    recent_ns: i64,
}

impl Seen {
    fn of(proc: &Proc, program: &str, recent_ns: i64) -> Self {
        Self {
            ppid: proc.ppid,
            cpu_ns: proc.cpu_ns,
            child_cpu_ns: proc.child_cpu_ns,
            program: program.to_owned(),
            recent_ns,
        }
    }

    /// CPU already counted of the process and the children it reaped.
    fn counted_ns(&self) -> i64 {
        self.cpu_ns + self.child_cpu_ns
    }
}

/// Processes are keyed by pid and start time, so a reused pid is a new process.
type ProcKey = (i64, i64);

fn key_of(proc: &Proc) -> ProcKey {
    (proc.pid, proc.start_ns)
}

/// What each process in a snapshot gained since the previous one, by snapshot index.
struct Gains {
    /// CPU the process itself used.
    own: Vec<i64>,
    /// Growth of its reaped-children counter not yet matched to an exited process.
    reaped: Vec<i64>,
}

/// Per reaper's snapshot index, the exited programs it reaped, each weighted
/// by the CPU its processes used over their last sampled interval.
type ReapedPrograms<'a> = HashMap<usize, BTreeMap<&'a str, i64>>;

/// A snapshot's processes indexed by pid.
struct Census<'a> {
    procs: &'a [Proc],
    by_pid: HashMap<i64, usize>,
    resolution_ns: i64,
}

impl<'a> Census<'a> {
    fn new(snap: &'a Snapshot) -> Self {
        Self {
            procs: &snap.procs,
            by_pid: snap
                .procs
                .iter()
                .enumerate()
                .map(|(i, p)| (p.pid, i))
                .collect(),
            resolution_ns: snap.resolution_ns,
        }
    }

    fn index_of(&self, pid: i64) -> Option<usize> {
        self.by_pid.get(&pid).copied()
    }

    /// A living process and its living ancestors, nearest first, stopping at a
    /// parent that started after its child and so holds a reused pid.
    fn lineage(&self, first: usize) -> impl Iterator<Item = usize> + '_ {
        let mut next = Some(first);
        let mut hops = 0;
        std::iter::from_fn(move || {
            let current = next?;
            hops += 1;
            let child = &self.procs[current];
            next = self.index_of(child.ppid).filter(|&parent| {
                parent != current
                    && self.procs[parent].start_ns <= child.start_ns
                    && hops <= self.procs.len()
            });
            Some(current)
        })
    }
}

/// Per-process counters carried from one sample to the next.
#[derive(Debug, Default)]
pub struct Ledger {
    seen: HashMap<ProcKey, Seen>,
    previous_now_ns: i64,
}

impl Ledger {
    /// Per-program usage since the previous snapshot; `roots[i]` is the index
    /// of the process heading process `i`'s program.
    ///
    /// A process seen before contributes the CPU it gained since then; one
    /// started after the previous snapshot contributes all of its CPU.
    /// A process that has exited reaches its nearest living ancestor's
    /// reaped-children counter; that counter's gain, less what was counted
    /// while the exited processes lived, is the work they did after the last
    /// sample plus the work of children too short-lived to be sampled. It goes
    /// to the programs the exited processes ran, split by the CPU each used
    /// over its last sampled interval, the best estimate of the unobserved
    /// stretch; with no exited process, or none that was busy across
    /// programs, it stays with the ancestor's.
    pub fn tally(&mut self, snap: &Snapshot, roots: &[usize]) -> BTreeMap<String, Tally> {
        let census = Census::new(snap);
        let mut gains = self.gains(snap);
        let reaped_programs = self.match_exits(&census, &mut gains.reaped);
        let mut usage: BTreeMap<String, Tally> = BTreeMap::new();
        let mut seen = HashMap::with_capacity(snap.procs.len());
        for (i, proc) in snap.procs.iter().enumerate() {
            let program = &snap.procs[roots[i]].name;
            let own = gains.own[i].max(0);
            let tail = gains.reaped[i].max(0);
            let tally = usage.entry(program.clone()).or_default();
            tally.cpu_ns += own;
            tally.mem_bytes += proc.mem_bytes;
            tally.procs += 1;
            for (to, cpu_ns) in split_tail(tail, reaped_programs.get(&i), program) {
                usage.entry(to.to_owned()).or_default().cpu_ns += cpu_ns;
            }
            seen.insert(key_of(proc), Seen::of(proc, program, own + tail));
        }
        self.seen = seen;
        self.previous_now_ns = snap.now_ns;
        usage
    }

    fn gains(&self, snap: &Snapshot) -> Gains {
        let (own, reaped) = snap
            .procs
            .iter()
            .map(|proc| self.gain_of(proc, snap.resolution_ns))
            .unzip();
        Gains { own, reaped }
    }

    /// The CPU the process used, and the growth of its reaped-children
    /// counter, since the previous snapshot. A process not seen then counts
    /// in full when it started after it; start times are rounded down to the
    /// OS's resolution, so one within that resolution counts as started after.
    fn gain_of(&self, proc: &Proc, resolution_ns: i64) -> (i64, i64) {
        match self.seen.get(&key_of(proc)) {
            Some(before) => (
                proc.cpu_ns - before.cpu_ns,
                proc.child_cpu_ns - before.child_cpu_ns,
            ),
            None if proc.start_ns + resolution_ns > self.previous_now_ns => {
                (proc.cpu_ns, proc.child_cpu_ns)
            }
            None => (0, 0),
        }
    }

    /// Processes seen at the previous snapshot and gone from this one, as pid to start time.
    fn exited(&self, census: &Census) -> HashMap<i64, i64> {
        let alive: HashSet<ProcKey> = census.procs.iter().map(key_of).collect();
        self.seen
            .keys()
            .filter(|key| !alive.contains(key))
            .copied()
            .collect()
    }

    /// Matches each exited process to the living process that reaped it,
    /// takes what was already counted of it out of that reaper's counter
    /// growth, and gathers the programs each reaper reaped.
    fn match_exits(&self, census: &Census, reaped: &mut [i64]) -> ReapedPrograms<'_> {
        let exited = self.exited(census);
        // The largest amounts are matched first, so a small one cannot take
        // counter growth that only a large one explains.
        let mut order: Vec<(i64, ProcKey)> = exited
            .iter()
            .map(|(&pid, &start_ns)| (self.seen[&(pid, start_ns)].counted_ns(), (pid, start_ns)))
            .collect();
        order.sort_unstable_by(|a, b| b.cmp(a));
        let mut programs: ReapedPrograms = HashMap::new();
        for (counted, (pid, start_ns)) in order {
            let gone = &self.seen[&(pid, start_ns)];
            let Some(reaper) = self.reaper_of(gone, start_ns, census, &exited, reaped) else {
                continue;
            };
            reaped[reaper] -= counted;
            *programs
                .entry(reaper)
                .or_default()
                .entry(gone.program.as_str())
                .or_default() += gone.recent_ns;
        }
        programs
    }

    /// The living process whose reaped-children counter took in `gone`.
    ///
    /// The reaper's counter grew by at least what was already counted of the
    /// process, less the rounding of its two counters. An orphan whose parent
    /// exited in the same interval was reaped further up, by a subreaper or
    /// init; when no counter grew that much, the OS kept none and nothing was
    /// counted twice.
    fn reaper_of(
        &self,
        gone: &Seen,
        start_ns: i64,
        census: &Census,
        exited: &HashMap<i64, i64>,
        reaped: &[i64],
    ) -> Option<usize> {
        let nearest = self.living_ancestor(gone.ppid, start_ns, exited, census)?;
        let rounding = 2 * census.resolution_ns;
        let counted = gone.counted_ns();
        census
            .lineage(nearest)
            .find(|&ancestor| reaped[ancestor] + rounding >= counted)
    }

    /// The snapshot index of the nearest living ancestor of an exited process,
    /// climbing through ancestors that exited in the same interval. A parent
    /// that started after its child holds a reused pid and ends the climb.
    fn living_ancestor(
        &self,
        ppid: i64,
        child_start_ns: i64,
        exited: &HashMap<i64, i64>,
        census: &Census,
    ) -> Option<usize> {
        let mut parent_pid = ppid;
        let mut start_ns = child_start_ns;
        for _ in 0..=exited.len() {
            if let Some(parent) = census.index_of(parent_pid) {
                return (census.procs[parent].start_ns <= start_ns).then_some(parent);
            }
            let &parent_start_ns = exited.get(&parent_pid)?;
            if parent_start_ns > start_ns {
                return None;
            }
            parent_pid = self.seen[&(parent_pid, parent_start_ns)].ppid;
            start_ns = parent_start_ns;
        }
        None
    }
}

/// A reaper's tail divided among exited programs in proportion to their recent CPU.
fn split_tail<'a>(
    tail: i64,
    weights: Option<&BTreeMap<&'a str, i64>>,
    reaper_program: &'a str,
) -> Vec<(&'a str, i64)> {
    let Some(by_program) = weights else {
        return vec![(reaper_program, tail)];
    };
    let total: i64 = by_program.values().sum();
    if total == 0 {
        return match by_program.keys().next() {
            Some(&only) if by_program.len() == 1 => vec![(only, tail)],
            _ => vec![(reaper_program, tail)],
        };
    }
    let mut shares: Vec<(&str, i64)> = by_program
        .iter()
        .map(|(&program, &weight)| {
            let share = i128::from(tail) * i128::from(weight) / i128::from(total);
            (program, i64::try_from(share).unwrap_or(0))
        })
        .collect();
    // Rounding leaves a few nanoseconds; they go to the busiest program.
    let given: i64 = shares.iter().map(|s| s.1).sum();
    let busiest = by_program
        .values()
        .enumerate()
        .max_by_key(|&(_, weight)| *weight)
        .map_or(0, |(i, _)| i);
    shares[busiest].1 += tail - given;
    shares
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::Proc;

    fn p(pid: i64, ppid: i64, start_ns: i64, cpu_ns: i64, child_cpu_ns: i64) -> Proc {
        Proc {
            pid,
            ppid,
            start_ns,
            cpu_ns,
            child_cpu_ns,
            mem_bytes: 1000,
            name: "x".to_owned(),
            ..Proc::default()
        }
    }

    fn seen(ppid: i64, cpu_ns: i64, child_cpu_ns: i64, program: &str, recent_ns: i64) -> Seen {
        Seen {
            ppid,
            cpu_ns,
            child_cpu_ns,
            program: program.to_owned(),
            recent_ns,
        }
    }

    fn ledger(prev: &[((i64, i64), Seen)]) -> Ledger {
        Ledger {
            seen: prev.iter().cloned().collect(),
            previous_now_ns: 1000,
        }
    }

    fn named(mut procs: Vec<Proc>, names: &[&str]) -> Snapshot {
        for (proc, name) in procs.iter_mut().zip(names) {
            proc.name = (*name).to_owned();
        }
        Snapshot {
            procs,
            now_ns: 2000,
            resolution_ns: 1,
        }
    }

    #[test]
    fn running_processes_contribute_their_cpu_gain() {
        let mut ledger = ledger(&[((1, 5), seen(0, 100, 0, "a", 0))]);
        let usage = ledger.tally(&named(vec![p(1, 0, 5, 350, 0)], &["a"]), &[0]);
        assert_eq!(
            usage["a"],
            Tally {
                cpu_ns: 250,
                mem_bytes: 1000,
                procs: 1
            }
        );
    }

    #[test]
    fn a_new_process_contributes_all_its_cpu_but_one_missed_earlier_none() {
        let mut ledger = ledger(&[]);
        let snap = named(
            vec![p(2, 0, 1500, 40, 0), p(3, 0, 500, 900, 0)],
            &["new", "old"],
        );
        let usage = ledger.tally(&snap, &[0, 1]);
        assert_eq!(usage["new"].cpu_ns, 40);
        assert_eq!(usage["old"].cpu_ns, 0);
    }

    #[test]
    fn a_process_started_within_one_clock_tick_of_the_last_sample_is_new() {
        let mut ledger = ledger(&[]);
        // Start times come truncated to a 10 ns tick here; this process
        // started after the last sample read the clock at 1000.
        let mut snap = named(vec![p(4, 0, 995, 70, 0)], &["quick"]);
        snap.resolution_ns = 10;
        let usage = ledger.tally(&snap, &[0]);
        assert_eq!(usage["quick"].cpu_ns, 70);
    }

    #[test]
    fn the_tail_of_an_exited_program_stays_with_that_program() {
        let mut ledger = ledger(&[
            ((1, 5), seen(0, 100, 0, "shell", 0)),
            ((2, 6), seen(1, 300, 900, "build", 0)),
        ]);
        // The build ran 50 ns more and reaped 200 ns of compilers after the last sample.
        let usage = ledger.tally(&named(vec![p(1, 0, 5, 100, 1450)], &["shell"]), &[0]);
        assert_eq!(usage["build"].cpu_ns, 250);
        assert_eq!(usage["build"].procs, 0);
        assert_eq!(usage["shell"].cpu_ns, 0);
    }

    #[test]
    fn a_tail_shared_by_exited_programs_follows_their_recent_cpu() {
        let mut ledger = ledger(&[
            ((1, 5), seen(0, 100, 0, "shell", 0)),
            ((2, 6), seen(1, 1, 0, "sudo", 0)),
            ((3, 7), seen(2, 300, 0, "make", 300)),
        ]);
        // make and the sudo wrapping it exited together, 90 ns past the last sample.
        let usage = ledger.tally(&named(vec![p(1, 0, 5, 100, 391)], &["shell"]), &[0]);
        assert_eq!(usage["make"].cpu_ns, 90);
        assert_eq!(usage["shell"].cpu_ns, 0);
    }

    #[test]
    fn an_orphan_reaped_by_a_subreaper_is_not_counted_twice() {
        let mut ledger = ledger(&[
            ((100, 1), seen(0, 50, 0, "systemd", 0)),
            ((1, 5), seen(100, 100, 0, "claude", 0)),
            ((2, 6), seen(1, 7, 0, "bash", 0)),
            ((3, 7), seen(2, 5000, 0, "orphan", 10)),
        ]);
        // bash exited and claude reaped it, 3 ns past the last sample; its
        // child, re-parented to systemd, exited 30 ns later in the same interval.
        let snap = named(
            vec![p(100, 0, 1, 50, 5030), p(1, 100, 5, 100, 10)],
            &["systemd", "claude"],
        );
        let usage = ledger.tally(&snap, &[0, 1]);
        assert_eq!(usage["orphan"].cpu_ns, 30);
        assert_eq!(usage["bash"].cpu_ns, 3);
        assert_eq!(usage["systemd"].cpu_ns, 0);
        assert_eq!(usage["claude"].cpu_ns, 0);
    }

    #[test]
    fn a_child_whose_counters_round_down_is_not_counted_twice() {
        let mut ledger = ledger(&[
            ((1, 5), seen(0, 100, 0, "make", 0)),
            ((2, 6), seen(1, 300, 0, "make", 0)),
        ]);
        // Counters come truncated to a 10 ns tick: the child had 300 counted,
        // and the parent's reaped counter grew by a truncated 290.
        let mut snap = named(vec![p(1, 0, 5, 100, 290)], &["make"]);
        snap.resolution_ns = 10;
        let usage = ledger.tally(&snap, &[0]);
        assert_eq!(usage["make"].cpu_ns, 0);
    }

    #[test]
    fn a_grandchild_reaped_with_its_parent_is_not_counted_twice() {
        let mut ledger = ledger(&[
            ((1, 5), seen(0, 100, 0, "x", 0)),
            ((2, 6), seen(1, 10, 0, "x", 0)),
            ((3, 7), seen(2, 5000, 0, "x", 0)),
        ]);
        // The grandchild ran 30 ns more and the child 4 ns more after the last
        // sample; the child reaped the grandchild, then the parent reaped the child.
        let usage = ledger.tally(&named(vec![p(1, 0, 5, 100, 5044)], &["x"]), &[0]);
        assert_eq!(usage["x"].cpu_ns, 34);
    }

    #[test]
    fn an_exited_child_adds_only_its_unsampled_tail_to_the_parent() {
        let mut ledger = ledger(&[
            ((1, 5), seen(0, 100, 0, "build", 0)),
            ((2, 6), seen(1, 300, 0, "build", 0)),
        ]);
        // The child ran 300 ns while sampled and 50 ns more before the parent
        // reaped it; another child the samples never saw ran 70 ns.
        let usage = ledger.tally(&named(vec![p(1, 0, 5, 100, 420)], &["build"]), &[0]);
        assert_eq!(usage["build"].cpu_ns, 120);
        assert!(!ledger.seen.contains_key(&(2, 6)));
    }
}
