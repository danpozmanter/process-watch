use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use crate::sort_key::SortKey;
use crate::span::{self, NS_PER_MS};
use crate::store::{self, Bucket, RecorderLock, Usage};
use crate::threshold::Threshold;
use crate::{data_dir, size};

/// Which end of the ranking comes first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Order {
    Desc,
    Asc,
}

/// What a report covers and how it ranks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportOptions {
    pub number: usize,
    pub order: Order,
    pub sort: SortKey,
    pub period_ms: i64,
    /// Shows only programs whose name contains this, ignoring case.
    pub pattern: Option<String>,
    pub threshold: Option<Threshold>,
    pub dir: Option<PathBuf>,
}

const COLUMNS: [&str; 7] = [
    "PROGRAM", "PROCS", "CPU TIME", "AVG CPU", "PEAK CPU", "AVG RAM", "PEAK RAM",
];

/// One program's usage over the whole period.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Row {
    name: String,
    usage: Usage,
}

/// Everything recorded over a period, merged per program.
#[derive(Debug, PartialEq, Eq)]
struct Summary {
    rows: Vec<Row>,
    covered_ms: i64,
    cpus: i64,
}

/// The report for the period ending now, or an error saying why there is nothing to show.
pub fn report(opts: &ReportOptions) -> Result<String> {
    let dir = data_dir::resolve(opts.dir.as_deref())?;
    let buckets = store::load(&dir, store::now_ms() - opts.period_ms)?;
    if buckets.is_empty() {
        bail!(nothing_recorded(&dir, opts.period_ms));
    }
    let summary = summarize(&buckets).filtered(opts);
    let ranked = rank(summary.rows.clone(), opts.sort, opts.order, opts.number);
    Ok(render(&summary, &ranked, opts))
}

fn nothing_recorded(dir: &Path, period_ms: i64) -> String {
    let hint = if RecorderLock::is_held(dir) {
        "the recorder is running; its first minute appears after two samples"
    } else {
        "start the recorder with `pw record` (see README for running it as a service)"
    };
    format!(
        "nothing recorded in the past {} under {}; {hint}",
        span::format(period_ms),
        dir.display()
    )
}

/// Merges buckets per program: CPU time and memory-time add up, peaks take the largest.
fn summarize(buckets: &[Bucket]) -> Summary {
    let mut by_name: BTreeMap<&str, Usage> = BTreeMap::new();
    for bucket in buckets {
        for (name, usage) in &bucket.usage {
            by_name.entry(name).or_default().absorb(usage);
        }
    }
    // The CPU count of the newest bucket, as the host has it now.
    let cpus = buckets
        .iter()
        .max_by_key(|b| b.opened_ms)
        .map_or(1, |b| b.cpus);
    Summary {
        rows: by_name
            .into_iter()
            .map(|(name, usage)| Row {
                name: name.to_owned(),
                usage,
            })
            .collect(),
        covered_ms: buckets.iter().map(|b| b.covered_ms).sum(),
        cpus,
    }
}

impl Summary {
    /// The summary keeping only the programs the pattern and threshold admit.
    fn filtered(mut self, opts: &ReportOptions) -> Self {
        let pattern = opts.pattern.as_deref().map(str::to_lowercase);
        let covered_ms = self.covered_ms.max(1);
        self.rows.retain(|row| {
            pattern
                .as_deref()
                .is_none_or(|p| row.name.to_lowercase().contains(p))
                && opts
                    .threshold
                    .is_none_or(|t| t.admits(&row.usage, covered_ms))
        });
        self
    }
}

/// The row's ranking value, then its peak RAM and peak CPU to break ties.
fn ranking_key(row: &Row, sort: SortKey) -> (i64, i64, i64) {
    let usage = &row.usage;
    (
        sort.rank_value(usage),
        usage.peak_mem,
        usage.peak_cpu_permille,
    )
}

/// The first `number` rows, ranked by `sort` in `order`; ties fall to the
/// peaks, then to the name, so the ranking is the same on every run.
fn rank(mut rows: Vec<Row>, sort: SortKey, order: Order, number: usize) -> Vec<Row> {
    rows.sort_by(|a, b| {
        let by_usage = ranking_key(a, sort).cmp(&ranking_key(b, sort));
        let directed = match order {
            Order::Asc => by_usage,
            Order::Desc => by_usage.reverse(),
        };
        directed.then_with(|| a.name.cmp(&b.name))
    });
    rows.truncate(number);
    rows
}

/// The two header lines naming the period, the coverage, the ranking, and
/// any pattern and threshold the programs were chosen by.
fn heading(summary: &Summary, shown: usize, opts: &ReportOptions) -> String {
    let measure = opts.sort.label();
    let direction = match opts.order {
        Order::Desc => "highest",
        Order::Asc => "lowest",
    };
    let mut chosen_by = String::new();
    if let Some(pattern) = &opts.pattern {
        chosen_by.push_str(&format!(", matching `{pattern}`"));
    }
    if let Some(threshold) = opts.threshold {
        chosen_by.push_str(&format!(", with {}", threshold.describe()));
    }
    format!(
        "Past {}: {} recorded, {} CPUs (100% CPU = one core)\n\
         Top {shown} of {} programs by {measure}, {direction} first{chosen_by}\n\n",
        span::format(opts.period_ms),
        span::format(summary.covered_ms),
        summary.cpus,
        summary.rows.len(),
    )
}

fn percent(permille: f64) -> String {
    format!("{:.1}%", permille / 10.0)
}

/// The table cells of one row, over `covered_ms` of recording.
fn cells(row: &Row, covered_ms: i64) -> Vec<String> {
    let u = &row.usage;
    let avg_cpu_permille = u.cpu_ns as f64 * 1000.0 / (covered_ms * NS_PER_MS) as f64;
    vec![
        row.name.clone(),
        u.peak_procs.to_string(),
        span::format(u.cpu_ns / NS_PER_MS),
        percent(avg_cpu_permille),
        percent(u.peak_cpu_permille as f64),
        size::format(u.mem_byte_ms / covered_ms),
        size::format(u.peak_mem),
    ]
}

/// The table as text: the first column aligned left, the others right,
/// two spaces between columns.
fn align(table: &[Vec<String>]) -> String {
    let mut widths = vec![0usize; COLUMNS.len()];
    for line in table {
        for (width, cell) in widths.iter_mut().zip(line) {
            *width = (*width).max(cell.chars().count());
        }
    }
    let mut out = String::new();
    for line in table {
        let mut text = String::new();
        for (column, (cell, &width)) in line.iter().zip(&widths).enumerate() {
            if column == 0 {
                text.push_str(&format!("{cell:<width$}"));
            } else {
                text.push_str(&format!("  {cell:>width$}"));
            }
        }
        out.push_str(text.trim_end());
        out.push('\n');
    }
    out
}

/// The ranked rows as an aligned table under a header naming the period;
/// with no rows, the header alone.
fn render(summary: &Summary, ranked: &[Row], opts: &ReportOptions) -> String {
    let heading = heading(summary, ranked.len(), opts);
    if ranked.is_empty() {
        return format!("{}\n", heading.trim_end());
    }
    let covered_ms = summary.covered_ms.max(1);
    let mut table = vec![COLUMNS.map(str::to_owned).to_vec()];
    table.extend(ranked.iter().map(|row| cells(row, covered_ms)));
    heading + &align(&table)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::store::decode;

    fn row(name: &str, peak_cpu_permille: i64, peak_mem: i64) -> Row {
        Row {
            name: name.to_owned(),
            usage: Usage {
                peak_cpu_permille,
                peak_mem,
                peak_procs: 1,
                ..Usage::default()
            },
        }
    }

    fn names(rows: &[Row]) -> Vec<&str> {
        rows.iter().map(|r| r.name.as_str()).collect()
    }

    fn options(dir: Option<PathBuf>) -> ReportOptions {
        ReportOptions {
            number: 10,
            order: Order::Desc,
            sort: SortKey::PeakRam,
            period_ms: 3_600_000,
            pattern: None,
            threshold: None,
            dir,
        }
    }

    #[test]
    fn ranking_by_peak_ram_descends_and_breaks_ties_by_peak_cpu_then_name() {
        let rows = vec![
            row("a", 5, 100),
            row("b", 9, 100),
            row("c", 1, 300),
            row("d", 9, 100),
        ];
        assert_eq!(
            names(&rank(rows, SortKey::PeakRam, Order::Desc, 3)),
            ["c", "b", "d"]
        );
    }

    #[test]
    fn ranking_by_cpu_ascending_puts_the_lowest_peak_first() {
        let rows = vec![row("a", 5, 100), row("b", 9, 100), row("c", 1, 300)];
        assert_eq!(
            names(&rank(rows, SortKey::PeakCpu, Order::Asc, 10)),
            ["c", "a", "b"]
        );
    }

    #[test]
    fn ranking_follows_peaks_not_totals_or_averages() {
        let mut steady = row("steady", 100, 100);
        steady.usage.cpu_ns = 1_000_000_000_000;
        steady.usage.mem_byte_ms = 1_000_000_000_000;
        let rows = vec![steady, row("spiky", 900, 900)];
        assert_eq!(
            names(&rank(rows.clone(), SortKey::PeakCpu, Order::Desc, 1)),
            ["spiky"]
        );
        assert_eq!(
            names(&rank(rows, SortKey::PeakRam, Order::Desc, 1)),
            ["spiky"]
        );
    }

    #[test]
    fn ranking_by_averages_and_totals_follows_what_was_used_over_the_period() {
        let mut steady = row("steady", 100, 100);
        steady.usage.cpu_ns = 50;
        steady.usage.mem_byte_ms = 50;
        let mut spiky = row("spiky", 900, 900);
        spiky.usage.cpu_ns = 10;
        spiky.usage.mem_byte_ms = 10;
        spiky.usage.peak_procs = 3;
        let rows = vec![spiky, steady];
        for key in [SortKey::AvgRam, SortKey::AvgCpu, SortKey::CpuTime] {
            assert_eq!(names(&rank(rows.clone(), key, Order::Desc, 1)), ["steady"]);
        }
        assert_eq!(
            names(&rank(rows, SortKey::Procs, Order::Desc, 1)),
            ["spiky"]
        );
    }

    #[test]
    fn summaries_add_usage_and_keep_the_largest_peaks() {
        let buckets = decode(
            "B\t1\t60000\t1000\t4\nG\t2000000000\t2000\t50000\t50\t3\tweb\n\
             B\t2\t120000\t1000\t8\nG\t500000000\t500\t80000\t80\t5\tweb\n",
        );
        let summary = summarize(&buckets);
        assert_eq!(summary.covered_ms, 2000);
        assert_eq!(summary.cpus, 8);
        let web = summary.rows[0].usage;
        assert_eq!(web.cpu_ns, 2_500_000_000);
        assert_eq!(web.peak_cpu_permille, 2000);
        assert_eq!(web.mem_byte_ms, 130_000);
        assert_eq!(web.peak_mem, 80);
        assert_eq!(web.peak_procs, 5);
    }

    #[test]
    fn the_table_aligns_names_left_and_numbers_right() {
        let summary = summarize(&decode(
            "B\t1\t60000\t60000\t4\n\
             G\t30000000000\t900\t60000000000\t2000000\t2\tfirefox\n\
             G\t0\t0\t6000000\t100\t1\tsh\n",
        ));
        let opts = options(None);
        let ranked = rank(summary.rows.clone(), opts.sort, opts.order, opts.number);
        assert_eq!(
            render(&summary, &ranked, &opts),
            "Past 1h: 1m recorded, 4 CPUs (100% CPU = one core)\n\
             Top 2 of 2 programs by peak RAM, highest first\n\
             \n\
             PROGRAM  PROCS  CPU TIME  AVG CPU  PEAK CPU    AVG RAM  PEAK RAM\n\
             firefox      2       30s    50.0%     90.0%  976.6 KiB   1.9 MiB\n\
             sh           1       0ms     0.0%      0.0%      100 B     100 B\n"
        );
    }

    #[test]
    fn the_heading_names_cpu_ranking_from_the_lowest() {
        let summary = summarize(&[]);
        let opts = ReportOptions {
            sort: SortKey::PeakCpu,
            order: Order::Asc,
            ..options(None)
        };
        assert!(
            heading(&summary, 0, &opts).contains("Top 0 of 0 programs by peak CPU, lowest first")
        );
    }

    #[test]
    fn patterns_match_any_part_of_a_name_ignoring_case() {
        let summary = summarize(&decode(
            "B\t1\t60000\t60000\t4\n\
             G\t0\t0\t60000\t1\t1\tfirefox-bin\n\
             G\t0\t0\t60000\t1\t1\tFirefox\n\
             G\t0\t0\t60000\t1\t1\tfire\n",
        ));
        let opts = ReportOptions {
            pattern: Some("FIREFO".to_owned()),
            ..options(None)
        };
        assert_eq!(
            names(&summary.filtered(&opts).rows),
            ["Firefox", "firefox-bin"]
        );
    }

    #[test]
    fn thresholds_keep_programs_on_their_side_of_the_limit() {
        let summary = summarize(&decode(
            "B\t1\t60000\t60000\t4\n\
             G\t0\t0\t1000\t2000\t1\tbig\n\
             G\t0\t0\t9000000\t1000\t1\tsmall\n",
        ));
        let opts = ReportOptions {
            threshold: Threshold::parse("<0.001mb", SortKey::PeakRam),
            ..options(None)
        };
        let kept = summary.filtered(&opts);
        assert_eq!(names(&kept.rows), ["small"]);
        assert!(heading(&kept, 1, &opts).contains(
            "Top 1 of 1 programs by peak RAM, highest first, with peak RAM below 1.0 KiB\n"
        ));
    }

    #[test]
    fn a_report_with_no_program_to_show_has_no_table() {
        let summary = summarize(&[]);
        let opts = ReportOptions {
            pattern: Some("zzz".to_owned()),
            ..options(None)
        };
        assert_eq!(
            render(&summary, &[], &opts),
            "Past 1h: 0ms recorded, 1 CPUs (100% CPU = one core)\n\
             Top 0 of 0 programs by peak RAM, highest first, matching `zzz`\n"
        );
    }

    #[test]
    fn the_heading_names_the_pattern_then_the_threshold() {
        let opts = ReportOptions {
            pattern: Some("fire".to_owned()),
            threshold: Threshold::parse(">50%", SortKey::PeakCpu),
            sort: SortKey::PeakCpu,
            ..options(None)
        };
        assert!(heading(&summarize(&[]), 0, &opts).contains(
            "programs by peak CPU, highest first, matching `fire`, with peak CPU above 50%\n"
        ));
    }

    #[test]
    fn a_report_over_an_empty_folder_says_how_to_start_recording() {
        let dir = std::env::temp_dir().join(format!("pw-report-empty-{}", std::process::id()));
        let err = report(&options(Some(dir))).unwrap_err().to_string();
        assert!(
            err.starts_with("nothing recorded in the past 1h under "),
            "{err}"
        );
        assert!(err.ends_with(
            "start the recorder with `pw record` (see README for running it as a service)"
        ));
    }

    #[test]
    fn a_report_shows_what_was_recorded_this_hour() {
        let dir = std::env::temp_dir().join(format!("pw-report-recent-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let minute = store::now_ms() / 60_000;
        let text = format!(
            "B\t{minute}\t{}\t60000\t2\nG\t1\t1\t60000\t1\t1\tediting\n",
            minute * 60_000
        );
        fs::write(dir.join("current.tsv"), text).unwrap();
        let shown = report(&options(Some(dir.clone()))).unwrap();
        fs::remove_dir_all(&dir).unwrap();
        assert!(shown.contains("\nediting "), "{shown}");
    }
}
