//! Small pure helpers shared by the CLI and the TUI: formatting, parsing, badges.

use std::time::Duration;

use tvremote_core::{CompatLevel, VerdictLevel};

/// Whether to draw emoji badges (✅/⚠/❌) or plain text (OK/WARN/BAD).
///
/// Emoji are used unless the terminal is the Linux console, the locale is not UTF-8,
/// or `TV_REMOTE_ASCII` is set.
pub fn terminal_supports_emoji() -> bool {
    if std::env::var_os("TV_REMOTE_ASCII").is_some() {
        return false;
    }
    if std::env::var("TERM").is_ok_and(|t| t == "linux" || t == "dumb") {
        return false;
    }
    let locale = ["LC_ALL", "LC_CTYPE", "LANG"]
        .iter()
        .find_map(|v| std::env::var(v).ok().filter(|s| !s.is_empty()))
        .unwrap_or_default()
        .to_ascii_lowercase();
    locale.contains("utf-8") || locale.contains("utf8")
}

/// Badge for a direct-cast compatibility level.
pub fn compat_badge(level: CompatLevel, emoji: bool) -> &'static str {
    match (level, emoji) {
        (CompatLevel::Plays, true) => "✅",
        (CompatLevel::NeedsTvFriendly, true) => "⚠",
        (CompatLevel::Unsupported, true) => "❌",
        (CompatLevel::Plays, false) => "OK",
        (CompatLevel::NeedsTvFriendly, false) => "WARN",
        (CompatLevel::Unsupported, false) => "BAD",
    }
}

/// Badge for a network verdict.
pub fn verdict_badge(level: VerdictLevel, emoji: bool) -> &'static str {
    match (level, emoji) {
        (VerdictLevel::Ok, true) => "✅",
        (VerdictLevel::Risky, true) => "⚠",
        (VerdictLevel::Bad, true) => "❌",
        (VerdictLevel::Ok, false) => "OK",
        (VerdictLevel::Risky, false) => "WARN",
        (VerdictLevel::Bad, false) => "BAD",
    }
}

/// `1.2 GB`, `350 MB`, `12 kB`, `512 B` (decimal units, like file managers).
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "kB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1000.0 && u < UNITS.len() - 1 {
        v /= 1000.0;
        u += 1;
    }
    if u == 0 {
        format!("{bytes} B")
    } else if v >= 100.0 {
        format!("{v:.0} {}", UNITS[u])
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

/// `H:MM:SS` (or `M:SS` below one hour).
pub fn fmt_duration(d: Duration) -> String {
    let s = d.as_secs();
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

/// `12 min ago`, `3 h ago`, `2 days ago`.
pub fn fmt_age(secs: u64) -> String {
    match secs {
        0..=59 => "just now".into(),
        60..=3599 => format!("{} min ago", secs / 60),
        3600..=172_799 => format!("{} h ago", secs / 3600),
        _ => format!("{} days ago", secs / 86_400),
    }
}

/// Parse `HH:MM:SS`, `MM:SS` or `SS` (each part may be fractional in the last field).
pub fn parse_time(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty time".into());
    }
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() > 3 {
        return Err(format!("invalid time '{s}' (use HH:MM:SS)"));
    }
    let mut total = 0.0f64;
    for (i, p) in parts.iter().enumerate() {
        let last = i == parts.len() - 1;
        // Plain digits (and one decimal point in the last field): f64 parsing alone
        // would take `1e2`, `inf` or `nan`.
        let decimal = last && p.matches('.').count() <= 1;
        if !p.chars().any(|c| c.is_ascii_digit())
            || !p.chars().all(|c| c.is_ascii_digit() || (decimal && c == '.'))
        {
            return Err(format!("invalid time '{s}'"));
        }
        let v: f64 = if last {
            p.parse().map_err(|_| format!("invalid time '{s}'"))?
        } else {
            p.parse::<u64>()
                .map_err(|_| format!("invalid time '{s}'"))? as f64
        };
        if !v.is_finite() || v < 0.0 {
            return Err(format!("invalid time '{s}'"));
        }
        // Minutes and seconds fields must be < 60 when a larger unit precedes them.
        if i > 0 && v >= 60.0 {
            return Err(format!("invalid time '{s}': field {p} must be below 60"));
        }
        total = total * 60.0 + v;
    }
    Ok(Duration::from_secs_f64(total))
}

/// Where `seek` goes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SeekArg {
    /// An absolute position.
    To(Duration),
    /// Forward (`+`) or back (`-`) from the current position.
    By { delta: Duration, forward: bool },
    /// A point of the media, 0.0-1.0 (`50%` → 0.5).
    Fraction(f64),
}

/// Parse a `seek` argument: `1:02:03` / `90` (absolute), `+10` / `-1:30` (relative),
/// or `50%` (0-100).
pub fn parse_seek(s: &str) -> Result<SeekArg, String> {
    let s = s.trim();
    if let Some(p) = s.strip_suffix('%') {
        let p = p.trim();
        let plain = p.chars().any(|c| c.is_ascii_digit())
            && p.chars().all(|c| c.is_ascii_digit() || c == '.')
            && p.matches('.').count() <= 1;
        let v: f64 = p
            .parse()
            .ok()
            .filter(|_| plain)
            .ok_or_else(|| format!("invalid percentage '{s}'"))?;
        if !(0.0..=100.0).contains(&v) {
            return Err(format!("invalid percentage '{s}' (0-100)"));
        }
        return Ok(SeekArg::Fraction(v / 100.0));
    }
    let (t, forward) = match (s.strip_prefix('+'), s.strip_prefix('-')) {
        (Some(t), _) => (t, true),
        (_, Some(t)) => (t, false),
        _ => return parse_time(s).map(SeekArg::To),
    };
    let delta = parse_time(t)?;
    // ±0 would only jump back to the last keyframe.
    if delta.is_zero() {
        return Err(format!("'{s}' doesn't move"));
    }
    Ok(SeekArg::By { delta, forward })
}

/// Fraction (0.0-1.0) of a progress bar `width` cells wide starting at column `x`
/// that a click on column `col` points at: the first cell is 0 %, the last 100 %.
pub fn column_fraction(col: u16, x: u16, width: u16) -> f64 {
    if width <= 1 {
        return 0.0;
    }
    let off = col.saturating_sub(x).min(width - 1);
    f64::from(off) / f64::from(width - 1)
}

/// True for `http://` / `https://` sources.
pub fn is_url(s: &str) -> bool {
    let l = s.trim().to_ascii_lowercase();
    l.starts_with("http://") || l.starts_with("https://")
}

/// Mbit/s with one decimal.
pub fn mbps(bps: u64) -> String {
    format!("{:.1} Mbps", bps as f64 / 1e6)
}

/// Shorten `s` to at most `max` characters, with a trailing `…`.
pub fn ellipsize(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let keep: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{keep}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_parsing() {
        assert_eq!(parse_time("1:02:03").unwrap(), Duration::from_secs(3723));
        assert_eq!(parse_time("02:03").unwrap(), Duration::from_secs(123));
        assert_eq!(parse_time("45").unwrap(), Duration::from_secs(45));
        assert_eq!(
            parse_time("0:00:01.5").unwrap(),
            Duration::from_millis(1500)
        );
        assert_eq!(parse_time("100").unwrap(), Duration::from_secs(100));
        assert!(parse_time("1:60:00").is_err());
        assert!(parse_time("a:b").is_err());
        assert!(parse_time("").is_err());
        assert!(parse_time("1:2:3:4").is_err());
        assert!(parse_time("-5").is_err());
        for bad in ["1e2", "inf", "nan", "NaN", "1.2.3", ".", "1:2.5:3", "+5"] {
            assert!(parse_time(bad).is_err(), "{bad}");
        }
        assert_eq!(parse_time("2.5").unwrap(), Duration::from_millis(2500));
        assert_eq!(parse_time(".5").unwrap(), Duration::from_millis(500));
    }

    #[test]
    fn seek_arguments() {
        assert_eq!(parse_seek("1:02:03").unwrap(), SeekArg::To(Duration::from_secs(3723)));
        assert_eq!(parse_seek("90").unwrap(), SeekArg::To(Duration::from_secs(90)));
        assert_eq!(
            parse_seek("+10").unwrap(),
            SeekArg::By { delta: Duration::from_secs(10), forward: true }
        );
        assert_eq!(
            parse_seek("-1:30").unwrap(),
            SeekArg::By { delta: Duration::from_secs(90), forward: false }
        );
        assert_eq!(parse_seek("50%").unwrap(), SeekArg::Fraction(0.5));
        assert_eq!(parse_seek("12.5 %").unwrap(), SeekArg::Fraction(0.125));
        assert!(parse_seek("101%").is_err());
        assert!(parse_seek("-5%").is_err());
        assert!(parse_seek("+").is_err());
        assert!(parse_seek("+0").is_err());
        assert!(parse_seek("-0:00").is_err());
        assert!(parse_seek("+1e1").is_err());
        assert!(parse_seek("inf%").is_err());
        assert!(parse_seek("nan%").is_err());
        assert!(parse_seek("1e1%").is_err());
        assert!(parse_seek("+-3").is_err());
        assert!(parse_seek("abc").is_err());
    }

    #[test]
    fn progress_bar_clicks() {
        assert_eq!(column_fraction(10, 10, 41), 0.0);
        assert_eq!(column_fraction(50, 10, 41), 1.0);
        assert_eq!(column_fraction(30, 10, 41), 0.5);
        assert_eq!(column_fraction(5, 10, 41), 0.0, "left of the bar");
        assert_eq!(column_fraction(90, 10, 41), 1.0, "right of the bar");
        assert_eq!(column_fraction(3, 3, 1), 0.0);
    }

    #[test]
    fn badges() {
        assert_eq!(compat_badge(CompatLevel::Plays, true), "✅");
        assert_eq!(compat_badge(CompatLevel::NeedsTvFriendly, false), "WARN");
        assert_eq!(compat_badge(CompatLevel::Unsupported, false), "BAD");
        assert_eq!(verdict_badge(VerdictLevel::Ok, false), "OK");
        assert_eq!(verdict_badge(VerdictLevel::Bad, true), "❌");
        // Must agree with core's own badges.
        assert_eq!(
            verdict_badge(VerdictLevel::Risky, true),
            VerdictLevel::Risky.badge()
        );
    }

    #[test]
    fn formatting() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1_500), "1.5 kB");
        assert_eq!(human_size(73_400_000_000), "73.4 GB");
        assert_eq!(human_size(350_000_000), "350 MB");
        assert_eq!(fmt_duration(Duration::from_secs(3723)), "1:02:03");
        assert_eq!(fmt_duration(Duration::from_secs(83)), "1:23");
        assert_eq!(fmt_age(30), "just now");
        assert_eq!(fmt_age(7200), "2 h ago");
        assert_eq!(ellipsize("abcdef", 4), "abc…");
        assert_eq!(ellipsize("abc", 4), "abc");
        assert!(is_url("HTTPS://x"));
        assert!(!is_url("/home/x.mkv"));
    }
}
