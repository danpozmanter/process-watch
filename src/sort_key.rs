use crate::store::Usage;

/// The report column programs are ranked by, which a threshold also compares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    PeakRam,
    PeakCpu,
    AvgRam,
    AvgCpu,
    CpuTime,
    Procs,
}

/// How a column's amounts are written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unit {
    /// Bytes, written as a binary size such as `1gb`.
    Size,
    /// Hundredths of a percent of one core, written as a percentage such as `50%`.
    Percent,
    /// Milliseconds, written as a span such as `10m`.
    Span,
    /// A plain count, such as `20`.
    Count,
}

impl SortKey {
    /// Every key, in the order `--help` lists them.
    pub const ALL: [SortKey; 6] = [
        SortKey::PeakRam,
        SortKey::PeakCpu,
        SortKey::AvgRam,
        SortKey::AvgCpu,
        SortKey::CpuTime,
        SortKey::Procs,
    ];

    /// The key a `--sort` value names, ignoring case; `ram` and `cpu` name the peaks.
    pub fn named(name: &str) -> Option<Self> {
        let key = match name.to_ascii_lowercase().as_str() {
            "peak-ram" | "ram" | "mem" | "memory" => SortKey::PeakRam,
            "peak-cpu" | "cpu" => SortKey::PeakCpu,
            "avg-ram" => SortKey::AvgRam,
            "avg-cpu" => SortKey::AvgCpu,
            "cpu-time" => SortKey::CpuTime,
            "procs" => SortKey::Procs,
            _ => return None,
        };
        Some(key)
    }

    /// The key's `--sort` value.
    pub fn name(self) -> &'static str {
        match self {
            SortKey::PeakRam => "peak-ram",
            SortKey::PeakCpu => "peak-cpu",
            SortKey::AvgRam => "avg-ram",
            SortKey::AvgCpu => "avg-cpu",
            SortKey::CpuTime => "cpu-time",
            SortKey::Procs => "procs",
        }
    }

    /// The measure in words, as a report heading names it.
    pub fn label(self) -> &'static str {
        match self {
            SortKey::PeakRam => "peak RAM",
            SortKey::PeakCpu => "peak CPU",
            SortKey::AvgRam => "average RAM",
            SortKey::AvgCpu => "average CPU",
            SortKey::CpuTime => "CPU time",
            SortKey::Procs => "process count",
        }
    }

    /// The measure in words, as a current report heading names it: over one
    /// moment, a peak and an average are the same amount.
    pub fn label_now(self) -> &'static str {
        match self {
            SortKey::PeakRam | SortKey::AvgRam => "RAM",
            SortKey::PeakCpu | SortKey::AvgCpu => "CPU",
            SortKey::CpuTime => "CPU time",
            SortKey::Procs => "process count",
        }
    }

    /// How a threshold on this key writes its limit.
    pub fn unit(self) -> Unit {
        match self {
            SortKey::PeakRam | SortKey::AvgRam => Unit::Size,
            SortKey::PeakCpu | SortKey::AvgCpu => Unit::Percent,
            SortKey::CpuTime => Unit::Span,
            SortKey::Procs => Unit::Count,
        }
    }

    /// A value that orders programs by this key. Every program in a report
    /// shares one recorded period, so totals order the same as averages.
    pub fn rank_value(self, usage: &Usage) -> i64 {
        match self {
            SortKey::PeakRam => usage.peak_mem,
            SortKey::PeakCpu => usage.peak_cpu_permille,
            SortKey::AvgRam => usage.mem_byte_ms,
            SortKey::AvgCpu | SortKey::CpuTime => usage.cpu_ns,
            SortKey::Procs => usage.peak_procs,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_key_is_named_by_its_own_name_in_any_case() {
        for key in SortKey::ALL {
            assert_eq!(SortKey::named(key.name()), Some(key));
            assert_eq!(SortKey::named(&key.name().to_uppercase()), Some(key));
        }
    }

    #[test]
    fn ram_and_cpu_are_short_for_the_peaks() {
        assert_eq!(SortKey::named("ram"), Some(SortKey::PeakRam));
        assert_eq!(SortKey::named("Memory"), Some(SortKey::PeakRam));
        assert_eq!(SortKey::named("cpu"), Some(SortKey::PeakCpu));
        assert_eq!(SortKey::named("disk"), None);
    }
}
