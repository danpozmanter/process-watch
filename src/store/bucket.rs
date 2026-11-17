use std::collections::BTreeMap;

use crate::accounting::{ProgramTally, Tally};
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

/// One program's usage over one bucket, and how it splits among its processes by name.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Program {
    pub usage: Usage,
    /// The most separately started copies of the program alive at one sample.
    pub peak_runs: i64,
    /// The usage by process name: totals add up to the program's, and each
    /// part keeps its own peaks. Empty where a recorder kept no breakdown.
    pub parts: BTreeMap<String, Usage>,
}

impl Program {
    fn add_interval(&mut self, interval_ms: i64, tally: &ProgramTally) {
        self.usage.add_interval(interval_ms, &tally.total);
        self.peak_runs = self.peak_runs.max(tally.runs);
        for (name, part) in &tally.parts {
            self.parts
                .entry(name.clone())
                .or_default()
                .add_interval(interval_ms, part);
        }
    }

    /// Takes in the program as recorded over another span.
    pub fn absorb(&mut self, other: &Program) {
        self.usage.absorb(&other.usage);
        self.peak_runs = self.peak_runs.max(other.peak_runs);
        for (name, part) in &other.parts {
            self.parts.entry(name.clone()).or_default().absorb(part);
        }
    }

    /// Whether the program's one part is its own name, so the part is the
    /// whole program and the store need not write it.
    fn is_only_itself(&self, name: &str) -> bool {
        self.parts.len() == 1 && self.parts.get(name) == Some(&self.usage)
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
    pub programs: BTreeMap<String, Program>,
}

impl Bucket {
    /// An empty bucket for the minute holding `now_ms`.
    pub fn open(now_ms: i64, cpus: i64) -> Self {
        Self {
            minute: now_ms / MINUTE_MS,
            opened_ms: now_ms,
            covered_ms: 0,
            cpus,
            programs: BTreeMap::new(),
        }
    }

    /// Whether `other` is this bucket, as recorded in another file.
    pub fn is_same_as(&self, other: &Bucket) -> bool {
        self.minute == other.minute && self.opened_ms == other.opened_ms
    }

    /// Folds in one sampling interval of `interval_ms` and the usage measured over it.
    pub fn add(&mut self, interval_ms: i64, tallies: &BTreeMap<String, ProgramTally>) {
        self.covered_ms += interval_ms;
        for (name, tally) in tallies {
            self.programs
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
            programs: BTreeMap::new(),
        };
        for bucket in buckets {
            merged.covered_ms += bucket.covered_ms;
            for (name, program) in &bucket.programs {
                merged
                    .programs
                    .entry(name.clone())
                    .or_default()
                    .absorb(program);
            }
        }
        Some(merged)
    }

    /// The bucket as lines of the store's text format, in coarse units,
    /// fields separated by tabs: a `B` line naming the bucket, then per
    /// program a `G` line with its usage, and where its breakdown was
    /// recorded, an `R` line with its peak runs and a `P` line per process
    /// name. A program whose one process name is its own has no `P` line.
    pub fn encode(&self) -> String {
        let mut out = String::with_capacity(64 + self.programs.len() * 80);
        out.push_str(&format!(
            "B\t{}\t{}\t{}\t{}\n",
            self.minute, self.opened_ms, self.covered_ms, self.cpus
        ));
        let covered_ms = self.covered_ms.max(1);
        for (name, program) in &self.programs {
            if program.parts.is_empty() || program.is_only_itself(name) {
                let amounts = Amounts::of(&program.usage, covered_ms);
                push_line(&mut out, 'G', amounts, name);
                if !program.parts.is_empty() {
                    out.push_str(&format!("R\t{}\n", program.peak_runs));
                }
                continue;
            }
            let parts: Vec<(&String, Amounts)> = program
                .parts
                .iter()
                .map(|(part, usage)| (part, Amounts::of(usage, covered_ms)))
                .collect();
            let unbroken = Amounts::of(&program.unbroken_down(), covered_ms);
            let total = Amounts {
                cpu: parts.iter().map(|(_, a)| a.cpu).sum::<i64>() + unbroken.cpu,
                mem: parts.iter().map(|(_, a)| a.mem).sum::<i64>() + unbroken.mem,
                ..Amounts::of(&program.usage, covered_ms)
            };
            push_line(&mut out, 'G', total, name);
            out.push_str(&format!("R\t{}\n", program.peak_runs));
            for (part, amounts) in parts {
                push_line(&mut out, 'P', amounts, part);
            }
        }
        out
    }
}

impl Program {
    /// The CPU time and memory-time no part accounts for, as in buckets a
    /// recorder kept without a breakdown; only totals, since peaks do not subtract.
    pub fn unbroken_down(&self) -> Usage {
        let parts = self.parts.values();
        Usage {
            cpu_ns: self.usage.cpu_ns - parts.clone().map(|u| u.cpu_ns).sum::<i64>(),
            mem_byte_ms: self.usage.mem_byte_ms - parts.map(|u| u.mem_byte_ms).sum::<i64>(),
            ..Usage::default()
        }
    }
}

/// How a recording writes a usage's amounts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Units {
    /// CPU in nanoseconds, memory-time in byte-milliseconds, and peak memory
    /// in bytes, as pw 0.5 and earlier recorded them.
    Exact,
    /// CPU in milliseconds, memory as its average over the bucket in KiB,
    /// and peak memory in KiB: what a report shows, in far fewer digits.
    Coarse,
}

const KIB: i64 = 1024;

/// A usage's amounts on one line, in coarse units.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Amounts {
    cpu: i64,
    peak_cpu: i64,
    mem: i64,
    peak_mem: i64,
    peak_procs: i64,
}

impl Amounts {
    /// The usage of a bucket covering `covered_ms`, each amount rounded to its nearest unit.
    fn of(u: &Usage, covered_ms: i64) -> Self {
        Self {
            cpu: rounded(u.cpu_ns.into(), NS_PER_MS.into()),
            peak_cpu: u.peak_cpu_permille,
            mem: rounded(
                u.mem_byte_ms.into(),
                i128::from(covered_ms) * i128::from(KIB),
            ),
            peak_mem: rounded(u.peak_mem.into(), KIB.into()),
            peak_procs: u.peak_procs,
        }
    }

    fn read([cpu, peak_cpu, mem, peak_mem, peak_procs]: [i64; 5]) -> Self {
        Self {
            cpu,
            peak_cpu,
            mem,
            peak_mem,
            peak_procs,
        }
    }

    /// The usage these amounts, written in `units`, stand for in a bucket covering `covered_ms`.
    fn usage(self, units: Units, covered_ms: i64) -> Usage {
        match units {
            Units::Exact => Usage {
                cpu_ns: self.cpu,
                peak_cpu_permille: self.peak_cpu,
                mem_byte_ms: self.mem,
                peak_mem: self.peak_mem,
                peak_procs: self.peak_procs,
            },
            Units::Coarse => Usage {
                cpu_ns: self.cpu.saturating_mul(NS_PER_MS),
                peak_cpu_permille: self.peak_cpu,
                mem_byte_ms: self.mem.saturating_mul(KIB).saturating_mul(covered_ms),
                peak_mem: self.peak_mem.saturating_mul(KIB),
                peak_procs: self.peak_procs,
            },
        }
    }
}

/// `amount` in whole `unit`s, to the nearest; amounts are never negative.
fn rounded(amount: i128, unit: i128) -> i64 {
    i64::try_from((amount + unit / 2) / unit.max(1)).unwrap_or(i64::MAX)
}

fn push_line(out: &mut String, kind: char, a: Amounts, name: &str) {
    let clean = name.replace(['\n', '\r'], " ");
    out.push_str(&format!(
        "{kind}\t{}\t{}\t{}\t{}\t{}\t{clean}\n",
        a.cpu, a.peak_cpu, a.mem, a.peak_mem, a.peak_procs
    ));
}

/// Where decoding is: the program its `R` and `P` lines belong to, and
/// whether the parts hold only the stand-in an `R` line gave them.
#[derive(Default)]
struct Cursor {
    program: Option<String>,
    standing_in: bool,
}

/// Buckets held in the store's text format, written in `units`; lines that
/// do not parse, such as one a recorder was still appending when it
/// stopped, are skipped.
pub fn decode(text: &str, units: Units) -> Vec<Bucket> {
    let mut buckets: Vec<Bucket> = Vec::new();
    let mut cursor = Cursor::default();
    for line in text.lines() {
        // The name is last, and may itself hold tabs.
        let fields: Vec<&str> = line.splitn(7, '\t').collect();
        match fields.as_slice() {
            ["B", numbers @ ..] => {
                buckets.extend(decode_header(numbers));
                cursor = Cursor::default();
            }
            ["G", numbers @ .., name] => {
                cursor = Cursor::default();
                let Some(bucket) = buckets.last_mut() else {
                    continue;
                };
                if let Some(usage) = decode_usage(numbers, units, bucket.covered_ms) {
                    let program = Program {
                        usage,
                        ..Program::default()
                    };
                    bucket.programs.insert((*name).to_owned(), program);
                    cursor.program = Some((*name).to_owned());
                }
            }
            ["R", runs] => decode_runs(&mut buckets, &mut cursor, runs),
            ["P", numbers @ .., name] => {
                decode_part(&mut buckets, &mut cursor, units, numbers, name)
            }
            _ => {}
        }
    }
    buckets
}

fn program_at<'a>(buckets: &'a mut [Bucket], cursor: &Cursor) -> Option<&'a mut Program> {
    let name = cursor.program.as_deref()?;
    buckets.last_mut()?.programs.get_mut(name)
}

/// An `R` line: the program's peak runs, and its own name as its one part
/// until a `P` line says otherwise.
fn decode_runs(buckets: &mut [Bucket], cursor: &mut Cursor, runs: &str) {
    let (Some(name), Ok(runs)) = (cursor.program.clone(), runs.parse()) else {
        return;
    };
    let Some(program) = program_at(buckets, cursor) else {
        return;
    };
    program.peak_runs = runs;
    program.parts = BTreeMap::from([(name, program.usage)]);
    cursor.standing_in = true;
}

fn decode_part(
    buckets: &mut [Bucket],
    cursor: &mut Cursor,
    units: Units,
    numbers: &[&str],
    name: &str,
) {
    let covered_ms = buckets.last().map_or(0, |b| b.covered_ms);
    let (Some(program), Some(usage)) = (
        program_at(buckets, cursor),
        decode_usage(numbers, units, covered_ms),
    ) else {
        return;
    };
    if cursor.standing_in {
        program.parts.clear();
        cursor.standing_in = false;
    }
    program.parts.insert(name.to_owned(), usage);
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
        programs: BTreeMap::new(),
    })
}

fn decode_usage(fields: &[&str], units: Units, covered_ms: i64) -> Option<Usage> {
    Some(Amounts::read(parse_numbers(fields)?).usage(units, covered_ms))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: i64 = 1_000_000;

    fn tally(cpu_ns: i64, mem_bytes: i64, procs: i64) -> Tally {
        Tally {
            cpu_ns,
            mem_bytes,
            procs,
        }
    }

    /// One program of one run, all of whose processes share its name.
    fn alone(name: &str, total: Tally) -> BTreeMap<String, ProgramTally> {
        let program = ProgramTally {
            total,
            runs: 1,
            parts: BTreeMap::from([(name.to_owned(), total)]),
        };
        BTreeMap::from([(name.to_owned(), program)])
    }

    fn coarse(text: &str) -> Vec<Bucket> {
        decode(text, Units::Coarse)
    }

    #[test]
    fn a_bucket_survives_encoding() {
        let mut bucket = Bucket::open(120_000, 8);
        let name = "fire\tfox".to_owned();
        bucket.add(5000, &alone(&name, tally(2_500 * MS, 3 * KIB, 4)));
        bucket.add(5000, &alone(&name, tally(500 * MS, KIB, 2)));
        let back = coarse(&bucket.encode());
        assert_eq!(back, [bucket.clone()]);
        let u = back[0].programs[&name].usage;
        assert_eq!(back[0].minute, 2);
        assert_eq!(back[0].covered_ms, 10_000);
        assert_eq!(u.cpu_ns, 3_000 * MS);
        assert_eq!(u.peak_cpu_permille, 500);
        assert_eq!(u.mem_byte_ms, 20_000 * KIB);
        assert_eq!(u.peak_mem, 3 * KIB);
        assert_eq!(u.peak_procs, 4);
    }

    #[test]
    fn amounts_are_written_to_the_nearest_millisecond_and_kib() {
        let mut bucket = Bucket::open(0, 1);
        bucket.add(
            60_000,
            &alone("a", tally(2_500_000_000 + 600_000, 10 * KIB + 600, 1)),
        );
        assert_eq!(
            bucket.encode(),
            "B\t0\t0\t60000\t1\nG\t2501\t41\t11\t11\t1\ta\nR\t1\n"
        );
    }

    #[test]
    fn exact_units_are_read_as_written() {
        let back = decode(
            "B\t1\t60000\t5000\t4\nG\t10\t2\t30\t40\t1\tsh\n",
            Units::Exact,
        );
        let sh = back[0].programs["sh"].usage;
        assert_eq!((sh.cpu_ns, sh.mem_byte_ms, sh.peak_mem), (10, 30, 40));
    }

    #[test]
    fn a_partly_written_line_is_skipped_and_a_numeric_name_kept() {
        let back = coarse("B\t1\t60000\t5000\t4\nG\t10\t2\t30\t40\t1\t7\nG\t7\t1");
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].programs.len(), 1);
        assert_eq!(back[0].programs["7"].usage.cpu_ns, 10 * MS);
    }

    #[test]
    fn usage_lines_before_any_header_and_unknown_lines_are_ignored() {
        let back = coarse("G\t1\t2\t3\t4\t5\tlost\nX\tjunk\nB\t1\t2\t3\t4\nB\tx\t2\t3\t4\n");
        assert_eq!(back.len(), 1);
        assert!(back[0].programs.is_empty());
    }

    #[test]
    fn line_breaks_in_a_name_cannot_split_its_line() {
        let mut bucket = Bucket::open(0, 1);
        bucket.add(1000, &alone("two\nlines", Tally::default()));
        let back = coarse(&bucket.encode());
        assert_eq!(back[0].programs["two lines"].parts.len(), 1);
    }

    #[test]
    fn merging_adds_totals_keeps_peaks_and_the_newest_cpu_count() {
        let mut first = Bucket::open(60_000, 4);
        first.add(60_000, &alone("a", tally(6_000_000_000, 50, 2)));
        let mut second = Bucket::open(120_000, 8);
        second.add(30_000, &alone("a", tally(30_000_000_000, 80, 1)));
        let merged = Bucket::merge(&[second, first]).unwrap_or_else(|| panic!("two buckets"));
        assert_eq!(merged.minute, 1);
        assert_eq!(merged.opened_ms, 60_000);
        assert_eq!(merged.covered_ms, 90_000);
        assert_eq!(merged.cpus, 8);
        let a = merged.programs["a"].usage;
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

    fn two_builds() -> Bucket {
        let mut bucket = Bucket::open(60_000, 4);
        let program = ProgramTally {
            total: tally(9_000 * MS, 700 * KIB, 5),
            runs: 2,
            parts: BTreeMap::from([
                ("cargo".to_owned(), tally(1_000 * MS, 100 * KIB, 2)),
                ("rustc".to_owned(), tally(8_000 * MS, 600 * KIB, 3)),
            ]),
        };
        bucket.add(10_000, &BTreeMap::from([("cargo".to_owned(), program)]));
        bucket
    }

    #[test]
    fn a_breakdown_survives_encoding() {
        let bucket = two_builds();
        let back = coarse(&bucket.encode());
        assert_eq!(back, [bucket]);
        let cargo = &back[0].programs["cargo"];
        assert_eq!(cargo.peak_runs, 2);
        assert_eq!(cargo.parts["rustc"].peak_mem, 600 * KIB);
        assert_eq!(cargo.parts["cargo"].peak_procs, 2);
    }

    #[test]
    fn a_programs_total_is_written_as_the_sum_of_its_rounded_parts() {
        let mut bucket = Bucket::open(0, 1);
        let program = ProgramTally {
            total: tally(3 * MS, 0, 2),
            runs: 1,
            parts: BTreeMap::from([
                ("a".to_owned(), tally(MS + MS / 2, 0, 1)),
                ("b".to_owned(), tally(MS + MS / 2, 0, 1)),
            ]),
        };
        bucket.add(1000, &BTreeMap::from([("p".to_owned(), program)]));
        let back = coarse(&bucket.encode());
        let p = &back[0].programs["p"];
        assert_eq!(p.usage.cpu_ns, 4 * MS);
        assert_eq!(p.unbroken_down(), Usage::default());
    }

    #[test]
    fn a_program_that_is_only_itself_writes_no_parts() {
        let mut bucket = Bucket::open(60_000, 4);
        bucket.add(1000, &alone("sh", tally(5 * MS, 10 * KIB, 1)));
        let text = bucket.encode();
        assert_eq!(text.lines().filter(|l| l.starts_with('P')).count(), 0);
        assert_eq!(coarse(&text), [bucket]);
    }

    #[test]
    fn a_program_from_an_older_recorder_has_no_breakdown() {
        let back = coarse("B\t1\t60000\t5000\t4\nG\t10\t2\t30\t40\t1\tcargo\n");
        let cargo = &back[0].programs["cargo"];
        assert!(cargo.parts.is_empty());
        assert_eq!(cargo.peak_runs, 0);
        assert_eq!(coarse(&back[0].encode()), back);
    }

    #[test]
    fn merging_adds_up_each_part_and_keeps_the_most_runs() {
        let mut older = Bucket::open(0, 4);
        older.add(1000, &alone("cargo", tally(4 * MS, 50 * KIB, 1)));
        let merged = Bucket::merge(&[older, two_builds()]).unwrap_or_else(|| panic!("two buckets"));
        let cargo = &merged.programs["cargo"];
        assert_eq!(cargo.peak_runs, 2);
        assert_eq!(cargo.parts["cargo"].cpu_ns, 1_004 * MS);
        assert_eq!(cargo.parts["cargo"].peak_mem, 100 * KIB);
        assert_eq!(cargo.parts["rustc"].peak_procs, 3);
        let written = merged.encode();
        assert_eq!(coarse(&written)[0].encode(), written);
    }

    #[test]
    fn what_no_part_accounts_for_keeps_its_own_total_when_written() {
        let legacy = decode(
            "B\t0\t0\t1000\t4\nG\t7000000\t2\t3072000\t3072\t1\tcargo\n",
            Units::Exact,
        );
        let merged =
            Bucket::merge(&[legacy[0].clone(), two_builds()]).unwrap_or_else(|| panic!("two"));
        let back = coarse(&merged.encode());
        let cargo = &back[0].programs["cargo"];
        assert_eq!(cargo.unbroken_down().cpu_ns, 7 * MS);
        assert_eq!(cargo.usage.cpu_ns, 9_007 * MS);
    }
}
