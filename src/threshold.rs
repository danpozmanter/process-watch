use crate::report::SortKey;
use crate::size;
use crate::store::Usage;

const MIB: f64 = 1024.0 * 1024.0;

/// Which side of its limit a program must fall on to be shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bound {
    Above,
    Below,
}

/// The peak a threshold compares, and its limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Limit {
    /// Peak RAM, in bytes.
    PeakRam(i64),
    /// Peak CPU, in hundredths of a percent of one core.
    PeakCpu(i64),
}

/// A limit a program's peak must be above or below for a report to show it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Threshold {
    pub bound: Bound,
    pub limit: Limit,
}

impl Threshold {
    /// Reads `>1gb` or `<500mb` when ranking by RAM, and `>50%` when ranking
    /// by CPU; sizes are binary, so `1gb` is 1024 MiB.
    pub fn parse(text: &str, sort: SortKey) -> Option<Self> {
        let mut chars = text.chars();
        let bound = match chars.next()? {
            '>' => Bound::Above,
            '<' => Bound::Below,
            _ => return None,
        };
        let amount = chars.as_str().trim();
        let limit = match sort {
            SortKey::Ram => Limit::PeakRam(parse_size(amount)?),
            SortKey::Cpu => {
                let percent = amount.strip_suffix('%')?.trim_end();
                Limit::PeakCpu(scaled(percent, 100.0)?)
            }
        };
        Some(Self { bound, limit })
    }

    /// Whether `usage`'s peak falls on the threshold's side of its limit.
    pub fn admits(&self, usage: &Usage) -> bool {
        // Peak CPU is kept in thousandths of a core, ten times coarser than the limit.
        let (peak, limit) = match self.limit {
            Limit::PeakRam(bytes) => (usage.peak_mem, bytes),
            Limit::PeakCpu(hundredths) => (usage.peak_cpu_permille.saturating_mul(10), hundredths),
        };
        match self.bound {
            Bound::Above => peak > limit,
            Bound::Below => peak < limit,
        }
    }

    /// The threshold in words, such as `peak RAM above 1.0 GiB`.
    pub fn describe(&self) -> String {
        let bound = match self.bound {
            Bound::Above => "above",
            Bound::Below => "below",
        };
        match self.limit {
            Limit::PeakRam(bytes) => format!("peak RAM {bound} {}", size::format(bytes)),
            Limit::PeakCpu(hundredths) => {
                format!("peak CPU {bound} {}", format_percent(hundredths))
            }
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

    fn usage(peak_cpu_permille: i64, peak_mem: i64) -> Usage {
        Usage {
            peak_cpu_permille,
            peak_mem,
            ..Usage::default()
        }
    }

    #[test]
    fn ram_thresholds_take_binary_sizes_in_any_case() {
        let above = |limit| {
            Some(Threshold {
                bound: Bound::Above,
                limit: Limit::PeakRam(limit),
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
    fn cpu_thresholds_take_a_percentage_of_one_core() {
        assert_eq!(cpu(">50%").map(|t| t.limit), Some(Limit::PeakCpu(5000)));
        assert_eq!(cpu("<12.5 %").map(|t| t.limit), Some(Limit::PeakCpu(1250)));
        assert_eq!(cpu(">250%").map(|t| t.limit), Some(Limit::PeakCpu(25_000)));
        assert_eq!(cpu(">1gb"), None);
        assert_eq!(cpu(">10m"), None);
        assert_eq!(cpu(">50"), None);
    }

    #[test]
    fn ram_thresholds_compare_peak_ram_strictly() {
        let above = ram(">1gb").unwrap();
        let below = ram("<1gb").unwrap();
        assert!(!above.admits(&usage(0, GIB)));
        assert!(above.admits(&usage(0, GIB + 1)));
        assert!(!below.admits(&usage(0, GIB)));
        assert!(below.admits(&usage(0, GIB - 1)));
    }

    #[test]
    fn cpu_thresholds_compare_peak_cpu_strictly() {
        let spike = usage(2500, 0);
        assert!(cpu(">200%").unwrap().admits(&spike));
        assert!(!cpu(">250%").unwrap().admits(&spike));
        assert!(cpu(">249.95%").unwrap().admits(&spike));
        assert!(cpu("<250.01%").unwrap().admits(&spike));
        assert!(!cpu("<250%").unwrap().admits(&spike));
    }

    #[test]
    fn thresholds_describe_themselves_in_report_units() {
        assert_eq!(ram(">1gb").unwrap().describe(), "peak RAM above 1.0 GiB");
        assert_eq!(cpu("<50%").unwrap().describe(), "peak CPU below 50%");
        assert_eq!(cpu(">12.5%").unwrap().describe(), "peak CPU above 12.5%");
        assert_eq!(cpu(">0.05%").unwrap().describe(), "peak CPU above 0.05%");
    }
}
