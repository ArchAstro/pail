//! Git's date and identity formatting (date.c, ident.c), for git-compatible output.

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Datelike, FixedOffset, Local, NaiveDate, NaiveDateTime, Offset, TimeZone, Timelike, Utc};

/// A Git timestamp: seconds since the epoch plus the author's zone as written (`+0200` → 200).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GitDate {
    pub secs: i64,
    pub tz: i32,
}

impl GitDate {
    /// Parse `<unix seconds> <+hhmm>`.
    pub fn parse(s: &str) -> Result<GitDate> {
        let (secs, tz) = s.trim().split_once(' ').with_context(|| format!("bad date {s:?}"))?;
        Ok(GitDate { secs: secs.parse()?, tz: tz.parse()? })
    }

    fn offset_secs(tz: i32) -> i32 {
        let m = tz.abs() / 100 * 60 + tz.abs() % 100;
        m * 60 * tz.signum()
    }

    /// Broken-down time in the commit's own zone, or the local zone.
    fn local_time(&self, local: bool) -> (DateTime<FixedOffset>, i32) {
        let utc = Utc.timestamp_opt(self.secs, 0).single().unwrap_or_default();
        if local {
            let l = utc.with_timezone(&Local);
            let off = l.offset().fix();
            let secs = off.local_minus_utc();
            let tz = secs.signum() * ((secs.abs() / 3600) * 100 + (secs.abs() % 3600) / 60);
            (utc.with_timezone(&off), tz)
        } else {
            let off = FixedOffset::east_opt(Self::offset_secs(self.tz)).unwrap_or(FixedOffset::east_opt(0).unwrap());
            (utc.with_timezone(&off), self.tz)
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum DateMode {
    Normal,
    Relative,
    Short,
    Iso,
    IsoStrict,
    Rfc,
    Raw,
    Unix,
    Human,
    Format(String),
}

/// `--date=<mode>`: the mode and whether `-local` was asked for.
pub fn parse_date_mode(s: &str) -> Result<(DateMode, bool)> {
    let (s, local) = match s.strip_suffix("-local") {
        Some(b) => (b, true),
        None if s == "local" => ("default", true),
        None => (s, false),
    };
    if let Some(f) = s.strip_prefix("format:") {
        return Ok((DateMode::Format(f.to_string()), local));
    }
    let m = match s {
        "default" => DateMode::Normal,
        "relative" => DateMode::Relative,
        "short" => DateMode::Short,
        "iso" | "iso8601" => DateMode::Iso,
        "iso-strict" | "iso8601-strict" => DateMode::IsoStrict,
        "rfc" | "rfc2822" => DateMode::Rfc,
        "raw" => DateMode::Raw,
        "unix" => DateMode::Unix,
        "human" => DateMode::Human,
        _ => bail!("unknown date format {s}"),
    };
    Ok((m, local))
}

const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

fn tz_str(tz: i32) -> String {
    format!("{}{:04}", if tz < 0 { '-' } else { '+' }, tz.abs())
}

pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Git's `show_date_relative`.
pub fn relative(secs: i64, now: i64) -> String {
    if now < secs {
        return "in the future".into();
    }
    let plural = |n: i64, unit: &str| format!("{n} {unit}{} ago", if n == 1 { "" } else { "s" });
    let mut diff = now - secs;
    if diff < 90 {
        return plural(diff, "second");
    }
    diff = (diff + 30) / 60;
    if diff < 90 {
        return plural(diff, "minute");
    }
    diff = (diff + 30) / 60;
    if diff < 36 {
        return plural(diff, "hour");
    }
    diff = (diff + 12) / 24;
    if diff < 14 {
        return plural(diff, "day");
    }
    if diff < 70 {
        return plural((diff + 3) / 7, "week");
    }
    if diff < 365 {
        return plural((diff + 15) / 30, "month");
    }
    if diff < 1825 {
        let total_months = (diff * 12 * 2 + 365) / (365 * 2);
        let years = total_months / 12;
        let months = total_months % 12;
        let y = format!("{years} year{}", if years == 1 { "" } else { "s" });
        if months > 0 {
            return format!("{y}, {months} month{} ago", if months == 1 { "" } else { "s" });
        }
        return format!("{y} ago");
    }
    plural((diff + 183) / 365, "year")
}

pub fn format_date(d: GitDate, mode: &DateMode, local: bool) -> String {
    match mode {
        DateMode::Unix => return d.secs.to_string(),
        DateMode::Relative => return relative(d.secs, now_secs()),
        DateMode::Raw if !local => return format!("{} {}", d.secs, tz_str(d.tz)),
        _ => {}
    }
    let (t, tz) = d.local_time(local);
    let (wd, mon) = (WEEKDAYS[t.weekday().num_days_from_sunday() as usize], MONTHS[t.month0() as usize]);
    let hms = format!("{:02}:{:02}:{:02}", t.hour(), t.minute(), t.second());
    match mode {
        DateMode::Raw => format!("{} {}", d.secs, tz_str(tz)),
        DateMode::Short => format!("{:04}-{:02}-{:02}", t.year(), t.month(), t.day()),
        DateMode::Iso => format!("{:04}-{:02}-{:02} {hms} {}", t.year(), t.month(), t.day(), tz_str(tz)),
        DateMode::IsoStrict => {
            let z = if tz == 0 {
                "Z".to_string()
            } else {
                format!("{}{:02}:{:02}", if tz < 0 { '-' } else { '+' }, tz.abs() / 100, tz.abs() % 100)
            };
            format!("{:04}-{:02}-{:02}T{hms}{z}", t.year(), t.month(), t.day())
        }
        DateMode::Rfc => format!("{wd}, {} {mon} {} {hms} {}", t.day(), t.year(), tz_str(tz)),
        DateMode::Format(f) => {
            // Git hands the zone to strftime as the commit's own offset; %Z is empty unless local.
            let f = if local { f.clone() } else { f.replace("%Z", "") };
            let f = f.replace("%z", &tz_str(tz));
            t.format(&f).to_string()
        }
        DateMode::Human => human(d, t, tz, local),
        _ => {
            let base = format!("{wd} {mon} {} {hms} {}", t.day(), t.year());
            if local { base } else { format!("{base} {}", tz_str(tz)) }
        }
    }
}

/// Git's `--date=human`: drop what matches "now" (same year, same day…).
fn human(d: GitDate, t: DateTime<FixedOffset>, tz: i32, local: bool) -> String {
    let now = Utc::now().with_timezone(&Local);
    let diff = now.timestamp() - d.secs;
    let same_tz = local || now.offset().fix().local_minus_utc() == GitDate::offset_secs(tz);
    if (0..12 * 3600).contains(&diff) && same_tz {
        return relative(d.secs, now.timestamp());
    }
    let (wd, mon) = (WEEKDAYS[t.weekday().num_days_from_sunday() as usize], MONTHS[t.month0() as usize]);
    let mut out = String::new();
    let hide_year = t.year() == now.year();
    let within_week = hide_year && (0..6 * 86400).contains(&diff);
    if within_week {
        out.push_str(wd);
        out.push(' ');
    } else {
        out.push_str(&format!("{mon} {}", t.day()));
        if !hide_year {
            out.push_str(&format!(" {}", t.year()));
            if !same_tz {
                out.push_str(&format!(" {}", tz_str(tz)));
            }
            return out;
        }
        out.push(' ');
    }
    out.push_str(&format!("{:02}:{:02}", t.hour(), t.minute()));
    if !same_tz {
        out.push_str(&format!(" {}", tz_str(tz)));
    }
    out
}

/// Split `Name <email>` into its parts.
pub fn split_ident(s: &str) -> (&str, &str) {
    match (s.find('<'), s.rfind('>')) {
        (Some(a), Some(b)) if a < b => (s[..a].trim_end(), &s[a + 1..b]),
        _ => (s.trim(), ""),
    }
}

/// Parse a `--since`/`--until` date: ISO-ish dates, `@<secs>`, `<n> <unit>s ago`,
/// `yesterday`, `now`. (A subset of Git's approxidate.)
pub fn parse_approx(s: &str) -> Result<i64> {
    let s = s.trim();
    let now = Local::now();
    if let Some(n) = s.strip_prefix('@') {
        return Ok(n.parse()?);
    }
    match s {
        "now" => return Ok(now.timestamp()),
        "yesterday" => return Ok(now.timestamp() - 86400),
        _ => {}
    }
    let words: Vec<&str> = s.split(|c: char| c.is_whitespace() || c == '.').filter(|w| !w.is_empty()).collect();
    if words.len() >= 2 && words.last() == Some(&"ago") || (words.len() == 2 && words[0].parse::<i64>().is_ok()) {
        let n: i64 = words[0].parse().with_context(|| format!("bad date {s}"))?;
        let unit = words[1].trim_end_matches('s');
        let secs = match unit {
            "second" | "sec" => 1,
            "minute" | "min" => 60,
            "hour" => 3600,
            "day" => 86400,
            "week" => 7 * 86400,
            "month" => 30 * 86400,
            "year" => 365 * 86400,
            _ => bail!("bad date {s}"),
        };
        return Ok(now.timestamp() - n * secs);
    }
    if let Ok(t) = DateTime::parse_from_rfc3339(s) {
        return Ok(t.timestamp());
    }
    if let Ok(t) = DateTime::parse_from_rfc2822(s) {
        return Ok(t.timestamp());
    }
    for f in ["%Y-%m-%d %H:%M:%S %z", "%Y-%m-%dT%H:%M:%S%z"] {
        if let Ok(t) = DateTime::parse_from_str(s, f) {
            return Ok(t.timestamp());
        }
    }
    for f in ["%Y-%m-%d %H:%M:%S", "%Y-%m-%dT%H:%M:%S", "%Y-%m-%d %H:%M"] {
        if let Ok(t) = NaiveDateTime::parse_from_str(s, f) {
            return Ok(Local.from_local_datetime(&t).earliest().context("bad local time")?.timestamp());
        }
    }
    for f in ["%Y-%m-%d", "%Y/%m/%d", "%m/%d/%Y"] {
        if let Ok(d) = NaiveDate::parse_from_str(s, f) {
            // Git keeps the current time of day for a bare date.
            let t = d.and_time(now.time());
            return Ok(Local.from_local_datetime(&t).earliest().context("bad local time")?.timestamp());
        }
    }
    bail!("unsupported date {s:?}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats() {
        let d = GitDate::parse("1729165696 +0200").unwrap();
        assert_eq!(format_date(d, &DateMode::Normal, false), "Thu Oct 17 13:48:16 2024 +0200");
        assert_eq!(format_date(d, &DateMode::Iso, false), "2024-10-17 13:48:16 +0200");
        assert_eq!(format_date(d, &DateMode::IsoStrict, false), "2024-10-17T13:48:16+02:00");
        assert_eq!(format_date(d, &DateMode::Rfc, false), "Thu, 17 Oct 2024 13:48:16 +0200");
        assert_eq!(format_date(d, &DateMode::Short, false), "2024-10-17");
        let d = GitDate::parse("1000 -0430").unwrap();
        assert_eq!(format_date(d, &DateMode::Raw, false), "1000 -0430");
        assert_eq!(relative(0, 89), "89 seconds ago");
        assert_eq!(relative(0, 86400 * 400), "1 year, 1 month ago");
    }
}
