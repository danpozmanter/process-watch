use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::accounting::Ledger;
use crate::snapshot::{self, Snapshot};
use crate::span::{self, DAY_MS, HOUR_MS, MINUTE_MS};
use crate::store::{self, Bucket, RecorderLock};
use crate::{data_dir, rollup};

/// How the recorder samples and how long it keeps what it records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordOptions {
    pub interval_ms: i64,
    pub retain_ms: i64,
    pub dir: Option<PathBuf>,
}

/// Samples every process at a fixed interval until stopped, writing
/// per-program usage into minute buckets under the data folder.
pub fn record(opts: &RecordOptions) -> Result<()> {
    let dir = data_dir::resolve(opts.dir.as_deref())?;
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let _lock = RecorderLock::acquire(&dir)?;
    store::adopt_current(&dir)?;
    tidy(&dir, store::now_ms(), opts.retain_ms)?;
    eprintln!(
        "pw: recording every {} into {}",
        span::format(opts.interval_ms),
        dir.display()
    );
    let interval = Duration::from_millis(opts.interval_ms.unsigned_abs());
    let mut lister = snapshot::Lister::new();
    let mut recorder = Recorder::new(dir, opts, snapshot::cpu_count(), store::now_ms());
    loop {
        let sampled_at = Instant::now();
        let snap = lister.take().context("listing processes")?;
        recorder.add_sample(&snap, sampled_at, store::now_ms())?;
        std::thread::sleep(interval.saturating_sub(sampled_at.elapsed()));
    }
}

/// Drops hours older than the retention and folds each hour older than a day
/// into one bucket: minute detail is kept for the day reports look at most,
/// and a month of hours stays small.
fn tidy(dir: &Path, now_ms: i64, retain_ms: i64) -> Result<()> {
    store::prune(dir, now_ms - retain_ms)?;
    store::compact(dir, now_ms - DAY_MS)
}

/// What the recorder carries from one sample to the next.
struct Recorder {
    dir: PathBuf,
    retain_ms: i64,
    cpus: i64,
    /// An interval longer than this means the host slept or the recorder
    /// stalled; nothing was observed across it, so it starts a fresh baseline.
    max_gap: Duration,
    ledger: Ledger,
    previous_sample: Option<Instant>,
    bucket: Bucket,
    wrote_current: bool,
}

impl Recorder {
    fn new(dir: PathBuf, opts: &RecordOptions, cpus: i64, now_ms: i64) -> Self {
        Self {
            dir,
            retain_ms: opts.retain_ms,
            cpus,
            max_gap: Duration::from_millis(opts.interval_ms.unsigned_abs()) * 3,
            ledger: Ledger::default(),
            previous_sample: None,
            bucket: Bucket::open(now_ms, cpus),
            wrote_current: false,
        }
    }

    /// Accounts for one snapshot, taken at `sampled_at` when the wall clock
    /// read `now_ms`, and leaves the minute in progress on disk.
    fn add_sample(&mut self, snap: &Snapshot, sampled_at: Instant, now_ms: i64) -> Result<()> {
        let roots = rollup::program_roots(&snap.procs);
        let usage = self.ledger.tally(snap, &roots);
        let observed = self.observed_interval(sampled_at);
        self.restart_bucket_if_cleared(now_ms);
        self.roll_over(now_ms)?;
        let Some(interval) = observed else {
            return Ok(());
        };
        let interval_ms = i64::try_from(interval.as_millis()).unwrap_or(i64::MAX);
        self.bucket.add(interval_ms.max(1), &usage);
        store::write_current(&self.dir, &self.bucket)?;
        self.wrote_current = true;
        Ok(())
    }

    /// The time since the previous sample, when the recorder watched all of it.
    fn observed_interval(&mut self, sampled_at: Instant) -> Option<Duration> {
        let previous = self.previous_sample.replace(sampled_at)?;
        Some(sampled_at - previous).filter(|&gap| gap <= self.max_gap)
    }

    /// A clear removed what was recorded until now, the minute in progress included.
    fn restart_bucket_if_cleared(&mut self, now_ms: i64) {
        if self.wrote_current && store::current_missing(&self.dir) {
            self.bucket = Bucket::open(now_ms, self.cpus);
            self.wrote_current = false;
        }
    }

    /// Archives the bucket once its minute has passed, and tidies the folder
    /// once its hour has.
    fn roll_over(&mut self, now_ms: i64) -> Result<()> {
        let minute = now_ms / MINUTE_MS;
        if minute == self.bucket.minute {
            return Ok(());
        }
        if self.bucket.covered_ms > 0 {
            store::append_bucket(&self.dir, &self.bucket)?;
        }
        if minute * MINUTE_MS / HOUR_MS != self.bucket.minute * MINUTE_MS / HOUR_MS {
            tidy(&self.dir, now_ms, self.retain_ms)?;
        }
        self.bucket = Bucket::open(now_ms, self.cpus);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::snapshot::Proc;
    use crate::span::SECOND_MS;

    const START_MS: i64 = 100 * HOUR_MS;

    /// A recorder writing into a fresh folder, with the folder removed when dropped.
    struct Harness {
        recorder: Recorder,
        start: Instant,
    }

    impl Harness {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("pw-recorder-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            let opts = RecordOptions {
                interval_ms: 5 * SECOND_MS,
                retain_ms: 12 * HOUR_MS,
                dir: None,
            };
            Self {
                recorder: Recorder::new(dir, &opts, 4, START_MS),
                start: Instant::now(),
            }
        }

        /// Samples one busy process `elapsed_ms` after the start, having used `cpu_ms` in all.
        fn sample(&mut self, elapsed_ms: i64, cpu_ms: i64) {
            let proc = Proc {
                pid: 7,
                ppid: 0,
                pgid: 7,
                start_ns: 1,
                cpu_ns: cpu_ms * 1_000_000,
                mem_bytes: 1000,
                name: "busy".to_owned(),
                ..Proc::default()
            };
            let snap = Snapshot {
                procs: vec![proc],
                now_ns: 2 + elapsed_ms * 1_000_000,
                resolution_ns: 1,
            };
            let at = self.start + Duration::from_millis(elapsed_ms.unsigned_abs());
            self.recorder
                .add_sample(&snap, at, START_MS + elapsed_ms)
                .unwrap();
        }

        fn loaded(&self) -> Vec<Bucket> {
            store::load(&self.recorder.dir, 0).unwrap()
        }
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.recorder.dir);
        }
    }

    #[test]
    fn the_first_sample_is_only_a_baseline() {
        let mut harness = Harness::new("baseline");
        harness.sample(0, 100);
        assert!(harness.loaded().is_empty());
    }

    #[test]
    fn each_later_sample_leaves_the_minute_in_progress_on_disk() {
        let mut harness = Harness::new("current");
        harness.sample(0, 100);
        harness.sample(5000, 600);
        harness.sample(10_000, 1100);
        let loaded = harness.loaded();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].covered_ms, 10_000);
        assert_eq!(loaded[0].cpus, 4);
        assert_eq!(loaded[0].usage["busy"].cpu_ns, 1_000_000_000);
        assert_eq!(loaded[0].usage["busy"].peak_cpu_permille, 100);
    }

    #[test]
    fn a_new_minute_archives_the_last_one() {
        let mut harness = Harness::new("roll");
        harness.sample(50_000, 0);
        harness.sample(55_000, 500);
        harness.sample(60_000, 1000);
        let loaded = harness.loaded();
        assert_eq!(loaded.len(), 2);
        assert!(loaded.iter().all(|b| b.covered_ms == 5000));
    }

    #[test]
    fn time_the_recorder_did_not_watch_is_not_covered() {
        let mut harness = Harness::new("gap");
        harness.sample(0, 0);
        harness.sample(5000, 100);
        harness.sample(50_000, 200);
        assert_eq!(harness.loaded()[0].covered_ms, 5000);
    }

    #[test]
    fn a_clear_starts_the_minute_afresh() {
        let mut harness = Harness::new("clear");
        harness.sample(0, 0);
        harness.sample(5000, 100);
        store::clear(&harness.recorder.dir).unwrap();
        harness.sample(10_000, 200);
        let loaded = harness.loaded();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].covered_ms, 5000);
        assert_eq!(loaded[0].opened_ms, START_MS + 10_000);
    }
}
