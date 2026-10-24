//! Timestamps: Git's display formats and the local UTC offset.

use anyhow::{Result, bail};

use crate::object::{Signature, parse_tz};

const DAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/// Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

struct Parts {
    year: i64,
    month: u32,
    day: u32,
    hour: i64,
    min: i64,
    sec: i64,
    weekday: usize,
}

fn parts(time: i64, tz_minutes: i32) -> Parts {
    let local = time + tz_minutes as i64 * 60;
    let days = local.div_euclid(86_400);
    let secs = local.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    Parts { year, month, day, hour: secs / 3600, min: secs / 60 % 60, sec: secs % 60, weekday: days.rem_euclid(7) as usize }
}

/// Git's default format: "Thu Oct 24 20:57:00 2026 +0530", in the signature's own timezone.
pub fn format_default(sig: &Signature) -> String {
    let p = parts(sig.time, sig.tz);
    format!(
        "{} {} {} {:02}:{:02}:{:02} {} {}",
        DAYS[p.weekday],
        MONTHS[p.month as usize - 1],
        p.day,
        p.hour,
        p.min,
        p.sec,
        p.year,
        sig.tz_string()
    )
}

/// ISO-8601-like, as `--date=iso`: "2026-10-24 20:57:00 +0530".
pub fn format_iso(sig: &Signature) -> String {
    let p = parts(sig.time, sig.tz);
    format!("{}-{:02}-{:02} {:02}:{:02}:{:02} {}", p.year, p.month, p.day, p.hour, p.min, p.sec, sig.tz_string())
}

/// "3 days ago" style, as `--date=relative`.
pub fn format_relative(time: i64, now: i64) -> String {
    let d = now - time;
    let unit = |n: i64, what: &str| format!("{n} {what}{} ago", if n == 1 { "" } else { "s" });
    match d {
        i64::MIN..=-1 => "in the future".to_string(),
        0..=89 => unit(d, "second"),
        90..=5399 => unit((d + 30) / 60, "minute"),
        5400..=129_599 => unit((d + 1800) / 3600, "hour"),
        129_600..=1_209_599 => unit((d + 43_200) / 86_400, "day"),
        1_209_600..=5_183_999 => unit((d + 302_400) / 604_800, "week"),
        5_184_000..=31_535_999 => unit((d + 1_296_000) / 2_592_000, "month"),
        _ => unit((d + 15_768_000) / 31_536_000, "year"),
    }
}

pub fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// The machine's current offset from UTC in minutes.
pub fn local_offset(time: i64) -> i32 {
    let t = time as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: localtime_r only writes into `tm`, which is a valid, owned struct.
    let ok = unsafe { !libc::localtime_r(&t, &mut tm).is_null() };
    if ok { (tm.tm_gmtoff / 60) as i32 } else { 0 }
}

/// Parse a date given in GIT_AUTHOR_DATE / GIT_COMMITTER_DATE: "<unix> <tz>", "@<unix> <tz>",
/// or "YYYY-MM-DD[T ]HH:MM:SS <tz>".
pub fn parse_env_date(s: &str) -> Result<(i64, i32)> {
    let s = s.trim();
    let (main, tz) = match s.rsplit_once(' ') {
        Some((m, tz)) if tz.starts_with(['+', '-']) => (m.trim(), parse_tz(tz)?),
        _ => (s, 0),
    };
    let main = main.strip_prefix('@').unwrap_or(main);
    if let Ok(t) = main.parse::<i64>() {
        return Ok((t, tz));
    }
    let b = main.as_bytes();
    if b.len() == 19 && b[4] == b'-' && b[7] == b'-' && (b[10] == b'T' || b[10] == b' ') {
        let num = |r: std::ops::Range<usize>| main[r].parse::<i64>();
        let (y, mo, d) = (num(0..4)?, num(5..7)? as u32, num(8..10)? as u32);
        let (h, mi, se) = (num(11..13)?, num(14..16)?, num(17..19)?);
        let local = days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + se;
        return Ok((local - tz as i64 * 60, tz));
    }
    bail!("unsupported date format {s:?}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sig(time: i64, tz: i32) -> Signature {
        Signature { name: String::new(), email: String::new(), time, tz }
    }

    #[test]
    fn default_format_matches_git() {
        // `git log` shows 1112911993 +0200 as:
        assert_eq!(format_default(&sig(1_112_911_993, 120)), "Fri Apr 8 00:13:13 2005 +0200");
        assert_eq!(format_default(&sig(0, 0)), "Thu Jan 1 00:00:00 1970 +0000");
        assert_eq!(format_default(&sig(1_798_000_000, -480)), "Tue Dec 22 20:26:40 2026 -0800");
        assert_eq!(format_iso(&sig(1_112_911_993, 120)), "2005-04-08 00:13:13 +0200");
    }

    #[test]
    fn civil_roundtrip() {
        for days in [-1000, -1, 0, 59, 60, 365, 11_016, 20_000, 60_000] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days);
        }
    }

    #[test]
    fn env_dates() {
        assert_eq!(parse_env_date("1700000000 +0530").unwrap(), (1_700_000_000, 330));
        assert_eq!(parse_env_date("@1700000000 -0100").unwrap(), (1_700_000_000, -60));
        assert_eq!(parse_env_date("2005-04-08T00:13:13 +0200").unwrap(), (1_112_911_993, 120));
        assert!(parse_env_date("yesterday").is_err());
    }

    #[test]
    fn relative() {
        assert_eq!(format_relative(100, 130), "30 seconds ago");
        assert_eq!(format_relative(0, 3 * 86_400), "3 days ago");
        assert_eq!(format_relative(0, 400 * 86_400), "1 year ago");
    }
}
