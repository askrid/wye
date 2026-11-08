//! Durations as people write them, in config and on the command line: a
//! whole number and one unit, `30s`, `10m`, `8h`, `7d`.

use std::time::Duration;

/// How a duration is written, as a literal for `concat!`: in config errors
/// and in the help and errors of options that take one.
#[macro_export]
macro_rules! duration_syntax {
    () => {
        "30s, 10m, 8h, 7d"
    };
}

/// A duration as written: digits, then `s`, `m`, `h`, or `d`. Zero is a
/// duration; a caller that needs one that runs checks.
pub fn parse(text: &str) -> Option<Duration> {
    let (digits, unit) = text.split_at_checked(text.len().checked_sub(1)?)?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let scale = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        _ => return None,
    };
    let n: u64 = digits.parse().ok()?;
    n.checked_mul(scale).map(Duration::from_secs)
}

/// `duration` as [`parse`] reads it back, in its largest whole unit: `7d`,
/// `8h`, `90s`. Below a second, it rounds down.
pub fn show(duration: Duration) -> String {
    match duration.as_secs() {
        0 => "0s".to_owned(),
        s if s % 86_400 == 0 => format!("{}d", s / 86_400),
        s if s % 3600 == 0 => format!("{}h", s / 3600),
        s if s % 60 == 0 => format!("{}m", s / 60),
        s => format!("{s}s"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_read_as_written_and_back() {
        for (text, secs) in [
            ("45s", 45),
            ("90s", 90),
            ("30m", 1800),
            ("8h", 28_800),
            ("7d", 604_800),
            ("0s", 0),
        ] {
            assert_eq!(parse(text), Some(Duration::from_secs(secs)), "{text}");
            assert_eq!(show(Duration::from_secs(secs)), text, "{text}");
        }
        assert_eq!(show(Duration::from_secs(120)), "2m");
        for bad in [
            "", "d", "7", "90", "7w", "-1d", "+1d", "1.5h", "1 h", "h1", "7D", "1é",
        ] {
            assert_eq!(parse(bad), None, "{bad}");
        }
        assert_eq!(parse(&format!("{}d", u64::MAX)), None, "overflow");
    }
}
