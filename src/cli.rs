use std::path::PathBuf;

use crate::recorder::RecordOptions;
use crate::report::{Order, ReportOptions, SortKey};
use crate::span::{self, DAY_MS, HOUR_MS, SECOND_MS};
use crate::threshold::Threshold;

/// The longest the recorder keeps recordings, and so the longest period a report covers.
const MAX_RETAIN_MS: i64 = 30 * DAY_MS;

/// The text `--help` prints.
pub const USAGE: &str = "\
pw - top programs by peak RAM or peak CPU over the past hours

Usage:
  pw [OPTIONS] [PATTERN]  report on what the recorder saw
  pw --clear [-d PATH]    delete every recording
  pw record [OPTIONS]     sample every process until stopped

Report options:
  PATTERN                show only programs whose name contains it, in any case
  -n, --number N         programs to show (default 10)
  -s, --sort ram|cpu     rank by peak RAM or peak CPU (default ram)
  -o, --order desc|asc   highest or lowest first (default desc)
  -p, --period SPAN      how far back to look, e.g. 6h, 2d, up to 30d (default 1h)
  -t, --threshold LIMIT  show only programs whose peak is above (>) or below
                         (<) a limit: peak RAM in mb, gb, or tb, e.g. '>1gb',
                         or with --sort cpu, peak CPU, e.g. '>200%'. Quote
                         it: a shell takes a bare > or < as a redirection

Record options:
  -i, --interval SPAN    time between samples, e.g. 5s, 30s (default 5s)
  -r, --retain SPAN      how long recordings are kept, up to 30d (default 12h)

Both:
  -d, --dir PATH         data folder (default: PROCESS_WATCH_DIR, else a `data`
                         folder beside pw if there is one, else the per-user
                         state folder)
  -h, --help             show this help
  -V, --version          show the version

No operating system keeps peak CPU or programs that have exited, so reports
come from what `pw record` saw; run it as a service. It can be stopped
at any moment, even with SIGKILL: every sample also leaves the minute in
progress on disk. Recordings older than a day are kept per hour rather than
per minute.

A program is a process together with every descendant it started that did
not begin a program of its own, so a browser's helper processes and a build
tool's compilers count toward the browser and the build tool.
";

/// What the command line asks for.
#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Report(ReportOptions),
    Record(RecordOptions),
    Clear(Option<PathBuf>),
    Help,
    Version,
}

/// Which of pw's two modes the arguments run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Report,
    Record,
}

impl Mode {
    fn command_name(self) -> &'static str {
        match self {
            Mode::Report => "pw",
            Mode::Record => "pw record",
        }
    }
}

/// A flag that takes a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flag {
    Number,
    Sort,
    Order,
    Period,
    Threshold,
    Interval,
    Retain,
    Dir,
}

impl Flag {
    fn named(name: &str) -> Option<Self> {
        let flag = match name {
            "-n" | "--number" => Flag::Number,
            "-s" | "--sort" => Flag::Sort,
            "-o" | "--order" => Flag::Order,
            "-p" | "--period" => Flag::Period,
            "-t" | "--threshold" => Flag::Threshold,
            "-i" | "--interval" => Flag::Interval,
            "-r" | "--retain" => Flag::Retain,
            "-d" | "--dir" => Flag::Dir,
            _ => return None,
        };
        Some(flag)
    }

    fn mode(self) -> Option<Mode> {
        match self {
            Flag::Number | Flag::Sort | Flag::Order | Flag::Period | Flag::Threshold => {
                Some(Mode::Report)
            }
            Flag::Interval | Flag::Retain => Some(Mode::Record),
            Flag::Dir => None,
        }
    }

    fn is_taken_by(self, mode: Mode) -> bool {
        self.mode().is_none_or(|own| own == mode)
    }
}

/// The options gathered so far, for whichever mode the arguments run.
struct Parsed {
    mode: Mode,
    report: ReportOptions,
    record: RecordOptions,
    dir: Option<PathBuf>,
    /// The threshold flag as given and its value, read once the sort is known.
    threshold: Option<(String, String)>,
    clear: bool,
    /// The last report option given, which `--clear` refuses.
    report_flag: Option<String>,
}

impl Parsed {
    fn new(mode: Mode) -> Self {
        Self {
            mode,
            report: ReportOptions {
                number: 10,
                order: Order::Desc,
                sort: SortKey::Ram,
                period_ms: HOUR_MS,
                pattern: None,
                threshold: None,
                dir: None,
            },
            record: RecordOptions {
                interval_ms: 5 * SECOND_MS,
                retain_ms: 12 * HOUR_MS,
                dir: None,
            },
            dir: None,
            threshold: None,
            clear: false,
            report_flag: None,
        }
    }

    /// Reads one argument: a flag, or in a report, the pattern.
    fn take(&mut self, arg: String, rest: &mut impl Iterator<Item = String>) -> Result<(), String> {
        if self.mode == Mode::Report && !arg.starts_with('-') {
            return self.take_pattern(arg);
        }
        self.take_flag(&arg, rest)
    }

    fn take_pattern(&mut self, pattern: String) -> Result<(), String> {
        if let Some(first) = &self.report.pattern {
            return Err(format!(
                "pw takes one pattern; got `{first}` and `{pattern}`"
            ));
        }
        self.report_flag = Some(pattern.clone());
        self.report.pattern = Some(pattern);
        Ok(())
    }

    /// Reads the flag in `arg`, taking its value from `arg` itself or from the next argument.
    fn take_flag(
        &mut self,
        arg: &str,
        rest: &mut impl Iterator<Item = String>,
    ) -> Result<(), String> {
        let (name, inline) = split_flag(arg);
        let flag = Flag::named(name)
            .filter(|flag| flag.is_taken_by(self.mode))
            .ok_or_else(|| {
                format!(
                    "{} does not take `{arg}`; see --help",
                    self.mode.command_name()
                )
            })?;
        let value = match inline {
            Some(value) => value.to_owned(),
            None => rest.next().ok_or_else(|| format!("{name} needs a value"))?,
        };
        if flag.mode() == Some(Mode::Report) {
            self.report_flag = Some(name.to_owned());
        }
        self.apply(flag, name, &value)
    }

    fn apply(&mut self, flag: Flag, name: &str, value: &str) -> Result<(), String> {
        match flag {
            Flag::Number => self.report.number = count_arg(name, value)?,
            Flag::Sort => self.report.sort = sort_arg(name, value)?,
            Flag::Order => self.report.order = order_arg(name, value)?,
            Flag::Period => self.report.period_ms = span_arg(name, value, HOUR_MS, MAX_RETAIN_MS)?,
            Flag::Threshold => self.threshold = Some((name.to_owned(), value.to_owned())),
            Flag::Interval => self.record.interval_ms = span_arg(name, value, SECOND_MS, HOUR_MS)?,
            Flag::Retain => self.record.retain_ms = span_arg(name, value, HOUR_MS, MAX_RETAIN_MS)?,
            Flag::Dir => self.dir = Some(PathBuf::from(value)),
        }
        Ok(())
    }

    fn into_command(self) -> Result<Command, String> {
        if self.clear {
            return match self.report_flag {
                Some(flag) => Err(format!("--clear takes no report options; got `{flag}`")),
                None => Ok(Command::Clear(self.dir)),
            };
        }
        Ok(match self.mode {
            Mode::Report => Command::Report(ReportOptions {
                threshold: self
                    .threshold
                    .map(|(flag, value)| threshold_arg(&flag, &value, self.report.sort))
                    .transpose()?,
                dir: self.dir,
                ..self.report
            }),
            Mode::Record => Command::Record(RecordOptions {
                dir: self.dir,
                ..self.record
            }),
        })
    }
}

/// Parses the arguments after the program name.
pub fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Command, String> {
    let mut args = args.into_iter().peekable();
    let mode = match args.next_if(|a| a == "record") {
        Some(_) => Mode::Record,
        None => Mode::Report,
    };
    let mut parsed = Parsed::new(mode);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(Command::Help),
            "-V" | "--version" => return Ok(Command::Version),
            "--clear" if mode == Mode::Report => parsed.clear = true,
            "--" if mode == Mode::Report => {
                for pattern in args.by_ref() {
                    parsed.take_pattern(pattern)?;
                }
            }
            _ => parsed.take(arg, &mut args)?,
        }
    }
    parsed.into_command()
}

/// A flag's name and the value written into the same argument, if any:
/// `--flag=value` and `-fvalue` carry one, `--flag` and `-f` do not.
fn split_flag(arg: &str) -> (&str, Option<&str>) {
    if arg.starts_with("--") {
        return match arg.split_once('=') {
            Some((name, value)) => (name, Some(value)),
            None => (arg, None),
        };
    }
    if arg.starts_with('-') && arg.len() > 2 && arg.is_char_boundary(2) {
        return (&arg[..2], Some(&arg[2..]));
    }
    (arg, None)
}

fn count_arg(flag: &str, value: &str) -> Result<usize, String> {
    value
        .parse()
        .ok()
        .filter(|&n| n > 0)
        .ok_or_else(|| format!("{flag} takes a positive whole number; got `{value}`"))
}

fn sort_arg(flag: &str, value: &str) -> Result<SortKey, String> {
    match value.to_ascii_lowercase().as_str() {
        "ram" | "mem" | "memory" => Ok(SortKey::Ram),
        "cpu" => Ok(SortKey::Cpu),
        _ => Err(format!("{flag} takes ram or cpu; got `{value}`")),
    }
}

fn order_arg(flag: &str, value: &str) -> Result<Order, String> {
    match value.to_ascii_lowercase().as_str() {
        "desc" => Ok(Order::Desc),
        "asc" => Ok(Order::Asc),
        _ => Err(format!("{flag} takes desc or asc; got `{value}`")),
    }
}

fn threshold_arg(flag: &str, value: &str, sort: SortKey) -> Result<Threshold, String> {
    Threshold::parse(value, sort).ok_or_else(|| match sort {
        SortKey::Ram => {
            format!("{flag} takes > or < and a size in mb, gb, or tb, such as >1gb; got `{value}`")
        }
        SortKey::Cpu => format!(
            "{flag} with --sort cpu takes > or < and a percentage, such as >200%; got `{value}`"
        ),
    })
}

fn span_arg(flag: &str, value: &str, min_ms: i64, max_ms: i64) -> Result<i64, String> {
    let ms = span::parse(value).ok_or_else(|| {
        format!("{flag} takes a span with units, such as 90s, 30m, 2h, or 1d; got `{value}`")
    })?;
    if ms < min_ms {
        return Err(format!("{flag} must be at least {}", span::format(min_ms)));
    }
    if ms > max_ms {
        return Err(format!("{flag} can be at most {}", span::format(max_ms)));
    }
    Ok(ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Command, String> {
        parse_args(args.iter().map(|a| (*a).to_owned()))
    }

    fn report_of(args: &[&str]) -> ReportOptions {
        match parse(args) {
            Ok(Command::Report(opts)) => opts,
            other => panic!("expected a report, got {other:?}"),
        }
    }

    #[test]
    fn report_defaults_to_top_ten_by_ram_descending_over_an_hour() {
        let opts = report_of(&[]);
        assert_eq!(opts.number, 10);
        assert_eq!(opts.sort, SortKey::Ram);
        assert_eq!(opts.order, Order::Desc);
        assert_eq!(opts.period_ms, 3_600_000);
        assert_eq!(opts.pattern, None);
        assert_eq!(opts.threshold, None);
        assert_eq!(opts.dir, None);
    }

    #[test]
    fn report_flags_take_short_long_and_inline_values() {
        let opts = report_of(&[
            "-n",
            "5",
            "--sort=cpu",
            "-oasc",
            "--period",
            "2h",
            "-d",
            "/x",
        ]);
        assert_eq!(opts.number, 5);
        assert_eq!(opts.sort, SortKey::Cpu);
        assert_eq!(opts.order, Order::Asc);
        assert_eq!(opts.period_ms, 7_200_000);
        assert_eq!(opts.dir, Some(PathBuf::from("/x")));
    }

    #[test]
    fn record_defaults_to_five_second_samples_kept_twelve_hours() {
        let expected = RecordOptions {
            interval_ms: 5_000,
            retain_ms: 43_200_000,
            dir: None,
        };
        assert_eq!(parse(&["record"]), Ok(Command::Record(expected)));
    }

    #[test]
    fn record_takes_its_own_flags_only() {
        let expected = RecordOptions {
            interval_ms: 2_000,
            retain_ms: 7_200_000,
            dir: Some(PathBuf::from("/x")),
        };
        assert_eq!(
            parse(&["record", "-i2s", "--retain=2h", "--dir", "/x"]),
            Ok(Command::Record(expected))
        );
        assert_eq!(
            parse(&["record", "-n", "3"]),
            Err("pw record does not take `-n`; see --help".to_owned())
        );
        assert_eq!(
            parse(&["-i", "3s"]),
            Err("pw does not take `-i`; see --help".to_owned())
        );
    }

    #[test]
    fn a_month_is_the_longest_period_and_retention() {
        assert_eq!(report_of(&["-p", "30d"]).period_ms, 30 * 86_400_000);
        let Ok(Command::Record(opts)) = parse(&["record", "-r", "30d"]) else {
            panic!("expected record");
        };
        assert_eq!(opts.retain_ms, 30 * 86_400_000);
    }

    #[test]
    fn clear_takes_only_a_folder() {
        assert_eq!(parse(&["--clear"]), Ok(Command::Clear(None)));
        assert_eq!(
            parse(&["--clear", "-d", "/x"]),
            Ok(Command::Clear(Some(PathBuf::from("/x"))))
        );
        assert_eq!(
            parse(&["--clear", "-n", "3"]),
            Err("--clear takes no report options; got `-n`".to_owned())
        );
        assert_eq!(
            parse(&["record", "--clear"]),
            Err("pw record does not take `--clear`; see --help".to_owned())
        );
    }

    fn error_of(args: &[&str]) -> String {
        parse(args).err().unwrap_or_default()
    }

    #[test]
    fn bad_report_values_are_rejected_with_the_flag_named() {
        assert_eq!(
            error_of(&["-n", "0"]),
            "-n takes a positive whole number; got `0`"
        );
        assert_eq!(
            error_of(&["--sort", "disk"]),
            "--sort takes ram or cpu; got `disk`"
        );
        assert_eq!(error_of(&["-o", "up"]), "-o takes desc or asc; got `up`");
        assert_eq!(
            error_of(&["-p", "24"]),
            "-p takes a span with units, such as 90s, 30m, 2h, or 1d; got `24`"
        );
        assert_eq!(error_of(&["-p", "30m"]), "-p must be at least 1h");
        assert_eq!(error_of(&["--period=31d"]), "--period can be at most 30d");
        assert_eq!(error_of(&["-n"]), "-n needs a value");
        assert_eq!(error_of(&["-x"]), "pw does not take `-x`; see --help");
    }

    #[test]
    fn bad_record_values_are_rejected_with_the_flag_named() {
        assert_eq!(
            error_of(&["record", "-i", "500ms"]),
            "-i takes a span with units, such as 90s, 30m, 2h, or 1d; got `500ms`"
        );
        assert_eq!(error_of(&["record", "-i", "2h"]), "-i can be at most 1h");
        assert_eq!(error_of(&["record", "-r", "30m"]), "-r must be at least 1h");
        assert_eq!(error_of(&["record", "-r", "31d"]), "-r can be at most 30d");
    }

    #[test]
    fn sort_and_order_values_ignore_case_and_take_memory_as_ram() {
        let opts = report_of(&["--sort", "Memory", "--order", "ASC"]);
        assert_eq!(opts.sort, SortKey::Ram);
        assert_eq!(opts.order, Order::Asc);
    }

    #[test]
    fn a_bare_argument_is_the_pattern_wherever_it_stands() {
        assert_eq!(report_of(&["firefo"]).pattern.as_deref(), Some("firefo"));
        let opts = report_of(&["-n", "3", "Fire", "-s", "cpu"]);
        assert_eq!(opts.pattern.as_deref(), Some("Fire"));
        assert_eq!(opts.number, 3);
        assert_eq!(opts.sort, SortKey::Cpu);
    }

    #[test]
    fn a_double_dash_lets_a_pattern_start_with_a_dash_or_name_record() {
        assert_eq!(report_of(&["--", "-bin"]).pattern.as_deref(), Some("-bin"));
        assert_eq!(
            report_of(&["-n", "2", "--", "record"]).pattern.as_deref(),
            Some("record")
        );
    }

    #[test]
    fn only_a_report_takes_one_pattern() {
        assert_eq!(
            error_of(&["a", "b"]),
            "pw takes one pattern; got `a` and `b`"
        );
        assert_eq!(
            error_of(&["a", "--", "b"]),
            "pw takes one pattern; got `a` and `b`"
        );
        assert_eq!(
            error_of(&["record", "firefox"]),
            "pw record does not take `firefox`; see --help"
        );
        assert_eq!(
            error_of(&["--clear", "firefox"]),
            "--clear takes no report options; got `firefox`"
        );
    }

    #[test]
    fn thresholds_measure_the_peak_the_sort_ranks_by_wherever_it_is_given() {
        let ram = report_of(&["-t", ">1gb"]).threshold.map(|t| t.describe());
        assert_eq!(ram.as_deref(), Some("peak RAM above 1.0 GiB"));
        let cpu = report_of(&["--threshold=<50%", "-s", "cpu"]).threshold;
        assert_eq!(
            cpu.map(|t| t.describe()).as_deref(),
            Some("peak CPU below 50%")
        );
        assert_eq!(
            error_of(&["-s", "cpu", "-t>10m"]),
            "-t with --sort cpu takes > or < and a percentage, such as >200%; got `>10m`"
        );
    }

    #[test]
    fn bad_thresholds_name_what_the_sort_takes() {
        assert_eq!(
            error_of(&["-t", "1gb"]),
            "-t takes > or < and a size in mb, gb, or tb, such as >1gb; got `1gb`"
        );
        assert_eq!(
            error_of(&["--threshold", ">50%"]),
            "--threshold takes > or < and a size in mb, gb, or tb, such as >1gb; got `>50%`"
        );
        assert_eq!(
            error_of(&["-t", ">1gb", "--sort", "cpu"]),
            "-t with --sort cpu takes > or < and a percentage, such as >200%; got `>1gb`"
        );
        assert_eq!(
            error_of(&["--clear", "-t", ">1gb"]),
            "--clear takes no report options; got `-t`"
        );
        assert_eq!(
            error_of(&["record", "-t", ">1gb"]),
            "pw record does not take `-t`; see --help"
        );
    }

    #[test]
    fn help_and_version_win_over_other_arguments() {
        assert_eq!(parse(&["-n", "3", "--help"]), Ok(Command::Help));
        assert_eq!(parse(&["record", "-V"]), Ok(Command::Version));
    }

    #[test]
    fn a_later_flag_overrides_an_earlier_one() {
        assert_eq!(report_of(&["-n", "3", "--number=7"]).number, 7);
    }

    #[test]
    fn flags_split_into_name_and_inline_value() {
        assert_eq!(split_flag("--dir=/a=b"), ("--dir", Some("/a=b")));
        assert_eq!(split_flag("--dir"), ("--dir", None));
        assert_eq!(split_flag("-n5"), ("-n", Some("5")));
        assert_eq!(split_flag("-n"), ("-n", None));
        assert_eq!(split_flag("record"), ("record", None));
    }
}
