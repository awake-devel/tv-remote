//! Pure formatting / mapping helpers used by the pages (unit-tested, no GTK).

use std::path::Path;
use std::time::Duration;

use tvremote_core::cast::{is_dts, is_tv_safe_audio, HdrKind, MediaInfo, StreamInfo};
use tvremote_core::{CloudState, CompatLevel, ConnectionState, PowerState, VerdictLevel};

/// Kind of a file in the media browser.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    Video,
    Audio,
    Image,
}

impl MediaKind {
    pub fn icon_name(self) -> &'static str {
        match self {
            MediaKind::Video => "video-x-generic-symbolic",
            MediaKind::Audio => "audio-x-generic-symbolic",
            MediaKind::Image => "image-x-generic-symbolic",
        }
    }
}

/// Classify a file by extension (case-insensitive). `None` = not shown in the browser.
pub fn media_kind(path: &Path) -> Option<MediaKind> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "mkv" | "mp4" | "m4v" | "mov" | "avi" | "webm" | "ts" | "m2ts" | "mts" | "wmv" | "mpg"
        | "mpeg" | "flv" | "3gp" | "ogv" | "vob" => MediaKind::Video,
        "mp3" | "flac" | "m4a" | "aac" | "ogg" | "oga" | "opus" | "wav" | "alac" | "wma" | "ac3"
        | "dts" | "mka" | "aiff" | "aif" => MediaKind::Audio,
        "jpg" | "jpeg" | "png" | "gif" | "webp" | "heic" | "heif" | "bmp" => MediaKind::Image,
        _ => return None,
    })
}

/// `1.2 GB`, `350 MB`, `12 kB`, `512 B` (decimal units, like GNOME Files).
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["kB", "MB", "GB", "TB", "PB"];
    if bytes < 1000 {
        return format!("{bytes} B");
    }
    let mut v = bytes as f64 / 1000.0;
    let mut i = 0;
    while v >= 1000.0 && i + 1 < UNITS.len() {
        v /= 1000.0;
        i += 1;
    }
    if v >= 100.0 {
        format!("{v:.0} {}", UNITS[i])
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

/// `1:02:03` or `2:03`.
pub fn fmt_duration(d: Duration) -> String {
    let s = d.as_secs();
    let (h, m, s) = (s / 3600, (s / 60) % 60, s % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// `75.8 Mbps`.
pub fn fmt_mbps(mbps: f64) -> String {
    if mbps >= 100.0 {
        format!("{mbps:.0} Mbps")
    } else {
        format!("{mbps:.1} Mbps")
    }
}

/// bit/s → `75.8 Mbps`.
pub fn fmt_bps(bps: u64) -> String {
    fmt_mbps(bps as f64 / 1e6)
}

/// Age of a measurement: `just now`, `5 min ago`, `3 h ago`, `2 days ago`.
pub fn fmt_age(secs: u64) -> String {
    match secs {
        0..=59 => "just now".into(),
        60..=3599 => format!("{} min ago", secs / 60),
        3600..=86_399 => format!("{} h ago", secs / 3600),
        _ => {
            let d = secs / 86_400;
            if d == 1 {
                "1 day ago".into()
            } else {
                format!("{d} days ago")
            }
        }
    }
}

/// Badge + short text for a compatibility level.
pub fn compat_badge(level: CompatLevel) -> (&'static str, &'static str) {
    match level {
        CompatLevel::Plays => ("✅", "Plays"),
        CompatLevel::NeedsTvFriendly => ("⚠", "Needs TV-friendly"),
        CompatLevel::Unsupported => ("❌", "Unsupported"),
    }
}

/// CSS class for a compatibility badge.
pub fn compat_css(level: CompatLevel) -> &'static str {
    match level {
        CompatLevel::Plays => "success",
        CompatLevel::NeedsTvFriendly => "warning",
        CompatLevel::Unsupported => "error",
    }
}

/// Heading text for a verdict level.
pub fn verdict_title(level: VerdictLevel) -> &'static str {
    match level {
        VerdictLevel::Ok => "✅ Should play smoothly",
        VerdictLevel::Risky => "⚠ Might stutter",
        VerdictLevel::Bad => "❌ Will most likely stutter",
    }
}

/// CSS class for a verdict level.
pub fn verdict_css(level: VerdictLevel) -> &'static str {
    match level {
        VerdictLevel::Ok => "success",
        VerdictLevel::Risky => "warning",
        VerdictLevel::Bad => "error",
    }
}

/// Connection state for the header.
pub fn connection_text(c: &ConnectionState) -> String {
    match c {
        ConnectionState::Disconnected => "Disconnected".into(),
        ConnectionState::Connecting => "Connecting… approve on the TV if asked".into(),
        ConnectionState::Connected => "Connected".into(),
        ConnectionState::Retrying(_) => "Reconnecting…".into(),
        ConnectionState::Unauthorized => "Approve on the TV".into(),
    }
}

/// Power state for the header.
pub fn power_text(p: &PowerState) -> String {
    match p {
        PowerState::On => "TV on".into(),
        PowerState::Standby => "TV in standby".into(),
        PowerState::Off => "TV off / unreachable".into(),
        PowerState::Unknown(s) if s.is_empty() => "Power unknown".into(),
        PowerState::Unknown(s) => format!("Power: {s}"),
    }
}

/// Icon name and CSS class for the header status indicator.
pub fn status_icon(c: &ConnectionState, p: Option<&PowerState>) -> (&'static str, &'static str) {
    match (c, p) {
        (ConnectionState::Connected, _) => ("network-wireless-signal-excellent-symbolic", "success"),
        (ConnectionState::Unauthorized, _) => ("dialog-password-symbolic", "warning"),
        (_, Some(PowerState::On)) => ("network-wireless-acquiring-symbolic", "warning"),
        (ConnectionState::Connecting | ConnectionState::Retrying(_), _) => {
            ("network-wireless-acquiring-symbolic", "dim-label")
        }
        _ => ("network-wireless-offline-symbolic", "dim-label"),
    }
}

fn lang(s: &StreamInfo) -> Option<&str> {
    s.language.as_deref().filter(|l| !l.is_empty() && *l != "und")
}

fn channels_text(s: &StreamInfo) -> Option<String> {
    if let Some(l) = s.channel_layout.as_deref().filter(|l| !l.is_empty()) {
        return Some(l.to_string());
    }
    s.channels.map(|c| match c {
        1 => "mono".to_string(),
        2 => "stereo".to_string(),
        n => format!("{n} ch"),
    })
}

/// Audio-track label for the TV-friendly dropdown, e.g.
/// `eng · DTS-HD MA · 7.1 · "Original" → AC3`. DTS and other codecs the TV cannot
/// decode are converted to AC3 by `AudioHandling::Auto`, which the label says.
pub fn audio_track_label(s: &StreamInfo) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(l) = lang(s) {
        parts.push(l.to_string());
    }
    let codec = match &s.profile {
        Some(p) if is_dts(&s.codec) && !p.is_empty() => p.clone(),
        _ => s.codec.to_uppercase(),
    };
    parts.push(codec);
    if let Some(c) = channels_text(s) {
        parts.push(c);
    }
    if let Some(t) = s.title.as_deref().filter(|t| !t.is_empty()) {
        parts.push(format!("\"{t}\""));
    }
    if s.default {
        parts.push("default".into());
    }
    let mut out = format!("#{} {}", s.index, parts.join(" · "));
    if is_dts(&s.codec) || !is_tv_safe_audio(&s.codec) {
        out.push_str(" → AC3");
    }
    out
}

/// Subtitle label, e.g. `#12 fre · SUBRIP · "Forced"`.
pub fn subtitle_label(s: &StreamInfo) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(l) = lang(s) {
        parts.push(l.to_string());
    }
    parts.push(s.codec.to_uppercase());
    if let Some(t) = s.title.as_deref().filter(|t| !t.is_empty()) {
        parts.push(format!("\"{t}\""));
    }
    if s.forced {
        parts.push("forced".into());
    }
    format!("#{} {}", s.index, parts.join(" · "))
}

/// One-line technical summary for the browser row: `3840×2160 · HEVC HDR10 · 2:49:00`.
pub fn media_summary(info: &MediaInfo) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(v) = info.video() {
        if let (Some(w), Some(h)) = (v.width, v.height) {
            parts.push(format!("{w}×{h}"));
        }
        let mut c = v.codec.to_uppercase();
        match v.hdr {
            HdrKind::Hdr10 => c.push_str(" HDR10"),
            HdrKind::Hlg => c.push_str(" HLG"),
            HdrKind::Sdr => {}
        }
        if v.dolby_vision_profile.is_some() {
            c.push_str(" DV");
        }
        parts.push(c);
    }
    if let Some(a) = info.default_audio() {
        let codec = match &a.profile {
            Some(p) if is_dts(&a.codec) && !p.is_empty() => p.clone(),
            _ => a.codec.to_uppercase(),
        };
        parts.push(codec);
    }
    if let Some(d) = info.duration() {
        parts.push(fmt_duration(d));
    }
    parts.join(" · ")
}

/// Video-quality steps (Mbit/s) offered for "Reduced": only those below the source
/// video bitrate (when known), always at least the lowest one, highest first. `extra`
/// (e.g. the recommended target) is merged in.
pub fn quality_steps(source_video_mbps: Option<f64>, extra: Option<f64>) -> Vec<f64> {
    const STEPS: [f64; 6] = [60.0, 40.0, 30.0, 20.0, 15.0, 10.0];
    let mut v: Vec<f64> = STEPS
        .iter()
        .copied()
        .filter(|s| source_video_mbps.is_none_or(|src| *s < src))
        .collect();
    if let Some(e) = extra.filter(|e| e.is_finite() && *e > 0.0) {
        if !v.iter().any(|s| (s - e).abs() < 0.05) {
            v.push(e);
        }
    }
    if v.is_empty() {
        v.push(STEPS[STEPS.len() - 1]);
    }
    v.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
    v
}

/// "Reduced to 30 Mbps".
pub fn quality_label(mbps: f64) -> String {
    if (mbps - mbps.round()).abs() < 0.05 {
        format!("Reduced to {mbps:.0} Mbps")
    } else {
        format!("Reduced to {mbps:.1} Mbps")
    }
}

/// Fraction (0.0-1.0) of a slider's range under pointer `x`: the knob (`knob` wide)
/// travels inside a trough starting at `start`, `width` wide, so its centre runs from
/// `start + knob / 2` (0 %) to `start + width - knob / 2` (100 %). Pure.
pub fn x_to_fraction(x: f64, start: f64, width: f64, knob: f64) -> f64 {
    let travel = width - knob;
    if !travel.is_finite() || travel <= 0.0 {
        return 0.0;
    }
    ((x - start - knob / 2.0) / travel).clamp(0.0, 1.0)
}

/// Throughput samples → points for the sparkline: x in 0..=1, y in 0..=1 (1 = top),
/// scaled to `max(samples, floor)`. Returns the scale used (Mbit/s).
pub fn sparkline_points(samples: &[f64], floor: f64) -> (Vec<(f64, f64)>, f64) {
    let max = samples
        .iter()
        .copied()
        .filter(|v| v.is_finite())
        .fold(floor.max(1e-9), f64::max);
    let n = samples.len();
    let pts = samples
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let x = if n <= 1 { 1.0 } else { i as f64 / (n - 1) as f64 };
            let y = if v.is_finite() { (v / max).clamp(0.0, 1.0) } else { 0.0 };
            (x, y)
        })
        .collect();
    (pts, max)
}

/// Extract a YouTube-ish id/URL from user input (trimmed); empty → None.
/// (dot colour, text) for the LAN status dot of a TV.
pub fn lan_dot(p: Option<&PowerState>) -> (&'static str, &'static str) {
    match p {
        Some(PowerState::On) => ("#2ec27e", "LAN: on"),
        Some(PowerState::Standby) => ("#e5a50a", "LAN: standby"),
        Some(PowerState::Off) => ("#e01b24", "LAN: unreachable"),
        Some(PowerState::Unknown(_)) => ("#e5a50a", "LAN: ?"),
        None => ("#9a9996", "LAN: …"),
    }
}

/// (dot colour, text) for the SmartThings status dot of a TV.
pub fn cloud_dot(linked: bool, c: Option<&CloudState>) -> (&'static str, &'static str) {
    match (linked, c) {
        (false, _) | (_, Some(CloudState::NotLinked)) => ("#9a9996", "cloud: not linked"),
        (_, Some(CloudState::Online)) => ("#2ec27e", "cloud: online"),
        (_, Some(CloudState::Offline)) => ("#e01b24", "cloud: offline"),
        (_, Some(CloudState::Unknown(_))) => ("#e5a50a", "cloud: ?"),
        (_, None) => ("#9a9996", "cloud: …"),
    }
}

/// Pango markup: two coloured dots (LAN, cloud), with text when `with_text`.
pub fn dots_markup(p: Option<&PowerState>, linked: bool, c: Option<&CloudState>, with_text: bool) -> String {
    let (lc, lt) = lan_dot(p);
    let (cc, ct) = cloud_dot(linked, c);
    if with_text {
        format!("<span foreground=\"{lc}\">●</span> {lt}   <span foreground=\"{cc}\">●</span> {ct}")
    } else {
        format!("<span foreground=\"{lc}\">●</span><span foreground=\"{cc}\">●</span>")
    }
}

pub fn non_empty(s: &str) -> Option<String> {
    let t = s.trim();
    (!t.is_empty()).then(|| t.to_string())
}

/// Does this look like a URL the TV could fetch?
pub fn looks_like_url(s: &str) -> bool {
    let t = s.trim().to_ascii_lowercase();
    (t.starts_with("http://") || t.starts_with("https://")) && t.len() > 10
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slider_x_to_fraction() {
        // Trough 10..210 px, 20 px knob: its centre travels 20..200.
        assert_eq!(x_to_fraction(20.0, 10.0, 200.0, 20.0), 0.0);
        assert_eq!(x_to_fraction(200.0, 10.0, 200.0, 20.0), 1.0);
        assert_eq!(x_to_fraction(110.0, 10.0, 200.0, 20.0), 0.5);
        assert_eq!(x_to_fraction(-50.0, 10.0, 200.0, 20.0), 0.0, "left of the trough");
        assert_eq!(x_to_fraction(500.0, 10.0, 200.0, 20.0), 1.0, "right of the trough");
        assert_eq!(x_to_fraction(50.0, 0.0, 100.0, 0.0), 0.5, "no knob");
        assert_eq!(x_to_fraction(5.0, 0.0, 10.0, 20.0), 0.0, "knob wider than the trough");
    }

    #[test]
    fn status_dots() {
        let m = dots_markup(Some(&PowerState::On), true, Some(&CloudState::Online), true);
        assert!(m.contains("LAN: on") && m.contains("cloud: online"));
        assert_eq!(cloud_dot(false, Some(&CloudState::Online)).1, "cloud: not linked");
        assert_eq!(lan_dot(None).1, "LAN: …");
        assert_eq!(dots_markup(None, false, None, false).matches('●').count(), 2);
    }
    use std::path::PathBuf;
    use tvremote_core::cast::StreamKind;

    fn audio(codec: &str, profile: Option<&str>) -> StreamInfo {
        StreamInfo {
            index: 8,
            kind: StreamKind::Audio,
            codec: codec.into(),
            profile: profile.map(Into::into),
            language: Some("eng".into()),
            title: Some("Original".into()),
            channels: Some(8),
            channel_layout: Some("7.1".into()),
            sample_rate: Some(48000),
            width: None,
            height: None,
            fps: None,
            pix_fmt: None,
            bit_depth: None,
            color_transfer: None,
            color_primaries: None,
            color_space: None,
            hdr: HdrKind::Sdr,
            dolby_vision_profile: None,
            bit_rate: None,
            default: false,
            forced: false,
            attached_pic: false,
        }
    }

    #[test]
    fn kinds() {
        assert_eq!(media_kind(&PathBuf::from("/a/B.MKV")), Some(MediaKind::Video));
        assert_eq!(media_kind(&PathBuf::from("x.flac")), Some(MediaKind::Audio));
        assert_eq!(media_kind(&PathBuf::from("x.jpeg")), Some(MediaKind::Image));
        assert_eq!(media_kind(&PathBuf::from("x.txt")), None);
        assert_eq!(media_kind(&PathBuf::from("noext")), None);
    }

    #[test]
    fn sizes_and_durations() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1_500), "1.5 kB");
        assert_eq!(human_size(350_000_000), "350 MB");
        assert_eq!(human_size(78_400_000_000), "78.4 GB");
        assert_eq!(fmt_duration(Duration::from_secs(123)), "2:03");
        assert_eq!(fmt_duration(Duration::from_secs(3723)), "1:02:03");
        assert_eq!(fmt_mbps(75.84), "75.8 Mbps");
        assert_eq!(fmt_mbps(312.4), "312 Mbps");
        assert_eq!(fmt_bps(640_000), "0.6 Mbps");
        assert_eq!(fmt_age(5), "just now");
        assert_eq!(fmt_age(600), "10 min ago");
        assert_eq!(fmt_age(3 * 3600 + 5), "3 h ago");
        assert_eq!(fmt_age(86_400), "1 day ago");
        assert_eq!(fmt_age(3 * 86_400), "3 days ago");
    }

    #[test]
    fn badges() {
        assert_eq!(compat_badge(CompatLevel::Plays).0, "✅");
        assert_eq!(compat_badge(CompatLevel::NeedsTvFriendly).0, "⚠");
        assert_eq!(compat_badge(CompatLevel::Unsupported).0, "❌");
        assert_eq!(compat_css(CompatLevel::Unsupported), "error");
        assert!(verdict_title(VerdictLevel::Bad).starts_with('❌'));
        assert_eq!(verdict_css(VerdictLevel::Risky), "warning");
    }

    #[test]
    fn audio_labels() {
        let dts = audio("dts", Some("DTS-HD MA"));
        let l = audio_track_label(&dts);
        assert_eq!(l, "#8 eng · DTS-HD MA · 7.1 · \"Original\" → AC3");
        let mut aac = audio("aac", Some("LC"));
        aac.channel_layout = None;
        aac.channels = Some(2);
        aac.title = None;
        aac.default = true;
        assert_eq!(audio_track_label(&aac), "#8 eng · AAC · stereo · default");
        let truehd = audio("truehd", None);
        assert!(audio_track_label(&truehd).ends_with("→ AC3"));
    }

    #[test]
    fn subtitle_labels() {
        let mut s = audio("subrip", None);
        s.kind = StreamKind::Subtitle;
        s.language = Some("fre".into());
        s.title = None;
        s.forced = true;
        assert_eq!(subtitle_label(&s), "#8 fre · SUBRIP · forced");
    }

    #[test]
    fn quality() {
        assert_eq!(quality_steps(Some(25.0), None), vec![20.0, 15.0, 10.0]);
        assert_eq!(quality_steps(Some(5.0), None), vec![10.0]);
        assert_eq!(quality_steps(None, Some(32.0)), vec![60.0, 40.0, 32.0, 30.0, 20.0, 15.0, 10.0]);
        assert_eq!(quality_steps(Some(80.0), Some(40.0)).len(), 6);
        assert_eq!(quality_label(30.0), "Reduced to 30 Mbps");
        assert_eq!(quality_label(32.5), "Reduced to 32.5 Mbps");
    }

    #[test]
    fn sparkline() {
        let (pts, max) = sparkline_points(&[0.0, 50.0, 100.0], 10.0);
        assert_eq!(max, 100.0);
        assert_eq!(pts, vec![(0.0, 0.0), (0.5, 0.5), (1.0, 1.0)]);
        let (pts, max) = sparkline_points(&[2.0], 10.0);
        assert_eq!(max, 10.0);
        assert_eq!(pts, vec![(1.0, 0.2)]);
        let (pts, _) = sparkline_points(&[], 10.0);
        assert!(pts.is_empty());
    }

    #[test]
    fn misc() {
        assert_eq!(non_empty("  x "), Some("x".into()));
        assert_eq!(non_empty("   "), None);
        assert!(looks_like_url("https://example.org/a.mp4"));
        assert!(!looks_like_url("/home/a.mp4"));
        let (_, css) = status_icon(&ConnectionState::Connected, None);
        assert_eq!(css, "success");
        assert_eq!(power_text(&PowerState::Standby), "TV in standby");
        assert_eq!(connection_text(&ConnectionState::Retrying("x".into())), "Reconnecting…");
    }
}
