use std::collections::BTreeMap;

use crate::accounting::Tally;
use crate::span::{MINUTE_MS, NS_PER_MS};

/// One program's usage over one bucket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Usage {
    pub cpu_ns: i64,
    /// Thousandths of one CPU at the busiest sampling interval.
    pub peak_cpu_permille: i64,
    pub mem_byte_ms: i64,
    pub peak_mem: i64,
    pub peak_procs: i64,
}

impl Usage {
    /// Adds one sampling interval of `interval_ms` in which the program used `tally`.
    fn add_interval(&mut self, interval_ms: i64, tally: &Tally) {
        let interval_ns = (interval_ms * NS_PER_MS).max(1);
        self.cpu_ns += tally.cpu_ns;
        self.peak_cpu_permille = self
            .peak_cpu_permille
            .max(tally.cpu_ns * 1000 / interval_ns);
        self.mem_byte_ms += tally.mem_bytes * interval_ms;
        self.peak_mem = self.peak_mem.max(tally.mem_bytes);
        self.peak_procs = self.peak_procs.max(tally.procs);
    }

    /// Takes in usage recorded over another span: totals add up, peaks keep the largest.
    pub fn absorb(&mut self, other: &Usage) {
        self.cpu_ns += other.cpu_ns;
        self.peak_cpu_permille = self.peak_cpu_permille.max(other.peak_cpu_permille);
        self.mem_byte_ms += other.mem_byte_ms;
        self.peak_mem = self.peak_mem.max(other.peak_mem);
        self.peak_procs = self.peak_procs.max(other.peak_procs);
    }
}

/// One wall-clock minute of recording by one recorder run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bucket {
    pub minute: i64,
    /// When the recorder opened this bucket; with `minute` it names the bucket
    /// in both the hour file and the current file, so neither is counted twice.
    pub opened_ms: i64,
    pub covered_ms: i64,
    pub cpus: i64,
    pub usage: BTreeMap<String, Usage>,
}

impl Bucket {
    /// An empty bucket for the minute holding `now_ms`.
    pub fn open(now_ms: i64, cpus: i64) -> Self {
        Self {
            minute: now_ms / MINUTE_MS,
            opened_ms: now_ms,
            covered_ms: 0,
            cpus,
            usage: BTreeMap::new(),
        }
    }

    /// Whether `other` is this bucket, as recorded in another file.
    pub fn is_same_as(&self, other: &Bucket) -> bool {
        self.minute == other.minute && self.opened_ms == other.opened_ms
    }

    /// Folds in one sampling interval of `interval_ms` and the usage measured over it.
    pub fn add(&mut self, interval_ms: i64, tallies: &BTreeMap<String, Tally>) {
        self.covered_ms += interval_ms;
        for (name, tally) in tallies {
            self.usage
                .entry(name.clone())
                .or_default()
                .add_interval(interval_ms, tally);
        }
    }

    /// One bucket holding everything in `buckets`: usage and covered time add
    /// up and peaks keep the largest, so a report over it matches one over
    /// the buckets it replaces, at the granularity of the whole span.
    pub fn merge(buckets: &[Bucket]) -> Option<Self> {
        let first = buckets.iter().min_by_key(|b| (b.minute, b.opened_ms))?;
        let newest = buckets.iter().max_by_key(|b| b.opened_ms)?;
        let mut merged = Self {
            minute: first.minute,
            opened_ms: first.opened_ms,
            covered_ms: 0,
            cpus: newest.cpus,
            usage: BTreeMap::new(),
        };
        for bucket in buckets {
            merged.covered_ms += bucket.covered_ms;
            for (name, usage) in &bucket.usage {
                merged.usage.entry(name.clone()).or_default().absorb(usage);
            }
        }
        Some(merged)
    }

    /// The bucket as lines of the store's text format: a `B` line naming the
    /// bucket, then one `G` line per program, fields separated by tabs.
    pub fn encode(&self) -> String {
        let mut out = String::with_capacity(64 + self.usage.len() * 64);
        out.push_str(&format!(
            "B\t{}\t{}\t{}\t{}\n",
            self.minute, self.opened_ms, self.covered_ms, self.cpus
        ));
        for (name, u) in &self.usage {
            let clean = name.replace(['\n', '\r'], " ");
            out.push_str(&format!(
                "G\t{}\t{}\t{}\t{}\t{}\t{clean}\n",
                u.cpu_ns, u.peak_cpu_permille, u.mem_byte_ms, u.peak_mem, u.peak_procs
            ));
        }
        out
    }
}

/// Buckets held in the store's text format; lines that do not parse, such as
/// one a recorder was still appending when it stopped, are skipped.
pub fn decode(text: &str) -> Vec<Bucket> {
    let mut buckets: Vec<Bucket> = Vec::new();
    for line in text.lines() {
        // The program name is last, and may itself hold tabs.
        let fields: Vec<&str> = line.splitn(7, '\t').collect();
        match fields.as_slice() {
            ["B", numbers @ ..] => buckets.extend(decode_header(numbers)),
            ["G", numbers @ .., name] => {
                if let (Some(usage), Some(bucket)) = (decode_usage(numbers), buckets.last_mut()) {
                    bucket.usage.insert((*name).to_owned(), usage);
                }
            }
            _ => {}
        }
    }
    buckets
}

/// Exactly `N` whole numbers, or `None`.
fn parse_numbers<const N: usize>(fields: &[&str]) -> Option<[i64; N]> {
    let mut parsed = [0; N];
    if fields.len() != N {
        return None;
    }
    for (slot, field) in parsed.iter_mut().zip(fields) {
        *slot = field.parse().ok()?;
    }
    Some(parsed)
}

fn decode_header(fields: &[&str]) -> Option<Bucket> {
    let [minute, opened_ms, covered_ms, cpus] = parse_numbers(fields)?;
    Some(Bucket {
        minute,
        opened_ms,
        covered_ms,
        cpus,
        usage: BTreeMap::new(),
    })
}

fn decode_usage(fields: &[&str]) -> Option<Usage> {
    let [cpu_ns, peak_cpu_permille, mem_byte_ms, peak_mem, peak_procs] = parse_numbers(fields)?;
    Some(Usage {
        cpu_ns,
        peak_cpu_permille,
        mem_byte_ms,
        peak_mem,
        peak_procs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tally(cpu_ns: i64, mem_bytes: i64, procs: i64) -> Tally {
        Tally {
            cpu_ns,
            mem_bytes,
            procs,
        }
    }

    #[test]
    fn a_bucket_survives_encoding() {
        let mut bucket = Bucket::open(120_000, 8);
        let name = "fire\tfox".to_owned();
        bucket.add(
            5000,
            &BTreeMap::from([(name.clone(), tally(2_500_000_000, 300, 4))]),
        );
        bucket.add(
            5000,
            &BTreeMap::from([(name.clone(), tally(500_000_000, 100, 2))]),
        );
        let back = decode(&bucket.encode());
        assert_eq!(back, [bucket.clone()]);
        let u = back[0].usage[&name];
        assert_eq!(back[0].minute, 2);
        assert_eq!(back[0].covered_ms, 10_000);
        assert_eq!(u.cpu_ns, 3_000_000_000);
        assert_eq!(u.peak_cpu_permille, 500);
        assert_eq!(u.mem_byte_ms, 2_000_000);
        assert_eq!(u.peak_mem, 300);
        assert_eq!(u.peak_procs, 4);
    }

    #[test]
    fn a_partly_written_line_is_skipped_and_a_numeric_name_kept() {
        let back = decode("B\t1\t60000\t5000\t4\nG\t10\t2\t30\t40\t1\t7\nG\t7\t1");
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].usage.len(), 1);
        assert_eq!(back[0].usage["7"].cpu_ns, 10);
    }

    #[test]
    fn usage_lines_before_any_header_and_unknown_lines_are_ignored() {
        let back = decode("G\t1\t2\t3\t4\t5\tlost\nX\tjunk\nB\t1\t2\t3\t4\nB\tx\t2\t3\t4\n");
        assert_eq!(back.len(), 1);
        assert!(back[0].usage.is_empty());
    }

    #[test]
    fn line_breaks_in_a_name_cannot_split_its_line() {
        let mut bucket = Bucket::open(0, 1);
        bucket.add(
            1000,
            &BTreeMap::from([("two\nlines".to_owned(), Tally::default())]),
        );
        let back = decode(&bucket.encode());
        assert!(back[0].usage.contains_key("two lines"));
    }

    #[test]
    fn merging_adds_totals_keeps_peaks_and_the_newest_cpu_count() {
        let mut first = Bucket::open(60_000, 4);
        first.add(
            60_000,
            &BTreeMap::from([("a".to_owned(), tally(6_000_000_000, 50, 2))]),
        );
        let mut second = Bucket::open(120_000, 8);
        second.add(
            30_000,
            &BTreeMap::from([("a".to_owned(), tally(30_000_000_000, 80, 1))]),
        );
        let merged = Bucket::merge(&[second, first]).unwrap_or_else(|| panic!("two buckets"));
        assert_eq!(merged.minute, 1);
        assert_eq!(merged.opened_ms, 60_000);
        assert_eq!(merged.covered_ms, 90_000);
        assert_eq!(merged.cpus, 8);
        let a = merged.usage["a"];
        assert_eq!(a.cpu_ns, 36_000_000_000);
        assert_eq!(a.peak_cpu_permille, 1000);
        assert_eq!(a.mem_byte_ms, 50 * 60_000 + 80 * 30_000);
        assert_eq!(a.peak_mem, 80);
        assert_eq!(a.peak_procs, 2);
    }

    #[test]
    fn merging_nothing_is_nothing() {
        assert_eq!(Bucket::merge(&[]), None);
    }
}
