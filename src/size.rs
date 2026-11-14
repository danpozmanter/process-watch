/// A byte count in binary units with one decimal, such as `512 B` or `1.5 KiB`.
pub fn format(bytes: i64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_use_binary_units_with_one_decimal() {
        assert_eq!(format(0), "0 B");
        assert_eq!(format(512), "512 B");
        assert_eq!(format(1023), "1023 B");
        assert_eq!(format(1536), "1.5 KiB");
        assert_eq!(format(3 * 1024 * 1024 * 1024), "3.0 GiB");
    }

    #[test]
    fn the_largest_unit_is_pebibytes() {
        assert_eq!(format(i64::MAX), "8192.0 PiB");
    }
}
