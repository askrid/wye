//! The usage log's file names and the UTC times in them: what the writer
//! renames a full log to, how `wye report` finds those files again, and the
//! dates `wye report --since` reads.
//!
//! A rotated log is `<stem>-<UTC time>.<extension>` beside the log
//! (`usage-20261107T091500Z.jsonl` for `usage.jsonl`), so the rotated files
//! of one log sort by name in the order they were written, and the log
//! itself (`usage.jsonl`, since `.` sorts after `-`) after them.

use std::path::{Path, PathBuf};

const SECS_PER_DAY: u64 = 86_400;

/// The name `log` is renamed to when it is rotated at `unix_secs`.
pub(crate) fn rotated(log: &Path, unix_secs: u64) -> PathBuf {
    let (prefix, suffix) = affixes(log);
    log.with_file_name(format!("{prefix}{}{suffix}", compact(unix_secs)))
}

/// Whether the file `name`, beside `log`, is one of its rotated files:
/// `usage-*.jsonl` for `usage.jsonl`.
pub(crate) fn is_rotated(log: &Path, name: &str) -> bool {
    let (prefix, suffix) = affixes(log);
    name.len() > prefix.len() + suffix.len() && name.starts_with(&prefix) && name.ends_with(&suffix)
}

/// `<stem>-` and `.<extension>` (empty without one) of `log`'s file name.
fn affixes(log: &Path) -> (String, String) {
    let stem = log
        .file_stem()
        .map(|s| s.to_string_lossy())
        .unwrap_or_default();
    let suffix = log
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    (format!("{stem}-"), suffix)
}

/// `unix_secs` as `20261107T091500Z`.
fn compact(unix_secs: u64) -> String {
    let ((y, m, d), (hh, mm, ss)) = civil_time(unix_secs);
    format!("{y:04}{m:02}{d:02}T{hh:02}{mm:02}{ss:02}Z")
}

/// `unix_secs` as `2026-11-07 09:15:00 UTC`.
pub(crate) fn readable(unix_secs: u64) -> String {
    let ((y, m, d), (hh, mm, ss)) = civil_time(unix_secs);
    format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02}:{ss:02} UTC")
}

/// Midnight UTC of the date `YYYY-MM-DD`, in Unix seconds; `None` for
/// anything else, an impossible date (`2026-02-30`), or one before 1970.
pub(crate) fn midnight(date: &str) -> Option<u64> {
    let mut parts = date.split('-');
    let mut part = |len: usize| {
        parts
            .next()
            .filter(|p| p.len() == len && p.bytes().all(|b| b.is_ascii_digit()))
            .and_then(|p| p.parse::<u32>().ok())
    };
    let (y, m, d) = (part(4)?, part(2)?, part(2)?);
    if parts.next().is_some() || !(1..=12).contains(&m) || d == 0 {
        return None;
    }
    let days = u64::try_from(days_from_civil(i64::from(y), m, d)).ok()?;
    // A day past the month's end lands in the next month.
    (civil(days) == (i64::from(y), m, d)).then_some(days * SECS_PER_DAY)
}

/// The UTC date and time of `unix_secs`.
fn civil_time(unix_secs: u64) -> ((i64, u32, u32), (u64, u64, u64)) {
    let secs = unix_secs % SECS_PER_DAY;
    (
        civil(unix_secs / SECS_PER_DAY),
        (secs / 3600, secs / 60 % 60, secs % 60),
    )
}

/// The proleptic Gregorian date of a day count since 1970-01-01
/// (Hinnant's `civil_from_days`).
fn civil(days: u64) -> (i64, u32, u32) {
    // Days since 0000-03-01, in 400-year eras of 146,097 days.
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Hinnant's
/// `days_from_civil`).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let (m, d) = (i64::from(m), i64::from(d));
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_read_as_utc() {
        assert_eq!(compact(0), "19700101T000000Z");
        // 2026-11-07 09:15:00 UTC.
        assert_eq!(compact(1_794_042_900), "20261107T091500Z");
        assert_eq!(readable(1_794_042_900), "2026-11-07 09:15:00 UTC");
        assert_eq!(readable(951_782_400), "2000-02-29 00:00:00 UTC");
        assert_eq!(midnight("2026-11-07"), Some(1_794_009_600));
        assert_eq!(midnight("2000-02-29"), Some(951_782_400));
        assert_eq!(midnight("1970-01-01"), Some(0));
        for bad in [
            "2026-02-29",
            "2026-13-01",
            "2026-11-00",
            "2026-11-31",
            "1969-12-31",
            "2026-1-07",
            "2026-11-07-",
            "26-11-07",
            "2026/11/07",
            "+026-11-07",
            "",
        ] {
            assert_eq!(midnight(bad), None, "{bad}");
        }
        // Every day of four centuries reads back.
        for days in 0..146_097 {
            let (y, m, d) = civil(days);
            assert_eq!(days_from_civil(y, m, d), days as i64);
        }
    }

    #[test]
    fn rotated_files_sort_before_the_log_in_the_order_written() {
        let log = Path::new("/home/a/.wye/usage.jsonl");
        let first = rotated(log, 1_794_042_900);
        assert_eq!(
            first,
            Path::new("/home/a/.wye/usage-20261107T091500Z.jsonl")
        );
        let second = rotated(log, 1_794_042_901);
        let mut names = [log, &second, &first].map(|p| p.file_name().unwrap().to_owned());
        names.sort();
        assert_eq!(
            names,
            [&first, &second, &log.to_path_buf()].map(|p| p.file_name().unwrap().to_owned())
        );
        let name = |p: &Path| p.file_name().unwrap().to_str().unwrap().to_owned();
        assert!(is_rotated(log, &name(&first)));
        for other in [
            "usage.jsonl",
            "usage-.jsonl",
            "wye.log",
            "usage-1.log",
            "usage.jsonl.1",
        ] {
            assert!(!is_rotated(log, other), "{other}");
        }
        // A log without an extension rotates to names without one.
        let bare = Path::new("usage");
        assert_eq!(rotated(bare, 0), Path::new("usage-19700101T000000Z"));
    }
}
