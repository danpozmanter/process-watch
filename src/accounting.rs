use std::collections::{BTreeMap, HashMap, HashSet};

use crate::lineage::{Credit, Lineage, ShortLived};
use crate::snapshot::{Proc, Snapshot};

/// What a program, or the processes of one name within it, used over one sampling interval.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Tally {
    pub cpu_ns: i64,
    pub mem_bytes: i64,
    pub procs: i64,
}

/// What one program used over one sampling interval, split by process name.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProgramTally {
    pub total: Tally,
    /// Separately started copies of the program alive at the sample.
    pub runs: i64,
    /// The total by process name; the parts add up to it.
    pub parts: BTreeMap<String, Tally>,
}

impl ProgramTally {
    fn add_process(&mut self, process: &str, cpu_ns: i64, mem_bytes: i64) {
        let part = self.parts.entry(process.to_owned()).or_default();
        for tally in [&mut self.total, part] {
            tally.cpu_ns += cpu_ns;
            tally.mem_bytes += mem_bytes;
            tally.procs += 1;
        }
    }

    fn add_cpu(&mut self, process: &str, cpu_ns: i64) {
        self.total.cpu_ns += cpu_ns;
        self.parts.entry(process.to_owned()).or_default().cpu_ns += cpu_ns;
    }
}

/// What a sample remembers about a process for the next one.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Seen {
    ppid: i64,
    cpu_ns: i64,
    child_cpu_ns: i64,
    credit: Credit,
    /// CPU the process and the children it reaped used over its latest interval.
    recent_ns: i64,
}

impl Seen {
    fn of(proc: &Proc, credit: Credit, recent_ns: i64) -> Self {
        Self {
            ppid: proc.ppid,
            cpu_ns: proc.cpu_ns,
            child_cpu_ns: proc.child_cpu_ns,
            credit,
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

/// Per reaper's snapshot index, the exited processes it reaped by credit,
/// each weighted by the CPU they used over their last sampled interval.
type ReapedPrograms<'a> = HashMap<usize, BTreeMap<&'a Credit, i64>>;

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
    lineage: Lineage,
}

impl Ledger {
    /// Per-program usage since the previous snapshot, split by process name;
    /// `roots[i]` is the index of the process heading process `i`'s program.
    ///
    /// A process seen before contributes the CPU it gained since then; one
    /// started after the previous snapshot contributes all of its CPU.
    /// A process that has exited reaches its nearest living ancestor's
    /// reaped-children counter; that counter's gain, less what was counted
    /// while the exited processes lived, is the work they did after the last
    /// sample plus the work of children too short-lived to be sampled. It goes
    /// to the programs and process names the exited processes ran under,
    /// split by the CPU each used over its last sampled interval, the best
    /// estimate of the unobserved stretch; with no exited process, or none
    /// that was busy across programs, it stays with the ancestor. Where the OS reports forks and
    /// exits, an orphan no snapshot listed joins that split under the program
    /// that started it, rather than leaving its CPU with the subreaper or init
    /// that reaped it.
    pub fn tally(&mut self, snap: &Snapshot, roots: &[usize]) -> BTreeMap<String, ProgramTally> {
        let census = Census::new(snap);
        let mut gains = self.gains(snap);
        let short_lived = self.follow_events(snap);
        let mut reaped_programs = self.match_exits(&census, &mut gains.reaped);
        credit_short_lived(&mut reaped_programs, &short_lived, &census);
        let mut usage: BTreeMap<String, ProgramTally> = BTreeMap::new();
        let mut seen = HashMap::with_capacity(snap.procs.len());
        for (i, proc) in snap.procs.iter().enumerate() {
            let credit = Credit {
                program: snap.procs[roots[i]].name.clone(),
                process: proc.name.clone(),
            };
            let own = gains.own[i].max(0);
            let tail = gains.reaped[i].max(0);
            let program = usage.entry(credit.program.clone()).or_default();
            program.add_process(&credit.process, own, proc.mem_bytes);
            if roots[i] == i {
                program.runs += 1;
            }
            for (to, cpu_ns) in split_tail(tail, reaped_programs.get(&i), &credit) {
                usage
                    .entry(to.program.clone())
                    .or_default()
                    .add_cpu(&to.process, cpu_ns);
            }
            seen.insert(key_of(proc), Seen::of(proc, credit, own + tail));
        }
        self.seen = seen;
        self.previous_now_ns = snap.now_ns;
        self.lineage
            .forget_listed(|pid| census.index_of(pid).is_some());
        usage
    }

    /// The processes the snapshot's events report starting and exiting
    /// unlisted since the previous snapshot.
    fn follow_events(&mut self, snap: &Snapshot) -> Vec<ShortLived> {
        let Some(events) = &snap.events else {
            self.lineage.reset();
            return Vec::new();
        };
        let listed: HashMap<i64, &Credit> = self
            .seen
            .iter()
            .map(|(&(pid, _), seen)| (pid, &seen.credit))
            .collect();
        self.lineage.follow(events, &listed)
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
                .entry(&gone.credit)
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

/// Adds what started and exited unlisted to the credits a reaper's tail is
/// split among, for each reaper that took in an orphan: an orphan's CPU
/// reaches a subreaper or init, and only the fork events name the program
/// and process that started it. Each one's lifetime stands in for the CPU it used.
fn credit_short_lived<'a>(
    programs: &mut ReapedPrograms<'a>,
    short_lived: &'a [ShortLived],
    census: &Census,
) {
    let adopters: HashSet<i64> = short_lived
        .iter()
        .filter(|gone| gone.orphaned)
        .map(|gone| gone.reaper)
        .collect();
    for gone in short_lived.iter().filter(|g| adopters.contains(&g.reaper)) {
        let Some(reaper) = census.index_of(gone.reaper) else {
            continue;
        };
        *programs
            .entry(reaper)
            .or_default()
            .entry(&gone.credit)
            .or_default() += gone.lifetime_ns.max(0);
    }
}

/// A reaper's tail divided among the exited processes' credits in proportion
/// to their recent CPU. When none was busy, a single program's processes
/// share it evenly; across programs, it stays with the reaper.
fn split_tail<'a>(
    tail: i64,
    weights: Option<&BTreeMap<&'a Credit, i64>>,
    reaper: &'a Credit,
) -> Vec<(&'a Credit, i64)> {
    let Some(by_credit) = weights else {
        return vec![(reaper, tail)];
    };
    if by_credit.values().any(|&weight| weight > 0) {
        return shares(
            tail,
            by_credit.iter().map(|(&credit, &weight)| (credit, weight)),
        );
    }
    let mut programs = by_credit.keys().map(|credit| &credit.program);
    match programs.next() {
        Some(first) if programs.all(|program| program == first) => {
            shares(tail, by_credit.keys().map(|&credit| (credit, 1)))
        }
        _ => vec![(reaper, tail)],
    }
}

/// `tail` divided in proportion to the weights, at least one of them
/// positive; rounding leaves a few nanoseconds, which go to the heaviest.
fn shares<'a>(
    tail: i64,
    weights: impl Iterator<Item = (&'a Credit, i64)>,
) -> Vec<(&'a Credit, i64)> {
    let weights: Vec<(&Credit, i64)> = weights.collect();
    let total: i64 = weights.iter().map(|&(_, weight)| weight).sum();
    let mut shares: Vec<(&Credit, i64)> = weights
        .iter()
        .map(|&(credit, weight)| {
            let share = i128::from(tail) * i128::from(weight) / i128::from(total);
            (credit, i64::try_from(share).unwrap_or(0))
        })
        .collect();
    let given: i64 = shares.iter().map(|s| s.1).sum();
    let heaviest = weights
        .iter()
        .enumerate()
        .max_by_key(|&(_, &(_, weight))| weight)
        .map_or(0, |(i, _)| i);
    shares[heaviest].1 += tail - given;
    shares
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::{Proc, ProcEvent};

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
        seen_as(ppid, cpu_ns, child_cpu_ns, (program, program), recent_ns)
    }

    fn seen_as(
        ppid: i64,
        cpu_ns: i64,
        child_cpu_ns: i64,
        (program, process): (&str, &str),
        recent_ns: i64,
    ) -> Seen {
        Seen {
            ppid,
            cpu_ns,
            child_cpu_ns,
            credit: Credit {
                program: program.to_owned(),
                process: process.to_owned(),
            },
            recent_ns,
        }
    }

    fn ledger(prev: &[((i64, i64), Seen)]) -> Ledger {
        Ledger {
            seen: prev.iter().cloned().collect(),
            previous_now_ns: 1000,
            lineage: Lineage::default(),
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
            events: None,
        }
    }

    fn fork(parent: i64, child: i64, at_ns: i64) -> ProcEvent {
        ProcEvent::Fork {
            parent,
            child,
            at_ns,
        }
    }

    fn exit(pid: i64, parent: i64, at_ns: i64) -> ProcEvent {
        ProcEvent::Exit { pid, parent, at_ns }
    }

    /// A build under a subreaper, whose short-lived child 50 started 51 and
    /// exited first, leaving 51 to the subreaper; 51 used 500 ns and 50 7 ns.
    fn orphaned_by_a_build(events: Option<Vec<ProcEvent>>) -> BTreeMap<String, ProgramTally> {
        let mut ledger = ledger(&[
            ((100, 1), seen(0, 50, 0, "systemd", 0)),
            ((1, 5), seen(100, 100, 0, "build", 0)),
        ]);
        let mut snap = named(
            vec![p(100, 0, 1, 50, 500), p(1, 100, 5, 100, 7)],
            &["systemd", "build"],
        );
        snap.events = events;
        ledger.tally(&snap, &[0, 1])
    }

    #[test]
    fn an_orphan_no_snapshot_listed_counts_toward_the_program_that_started_it() {
        let usage = orphaned_by_a_build(Some(vec![
            fork(1, 50, 1100),
            fork(50, 51, 1200),
            exit(50, 1, 1300),
            exit(51, 100, 1500),
        ]));
        assert_eq!(usage["build"].total.cpu_ns, 507);
        assert_eq!(usage["systemd"].total.cpu_ns, 0);
    }

    #[test]
    fn without_events_an_unlisted_orphan_counts_toward_its_reaper() {
        let usage = orphaned_by_a_build(None);
        assert_eq!(usage["build"].total.cpu_ns, 7);
        assert_eq!(usage["systemd"].total.cpu_ns, 500);
    }

    #[test]
    fn orphans_of_two_programs_split_the_reapers_tail_by_lifetime() {
        let mut ledger = ledger(&[
            ((100, 1), seen(0, 50, 0, "systemd", 0)),
            ((1, 5), seen(100, 0, 0, "a", 0)),
            ((2, 6), seen(100, 0, 0, "b", 0)),
        ]);
        let mut snap = named(
            vec![
                p(100, 0, 1, 50, 400),
                p(1, 100, 5, 0, 0),
                p(2, 100, 6, 0, 0),
            ],
            &["systemd", "a", "b"],
        );
        snap.events = Some(vec![
            fork(1, 50, 1000),
            fork(2, 60, 1000),
            exit(50, 100, 1300),
            exit(60, 100, 1100),
        ]);
        let usage = ledger.tally(&snap, &[0, 1, 2]);
        assert_eq!(usage["a"].total.cpu_ns, 300);
        assert_eq!(usage["b"].total.cpu_ns, 100);
        assert_eq!(usage["systemd"].total.cpu_ns, 0);
    }

    #[test]
    fn running_processes_contribute_their_cpu_gain() {
        let mut ledger = ledger(&[((1, 5), seen(0, 100, 0, "a", 0))]);
        let usage = ledger.tally(&named(vec![p(1, 0, 5, 350, 0)], &["a"]), &[0]);
        assert_eq!(
            usage["a"].total,
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
        assert_eq!(usage["new"].total.cpu_ns, 40);
        assert_eq!(usage["old"].total.cpu_ns, 0);
    }

    #[test]
    fn a_process_started_within_one_clock_tick_of_the_last_sample_is_new() {
        let mut ledger = ledger(&[]);
        // Start times come truncated to a 10 ns tick here; this process
        // started after the last sample read the clock at 1000.
        let mut snap = named(vec![p(4, 0, 995, 70, 0)], &["quick"]);
        snap.resolution_ns = 10;
        let usage = ledger.tally(&snap, &[0]);
        assert_eq!(usage["quick"].total.cpu_ns, 70);
    }

    #[test]
    fn the_tail_of_an_exited_program_stays_with_that_program() {
        let mut ledger = ledger(&[
            ((1, 5), seen(0, 100, 0, "shell", 0)),
            ((2, 6), seen(1, 300, 900, "build", 0)),
        ]);
        // The build ran 50 ns more and reaped 200 ns of compilers after the last sample.
        let usage = ledger.tally(&named(vec![p(1, 0, 5, 100, 1450)], &["shell"]), &[0]);
        assert_eq!(usage["build"].total.cpu_ns, 250);
        assert_eq!(usage["build"].total.procs, 0);
        assert_eq!(usage["shell"].total.cpu_ns, 0);
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
        assert_eq!(usage["make"].total.cpu_ns, 90);
        assert_eq!(usage["shell"].total.cpu_ns, 0);
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
        assert_eq!(usage["orphan"].total.cpu_ns, 30);
        assert_eq!(usage["bash"].total.cpu_ns, 3);
        assert_eq!(usage["systemd"].total.cpu_ns, 0);
        assert_eq!(usage["claude"].total.cpu_ns, 0);
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
        assert_eq!(usage["make"].total.cpu_ns, 0);
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
        assert_eq!(usage["x"].total.cpu_ns, 34);
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
        assert_eq!(usage["build"].total.cpu_ns, 120);
        assert!(!ledger.seen.contains_key(&(2, 6)));
    }

    fn part(usage: &BTreeMap<String, ProgramTally>, program: &str, process: &str) -> Tally {
        usage[program].parts[process]
    }

    #[test]
    fn a_program_splits_into_its_processes_by_name_and_counts_its_runs() {
        let mut ledger = ledger(&[]);
        let snap = named(
            vec![
                p(1, 0, 1500, 10, 0),
                p(2, 1, 1500, 200, 0),
                p(3, 1, 1500, 300, 0),
                p(4, 0, 1500, 20, 0),
                p(5, 4, 1500, 400, 0),
            ],
            &["cargo", "rustc", "rustc", "cargo", "rustc"],
        );
        let usage = ledger.tally(&snap, &[0, 0, 0, 3, 3]);
        let cargo = &usage["cargo"];
        assert_eq!(cargo.runs, 2);
        assert_eq!(
            cargo.total,
            Tally {
                cpu_ns: 930,
                mem_bytes: 5000,
                procs: 5
            }
        );
        assert_eq!(
            part(&usage, "cargo", "rustc"),
            Tally {
                cpu_ns: 900,
                mem_bytes: 3000,
                procs: 3
            }
        );
        assert_eq!(
            part(&usage, "cargo", "cargo"),
            Tally {
                cpu_ns: 30,
                mem_bytes: 2000,
                procs: 2
            }
        );
    }

    #[test]
    fn the_tail_of_an_exited_process_goes_to_its_own_name() {
        let mut ledger = ledger(&[
            ((1, 5), seen(0, 100, 0, "make", 0)),
            ((2, 6), seen_as(1, 300, 0, ("make", "cc1"), 30)),
            ((3, 7), seen_as(1, 300, 0, ("make", "as"), 10)),
        ]);
        // cc1 and as ran 80 ns between them after the last sample.
        let usage = ledger.tally(&named(vec![p(1, 0, 5, 100, 680)], &["make"]), &[0]);
        assert_eq!(usage["make"].total.cpu_ns, 80);
        assert_eq!(part(&usage, "make", "cc1").cpu_ns, 60);
        assert_eq!(part(&usage, "make", "as").cpu_ns, 20);
        assert_eq!(part(&usage, "make", "make").cpu_ns, 0);
    }

    #[test]
    fn an_idle_tail_within_one_program_is_shared_by_its_processes() {
        let mut ledger = ledger(&[
            ((1, 5), seen(0, 100, 0, "shell", 0)),
            ((2, 6), seen_as(1, 300, 0, ("build", "make"), 0)),
            ((3, 7), seen_as(2, 300, 0, ("build", "cc1"), 0)),
        ]);
        let usage = ledger.tally(&named(vec![p(1, 0, 5, 100, 611)], &["shell"]), &[0]);
        assert_eq!(usage["build"].total.cpu_ns, 11);
        assert_eq!(part(&usage, "build", "cc1").cpu_ns, 5);
        assert_eq!(part(&usage, "build", "make").cpu_ns, 6);
        assert_eq!(usage["shell"].total.cpu_ns, 0);
    }

    #[test]
    fn an_unlisted_orphan_counts_toward_the_process_that_started_it() {
        let mut ledger = ledger(&[
            ((100, 1), seen(0, 50, 0, "systemd", 0)),
            ((1, 5), seen_as(100, 100, 0, ("build", "sh"), 0)),
        ]);
        let mut snap = named(
            vec![p(100, 0, 1, 50, 500), p(1, 100, 5, 100, 0)],
            &["systemd", "sh"],
        );
        snap.events = Some(vec![fork(1, 50, 1100), exit(50, 100, 1500)]);
        let usage = ledger.tally(&snap, &[0, 1]);
        assert_eq!(part(&usage, "build", "sh").cpu_ns, 500);
        assert_eq!(usage["systemd"].total.cpu_ns, 0);
    }
}
