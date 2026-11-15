use crate::report::SortKey;
use crate::size;
use crate::span::{self, NS_PER_MS};
use crate::store::Usage;

const MIB: f64 = 1024.0 * 1024.0;

/// Which side of its limit a program must fall on to be shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bound {
    Above,
    Below,
}

/// The measure a threshold compares, and its limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Limit {
    /// Average RAM, in bytes.
    AvgRam(i64),
    /// Average CPU, in hundredths of a percent of one core.
    AvgCpu(i64),
    /// CPU time over the period, in milliseconds.
    CpuTime(i64),
}

/// A limit a program's usage must be above or below for a report to show it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Threshold {
    pub bound: Bound,
    pub limit: Limit,
}

impl Threshold {
    /// Reads `>1gb` or `<500mb` when ranking by RAM, and `>50%` or `>10m`
    /// when ranking by CPU; sizes are binary, so `1gb` is 1024 MiB.
    pub fn parse(text: &str, sort: SortKey) -> Option<Self> {
        let mut chars = text.chars();
        let bound = match chars.next()? {
            '>' => Bound::Above,
            '<' => Bound::Below,
            _ => return None,
        };
        let amount = chars.as_str().trim();
        let limit = match sort {
            SortKey::Ram => Limit::AvgRam(parse_size(amount)?),
            SortKey::Cpu => match amount.strip_suffix('%') {
                Some(percent) => Limit::AvgCpu(scaled(percent.trim_end(), 100.0)?),
                None => Limit::CpuTime(span::parse(amount)?),
            },
        };
        Some(Self { bound, limit })
    }

    /// Whether `usage`, recorded over `covered_ms`, falls on the threshold's side of its limit.
    pub fn admits(&self, usage: &Usage, covered_ms: i64) -> bool {
        let covered_ns = i128::from(covered_ms) * i128::from(NS_PER_MS);
        let cpu_ns = i128::from(usage.cpu_ns);
        // Each measure and its limit are brought to a common scale, so the
        // comparison is exact.
        let (measure, limit) = match self.limit {
            Limit::AvgRam(bytes) => (
                i128::from(usage.mem_byte_ms / covered_ms),
                i128::from(bytes),
            ),
            Limit::AvgCpu(hundredths) => (cpu_ns * 10_000, i128::from(hundredths) * covered_ns),
            Limit::CpuTime(ms) => (cpu_ns, i128::from(ms) * i128::from(NS_PER_MS)),
        };
        match self.bound {
            Bound::Above => measure > limit,
            Bound::Below => measure < limit,
        }
    }

    /// The threshold in words, such as `average RAM above 1.0 GiB`.
    pub fn describe(&self) -> String {
        let bound = match self.bound {
            Bound::Above => "above",
            Bound::Below => "below",
        };
        match self.limit {
            Limit::AvgRam(bytes) => format!("average RAM {bound} {}", size::format(bytes)),
            Limit::AvgCpu(hundredths) => {
                format!("average CPU {bound} {}", format_percent(hundredths))
            }
            Limit::CpuTime(ms) => format!("CPU time {bound} {}", span::format(ms)),
        }
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

    fn ram(text: &str) -> Option<Threshold> {
        Threshold::parse(text, SortKey::Ram)
    }

    fn cpu(text: &str) -> Option<Threshold> {
        Threshold::parse(text, SortKey::Cpu)
    }

    fn usage(cpu_ns: i64, mem_byte_ms: i64) -> Usage {
        Usage {
            cpu_ns,
            mem_byte_ms,
            ..Usage::default()
        }
    }

    #[test]
    fn ram_thresholds_take_binary_sizes_in_any_case() {
        let above = |limit| {
            Some(Threshold {
                bound: Bound::Above,
                limit: Limit::AvgRam(limit),
            })
        };
        assert_eq!(ram(">1gb"), above(GIB));
        assert_eq!(ram(">1GB"), above(GIB));
        assert_eq!(ram("> 1.5 GiB"), above(GIB + GIB / 2));
        assert_eq!(ram(">500mb"), above(500 << 20));
        assert_eq!(ram(">2tb"), above(2 << 40));
        assert_eq!(ram("<1gb").map(|t| t.bound), Some(Bound::Below));
    }

    #[test]
    fn ram_thresholds_need_a_bound_a_number_and_a_unit() {
        for text in [
            "1gb", "=1gb", ">", ">gb", ">1", ">1kb", ">1g", ">-1gb", ">1e3gb", ">.5gb", ">1.gbx",
            ">inf gb", ">50%", ">10m", "",
        ] {
            assert_eq!(ram(text), None, "{text}");
        }
        assert_eq!(ram(">99999999tb"), None);
    }

    #[test]
    fn cpu_thresholds_take_a_percentage_or_a_cpu_time() {
        assert_eq!(cpu(">50%").map(|t| t.limit), Some(Limit::AvgCpu(5000)));
        assert_eq!(cpu("<12.5 %").map(|t| t.limit), Some(Limit::AvgCpu(1250)));
        assert_eq!(
            cpu(">1h30m").map(|t| t.limit),
            Some(Limit::CpuTime(5_400_000))
        );
        assert_eq!(cpu(">1gb"), None);
        assert_eq!(cpu(">50"), None);
    }

    #[test]
    fn ram_thresholds_compare_average_ram_strictly() {
        let above = ram(">1gb").unwrap();
        let below = ram("<1gb").unwrap();
        let exactly = usage(0, GIB * 60_000);
        let more = usage(0, (GIB + 1) * 60_000);
        assert!(!above.admits(&exactly, 60_000));
        assert!(above.admits(&more, 60_000));
        assert!(!below.admits(&exactly, 60_000));
        assert!(below.admits(&usage(0, GIB * 30_000), 60_000));
    }

    #[test]
    fn cpu_thresholds_compare_average_cpu_or_cpu_time() {
        // Thirty seconds of CPU over a minute is 50% of one core.
        let half = usage(30_000_000_000, 0);
        assert!(!cpu(">50%").unwrap().admits(&half, 60_000));
        assert!(cpu(">49.9%").unwrap().admits(&half, 60_000));
        assert!(cpu("<50.01%").unwrap().admits(&half, 60_000));
        assert!(cpu(">29s").unwrap().admits(&half, 60_000));
        assert!(!cpu(">30s").unwrap().admits(&half, 60_000));
        assert!(cpu("<31s").unwrap().admits(&half, 60_000));
    }

    #[test]
    fn thresholds_describe_themselves_in_report_units() {
        assert_eq!(ram(">1gb").unwrap().describe(), "average RAM above 1.0 GiB");
        assert_eq!(cpu("<50%").unwrap().describe(), "average CPU below 50%");
        assert_eq!(cpu(">12.5%").unwrap().describe(), "average CPU above 12.5%");
        assert_eq!(cpu(">0.05%").unwrap().describe(), "average CPU above 0.05%");
        assert_eq!(cpu(">90m").unwrap().describe(), "CPU time above 1h 30m");
    }
}
