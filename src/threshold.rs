use crate::size;
use crate::sort_key::{SortKey, Unit};
use crate::span::{self, NS_PER_MS};
use crate::store::Usage;

const MIB: f64 = 1024.0 * 1024.0;

/// Which side of its limit a program must fall on to be shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bound {
    Above,
    Below,
}

/// A limit on one report column that a program must be above or below to be shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Threshold {
    pub bound: Bound,
    pub key: SortKey,
    /// The limit in the key's unit: bytes, hundredths of a percent, milliseconds, or a count.
    pub limit: i64,
}

impl Threshold {
    /// Reads `>` or `<` and an amount in `key`'s unit: a binary size (`1gb` is
    /// 1024 MiB), a percentage of one core, a span, or a count.
    pub fn parse(text: &str, key: SortKey) -> Option<Self> {
        let mut chars = text.chars();
        let bound = match chars.next()? {
            '>' => Bound::Above,
            '<' => Bound::Below,
            _ => return None,
        };
        let amount = chars.as_str().trim();
        let limit = match key.unit() {
            Unit::Size => parse_size(amount)?,
            Unit::Percent => scaled(amount.strip_suffix('%')?.trim_end(), 100.0)?,
            Unit::Span => span::parse(amount)?,
            Unit::Count => i64::from(amount.parse::<u32>().ok()?),
        };
        Some(Self { bound, key, limit })
    }

    /// Whether `usage`, recorded over `covered_ms`, falls on the threshold's side of its limit.
    pub fn admits(&self, usage: &Usage, covered_ms: i64) -> bool {
        let covered_ns = i128::from(covered_ms) * i128::from(NS_PER_MS);
        let limit = i128::from(self.limit);
        // Each measure and its limit are brought to a common scale, so the
        // comparison is exact.
        let (measure, limit) = match self.key {
            SortKey::PeakRam => (i128::from(usage.peak_mem), limit),
            SortKey::AvgRam => (
                i128::from(usage.mem_byte_ms),
                limit * i128::from(covered_ms),
            ),
            SortKey::PeakCpu => (i128::from(usage.peak_cpu_permille) * 10, limit),
            SortKey::AvgCpu => (i128::from(usage.cpu_ns) * 10_000, limit * covered_ns),
            SortKey::CpuTime => (i128::from(usage.cpu_ns), limit * i128::from(NS_PER_MS)),
            SortKey::Procs => (i128::from(usage.peak_procs), limit),
        };
        match self.bound {
            Bound::Above => measure > limit,
            Bound::Below => measure < limit,
        }
    }

    /// The threshold in words, such as `peak RAM above 1.0 GiB`.
    pub fn describe(&self) -> String {
        self.describe_as(self.key.label())
    }

    /// The threshold in words for a current report, such as `RAM above 1.0 GiB`.
    pub fn describe_now(&self) -> String {
        self.describe_as(self.key.label_now())
    }

    fn describe_as(&self, measure: &str) -> String {
        let bound = match self.bound {
            Bound::Above => "above",
            Bound::Below => "below",
        };
        let limit = match self.key.unit() {
            Unit::Size => size::format(self.limit),
            Unit::Percent => format_percent(self.limit),
            Unit::Span => span::format(self.limit),
            Unit::Count => self.limit.to_string(),
        };
        format!("{measure} {bound} {limit}")
    }
}

/// A size such as `1gb`, `1.5GiB`, or `500mb`, in bytes.
fn parse_size(text: &str) -> Option<i64> {
    let split = text.find(|c: char| c.is_ascii_alphabetic())?;
    let (number, unit) = text.split_at(split);
    let scale = match unit.to_ascii_lowercase().as_str() {
        "mb" | "mib" => MIB,
        "gb" | "gib" => MIB * 1024.0,
        "tb" | "tib" => MIB * 1024.0 * 1024.0,
        _ => return None,
    };
    scaled(number.trim_end(), scale)
}

/// A plain decimal such as `12` or `1.5`, multiplied by `scale` and rounded.
fn scaled(number: &str, scale: f64) -> Option<i64> {
    let (whole, fraction) = number.split_once('.').unwrap_or((number, ""));
    let digits_only = |part: &str| part.chars().all(|c| c.is_ascii_digit());
    if whole.is_empty() || !digits_only(whole) || !digits_only(fraction) {
        return None;
    }
    let value = (number.parse::<f64>().ok()? * scale).round();
    // `as` saturates, so a value past i64 is caught by the bound itself.
    (value < i64::MAX as f64).then_some(value as i64)
}

/// Hundredths of a percent as a percentage without trailing zeros, such as `50%` or `12.5%`.
fn format_percent(hundredths: i64) -> String {
    let whole = hundredths / 100;
    match hundredths % 100 {
        0 => format!("{whole}%"),
        tenths if tenths % 10 == 0 => format!("{whole}.{}%", tenths / 10),
        fraction => format!("{whole}.{fraction:02}%"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: i64 = 1 << 30;

    fn parse(key: SortKey, text: &str) -> Option<i64> {
        Threshold::parse(text, key).map(|t| t.limit)
    }

    fn admits(key: SortKey, text: &str, usage: Usage) -> bool {
        Threshold::parse(text, key).unwrap().admits(&usage, 60_000)
    }

    #[test]
    fn sizes_are_binary_in_any_case() {
        let ram = |text| parse(SortKey::PeakRam, text);
        assert_eq!(ram(">1gb"), Some(GIB));
        assert_eq!(ram(">1GB"), Some(GIB));
        assert_eq!(ram("> 1.5 GiB"), Some(GIB + GIB / 2));
        assert_eq!(ram(">500mb"), Some(500 << 20));
        assert_eq!(ram(">2tb"), Some(2 << 40));
        assert_eq!(parse(SortKey::AvgRam, "<1gb"), Some(GIB));
        let below = Threshold::parse("<1gb", SortKey::PeakRam).map(|t| t.bound);
        assert_eq!(below, Some(Bound::Below));
    }

    #[test]
    fn sizes_need_a_bound_a_number_and_a_unit() {
        for text in [
            "1gb",
            "=1gb",
            ">",
            ">gb",
            ">1",
            ">1kb",
            ">1g",
            ">-1gb",
            ">1e3gb",
            ">.5gb",
            ">1.gbx",
            ">inf gb",
            ">50%",
            ">10m",
            "",
            ">99999999tb",
        ] {
            assert_eq!(parse(SortKey::PeakRam, text), None, "{text}");
        }
    }

    #[test]
    fn each_key_takes_its_own_unit() {
        assert_eq!(parse(SortKey::PeakCpu, ">50%"), Some(5000));
        assert_eq!(parse(SortKey::AvgCpu, "<12.5 %"), Some(1250));
        assert_eq!(parse(SortKey::CpuTime, ">1h30m"), Some(5_400_000));
        assert_eq!(parse(SortKey::Procs, ">20"), Some(20));
        assert_eq!(parse(SortKey::PeakCpu, ">1gb"), None);
        assert_eq!(parse(SortKey::PeakCpu, ">50"), None);
        assert_eq!(parse(SortKey::CpuTime, ">50%"), None);
        assert_eq!(parse(SortKey::Procs, ">2.5"), None);
        assert_eq!(parse(SortKey::Procs, ">-1"), None);
    }

    fn usage(peak_cpu_permille: i64, peak_mem: i64) -> Usage {
        Usage {
            peak_cpu_permille,
            peak_mem,
            ..Usage::default()
        }
    }

    #[test]
    fn peaks_compare_strictly() {
        assert!(!admits(SortKey::PeakRam, ">1gb", usage(0, GIB)));
        assert!(admits(SortKey::PeakRam, ">1gb", usage(0, GIB + 1)));
        assert!(!admits(SortKey::PeakRam, "<1gb", usage(0, GIB)));
        assert!(admits(SortKey::PeakRam, "<1gb", usage(0, GIB - 1)));
        assert!(admits(SortKey::PeakCpu, ">200%", usage(2500, 0)));
        assert!(!admits(SortKey::PeakCpu, ">250%", usage(2500, 0)));
        assert!(admits(SortKey::PeakCpu, ">249.95%", usage(2500, 0)));
        assert!(!admits(SortKey::PeakCpu, "<250%", usage(2500, 0)));
    }

    #[test]
    fn averages_and_totals_compare_over_the_recorded_time() {
        // Over a minute: half a GiB on average, and thirty seconds of CPU, 50% of one core.
        let used = Usage {
            cpu_ns: 30_000_000_000,
            mem_byte_ms: GIB / 2 * 60_000,
            peak_procs: 4,
            ..Usage::default()
        };
        assert!(admits(SortKey::AvgRam, ">511mb", used));
        assert!(!admits(SortKey::AvgRam, ">512mb", used));
        assert!(admits(SortKey::AvgCpu, ">49.9%", used));
        assert!(!admits(SortKey::AvgCpu, ">50%", used));
        assert!(admits(SortKey::CpuTime, ">29s", used));
        assert!(!admits(SortKey::CpuTime, ">30s", used));
        assert!(admits(SortKey::Procs, ">3", used));
        assert!(!admits(SortKey::Procs, ">4", used));
    }

    #[test]
    fn thresholds_describe_themselves_in_report_units() {
        let describe = |key, text| Threshold::parse(text, key).unwrap().describe();
        assert_eq!(describe(SortKey::PeakRam, ">1gb"), "peak RAM above 1.0 GiB");
        assert_eq!(describe(SortKey::PeakCpu, "<50%"), "peak CPU below 50%");
        assert_eq!(
            describe(SortKey::AvgCpu, ">12.5%"),
            "average CPU above 12.5%"
        );
        assert_eq!(
            describe(SortKey::AvgRam, "<512mb"),
            "average RAM below 512.0 MiB"
        );
        assert_eq!(describe(SortKey::PeakCpu, ">0.05%"), "peak CPU above 0.05%");
        assert_eq!(describe(SortKey::CpuTime, ">90m"), "CPU time above 1h 30m");
        assert_eq!(describe(SortKey::Procs, ">20"), "process count above 20");
    }
}
