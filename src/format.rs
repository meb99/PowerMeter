//! Number formatting shared by the UI, the overlay and the exports.

/// Formats `value` with an SI prefix and a fixed number of significant
/// digits, e.g. `si(0.01234, "A", 4)` → `"12.34 mA"`.
pub fn si(value: f64, unit: &str, digits: usize) -> String {
    if value.is_nan() {
        return format!("OL {unit}");
    }
    if value.is_infinite() {
        return format!("{}∞ {unit}", if value < 0.0 { "-" } else { "" });
    }
    let (scaled, prefix) = scale(value);
    format!("{} {prefix}{unit}", sig(scaled, digits))
}

/// Picks an SI prefix so the mantissa ends up in [1, 1000).
pub fn scale(value: f64) -> (f64, &'static str) {
    let a = value.abs();
    const PREFIXES: [(f64, &str); 8] =
        [(1e9, "G"), (1e6, "M"), (1e3, "k"), (1.0, ""), (1e-3, "m"), (1e-6, "µ"), (1e-9, "n"), (1e-12, "p")];
    if a == 0.0 {
        return (0.0, "");
    }
    for (f, p) in PREFIXES {
        if a >= f * 0.99995 {
            return (value / f, p);
        }
    }
    (value / 1e-12, "p")
}

/// Formats with `digits` significant digits, keeping trailing zeros so the
/// width of a live readout doesn't jump around.
pub fn sig(value: f64, digits: usize) -> String {
    if value == 0.0 {
        return format!("{:.*}", digits.saturating_sub(1), 0.0);
    }
    let mag = value.abs().log10().floor() as i32;
    let decimals = (digits as i32 - 1 - mag).max(0) as usize;
    format!("{value:.decimals$}")
}

/// Fixed-decimal readout, e.g. `fixed(19.0004, 3)` → `"19.000"`.
pub fn fixed(value: f64, decimals: usize) -> String {
    if value.is_finite() { format!("{value:.decimals$}") } else { "----".into() }
}

/// `mm:ss.s` or `h:mm:ss` for a duration in seconds.
pub fn duration(secs: f64) -> String {
    if secs < 1.0 {
        return format!("{:.0} ms", secs * 1000.0);
    }
    if secs < 60.0 {
        return format!("{secs:.2} s");
    }
    let total = secs as u64;
    let (h, m, s) = (total / 3600, (total / 60) % 60, total % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}") }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn si_prefixes() {
        assert_eq!(si(0.012_34, "A", 4), "12.34 mA");
        assert_eq!(si(19.0, "V", 5), "19.000 V");
        assert_eq!(si(1500.0, "Ω", 4), "1.500 kΩ");
        assert_eq!(si(f64::NAN, "Ω", 4), "OL Ω");
        assert_eq!(si(0.0, "V", 4), "0.000 V");
        assert_eq!(si(-0.5, "A", 3), "-500 mA");
        assert_eq!(si(4.7e-6, "F", 3), "4.70 µF");
    }

    #[test]
    fn durations() {
        assert_eq!(duration(0.08), "80 ms");
        assert_eq!(duration(5.5), "5.50 s");
        assert_eq!(duration(125.0), "2:05");
        assert_eq!(duration(3725.0), "1:02:05");
    }
}
