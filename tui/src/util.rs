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
