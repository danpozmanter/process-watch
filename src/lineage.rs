use std::collections::HashMap;

use crate::snapshot::ProcEvent;

/// A process that started and exited between two snapshots, listed by neither.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShortLived {
    /// The process that reaped it.
    pub reaper: i64,
    /// The program of its nearest ancestor a snapshot listed.
    pub program: String,
    pub lifetime_ns: i64,
    /// Whether an ancestor further up reaped it, the parent that started it
    /// having exited first.
    pub orphaned: bool,
}

/// A process the events reported starting that no snapshot has listed yet.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Newcomer {
    parent: i64,
    program: Option<String>,
    started_ns: i64,
}

/// Follows forks and exits between snapshots, so a process no snapshot
/// listed is still known by the program that started it, even once an
/// orphan has been handed to a subreaper or init.
#[derive(Debug, Default)]
pub struct Lineage {
    newcomers: HashMap<i64, Newcomer>,
}

impl Lineage {
    /// The processes among `events` that started and exited unlisted, given
    /// the program of each process the previous snapshot listed, by pid.
    pub fn follow(&mut self, events: &[ProcEvent], listed: &HashMap<i64, &str>) -> Vec<ShortLived> {
        let mut gone = Vec::new();
        for event in events {
            match *event {
                ProcEvent::Fork {
                    parent,
                    child,
                    at_ns,
                } => self.started(parent, child, at_ns, listed),
                ProcEvent::Exit { pid, parent, at_ns } => {
                    gone.extend(self.ended(pid, parent, at_ns))
                }
            }
        }
        gone
    }

    fn started(&mut self, parent: i64, child: i64, at_ns: i64, listed: &HashMap<i64, &str>) {
        // A child forked while the previous snapshot was being taken may be in it.
        if listed.contains_key(&child) {
            return;
        }
        let program = match self.newcomers.get(&parent) {
            Some(newcomer) => newcomer.program.clone(),
            None => listed.get(&parent).map(|&program| program.to_owned()),
        };
        let newcomer = Newcomer {
            parent,
            program,
            started_ns: at_ns,
        };
        self.newcomers.insert(child, newcomer);
    }

    fn ended(&mut self, pid: i64, reaper: i64, at_ns: i64) -> Option<ShortLived> {
        let newcomer = self.newcomers.remove(&pid)?;
        Some(ShortLived {
            reaper,
            program: newcomer.program?,
            lifetime_ns: at_ns - newcomer.started_ns,
            orphaned: reaper != newcomer.parent,
        })
    }

    /// Forgets the newcomers a snapshot now lists; the ledger follows those.
    pub fn forget_listed(&mut self, is_listed: impl Fn(i64) -> bool) {
        self.newcomers.retain(|&pid, _| !is_listed(pid));
    }

    /// Forgets every newcomer, as when the OS lost events and the lineage has gaps.
    pub fn reset(&mut self) {
        self.newcomers.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn listed() -> HashMap<i64, &'static str> {
        HashMap::from([(1, "init"), (10, "build")])
    }

    #[test]
    fn an_orphan_keeps_the_program_of_its_listed_ancestor() {
        let mut lineage = Lineage::default();
        let events = [
            fork(10, 20, 100),
            fork(20, 21, 150),
            exit(20, 10, 200),
            exit(21, 1, 450),
        ];
        assert_eq!(
            lineage.follow(&events, &listed()),
            [
                ShortLived {
                    reaper: 10,
                    program: "build".to_owned(),
                    lifetime_ns: 100,
                    orphaned: false,
                },
                ShortLived {
                    reaper: 1,
                    program: "build".to_owned(),
                    lifetime_ns: 300,
                    orphaned: true,
                },
            ]
        );
    }

    #[test]
    fn a_newcomer_still_running_is_followed_into_the_next_interval() {
        let mut lineage = Lineage::default();
        assert!(lineage.follow(&[fork(10, 20, 100)], &listed()).is_empty());
        let later = HashMap::from([(1, "init")]);
        let gone = lineage.follow(&[exit(20, 1, 900)], &later);
        assert_eq!(gone[0].program, "build");
        assert!(gone[0].orphaned);
    }

    #[test]
    fn a_newcomer_a_snapshot_lists_is_left_to_the_ledger() {
        let mut lineage = Lineage::default();
        lineage.follow(&[fork(10, 20, 100)], &listed());
        lineage.forget_listed(|pid| pid == 20);
        assert!(lineage.follow(&[exit(20, 10, 900)], &listed()).is_empty());
    }

    #[test]
    fn a_child_the_previous_snapshot_listed_is_no_newcomer() {
        let mut lineage = Lineage::default();
        let events = [fork(1, 10, 100), exit(10, 1, 900)];
        assert!(lineage.follow(&events, &listed()).is_empty());
    }

    #[test]
    fn a_process_with_no_listed_ancestor_has_no_program_to_credit() {
        let mut lineage = Lineage::default();
        let events = [fork(77, 78, 100), exit(78, 1, 900)];
        assert!(lineage.follow(&events, &listed()).is_empty());
    }

    #[test]
    fn a_reset_forgets_every_newcomer() {
        let mut lineage = Lineage::default();
        lineage.follow(&[fork(10, 20, 100)], &listed());
        lineage.reset();
        assert!(lineage.follow(&[exit(20, 1, 900)], &listed()).is_empty());
    }
}
