//! Media analysis for casting: ffprobe → [`MediaInfo`], TV compatibility check,
//! [`CastProfile`] (TV-friendly options), bitrate estimates, ffmpeg argument building,
//! and the VAAPI availability probe.
//!
//! Everything except [`probe`] and [`vaapi_available`] is pure and unit-tested.
//! Re-exported from [`crate::cast`].

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

// ---------------------------------------------------------------------------
// TV limits (verified on the Q7F, see the plan)

/// Max decodable width (4K DCI).
pub const MAX_WIDTH: u32 = 4096;
/// Max decodable height.
pub const MAX_HEIGHT: u32 = 2176;
/// Max frame rate (4K120 was rejected).
pub const MAX_FPS: f64 = 60.5;
/// AC3 bitrate used when converting audio.
pub const AC3_BITRATE: u64 = 640_000;

/// Video bitrate cap in bit/s for a codec, if the codec is known to play.
pub fn video_bitrate_cap(codec: &str) -> Option<u64> {
    match codec {
        "hevc" | "vp9" => Some(80_000_000),
        "h264" => Some(60_000_000),
        "av1" => Some(40_000_000),
        _ => None,
    }
}

/// Video codecs verified to play.
pub fn is_tested_video_codec(codec: &str) -> bool {
    matches!(codec, "h264" | "hevc" | "vp9" | "av1")
}

/// Video codecs that probably play but were not tested.
pub fn is_untested_video_codec(codec: &str) -> bool {
    matches!(codec, "mpeg2video" | "mpeg4" | "vc1" | "vp8" | "mjpeg" | "wmv3")
}

/// Audio codecs the TV decodes itself (copied as-is in TV-friendly mode).
pub fn is_tv_safe_audio(codec: &str) -> bool {
    matches!(
        codec,
        "aac" | "ac3" | "eac3" | "mp3" | "mp2" | "flac" | "opus" | "vorbis" | "alac"
    ) || codec.starts_with("pcm_")
}

/// DTS family (core, HD MA, HRA, Express): the TV has **no** DTS decoder.
pub fn is_dts(codec: &str) -> bool {
    codec == "dts" || codec.starts_with("dts")
}

// ---------------------------------------------------------------------------
// MediaInfo

/// Kind of a stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StreamKind {
    /// Video (attached cover pictures are flagged via `StreamInfo::attached_pic`).
    Video,
    /// Audio.
    Audio,
    /// Subtitles.
    Subtitle,
    /// Attachment (fonts in mkv).
    Attachment,
    /// Data stream.
    Data,
    /// Anything else.
    Other,
}

/// HDR flavour of a video stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HdrKind {
    /// Standard dynamic range.
    Sdr,
    /// PQ transfer (`smpte2084`): HDR10 / HDR10+.
    Hdr10,
    /// HLG (`arib-std-b67`).
    Hlg,
}

/// One stream from ffprobe.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StreamInfo {
    /// Absolute stream index in the file (ffmpeg `0:<index>`).
    pub index: u32,
    /// Stream kind.
    pub kind: StreamKind,
    /// ffprobe `codec_name` (`hevc`, `dts`, `subrip`, ...).
    pub codec: String,
    /// ffprobe `profile` (`Main 10`, `DTS-HD MA`, ...).
    pub profile: Option<String>,
    /// `tags.language` (ISO 639-2, e.g. `eng`).
    pub language: Option<String>,
    /// `tags.title`.
    pub title: Option<String>,
    /// Audio channel count.
    pub channels: Option<u32>,
    /// Audio channel layout (`5.1(side)`).
    pub channel_layout: Option<String>,
    /// Audio sample rate (Hz).
    pub sample_rate: Option<u32>,
    /// Video width.
    pub width: Option<u32>,
    /// Video height.
    pub height: Option<u32>,
    /// Video frame rate (from `avg_frame_rate`, else `r_frame_rate`).
    pub fps: Option<f64>,
    /// Pixel format (`yuv420p10le`).
    pub pix_fmt: Option<String>,
    /// Bits per sample of the video (8, 10, 12) when known.
    pub bit_depth: Option<u32>,
    /// `color_transfer` (`smpte2084`, `arib-std-b67`, `bt709`).
    pub color_transfer: Option<String>,
    /// `color_primaries`.
    pub color_primaries: Option<String>,
    /// `color_space`.
    pub color_space: Option<String>,
    /// HDR classification from the transfer function.
    pub hdr: HdrKind,
    /// Dolby Vision profile if a DOVI configuration record is present.
    pub dolby_vision_profile: Option<u32>,
    /// Bitrate in bit/s: stream `bit_rate`, else the mkv `BPS`/`BPS-eng` tag.
    pub bit_rate: Option<u64>,
    /// `disposition.default`.
    pub default: bool,
    /// `disposition.forced`.
    pub forced: bool,
    /// `disposition.attached_pic` (cover art, not real video).
    pub attached_pic: bool,
}

impl StreamInfo {
    /// Text-based subtitle that can be kept as an SRT soft track.
    pub fn is_text_subtitle(&self) -> bool {
        self.kind == StreamKind::Subtitle
            && matches!(
                self.codec.as_str(),
                "subrip" | "srt" | "ass" | "ssa" | "mov_text" | "webvtt" | "text"
            )
    }

    /// Human label, e.g. `#8 eng · DTS-HD MA 7.1 · "Original"`.
    pub fn label(&self) -> String {
        let mut parts = vec![format!("#{}", self.index)];
        if let Some(l) = &self.language {
            parts.push(l.clone());
        }
        let codec = match (&self.profile, self.kind) {
            (Some(p), StreamKind::Audio) if is_dts(&self.codec) => p.clone(),
            _ => self.codec.to_uppercase(),
        };
        let mut desc = codec;
        if let Some(cl) = &self.channel_layout {
            desc.push(' ');
            desc.push_str(cl);
        } else if let (Some(w), Some(h)) = (self.width, self.height) {
            desc.push_str(&format!(" {w}×{h}"));
        }
        parts.push(desc);
        if let Some(b) = self.bit_rate {
            parts.push(format!("{:.1} Mbps", b as f64 / 1e6));
        }
        if let Some(t) = &self.title {
            parts.push(format!("\"{t}\""));
        }
        parts.join(" · ")
    }
}

/// Container family.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Container {
    /// Matroska / WebM.
    Matroska,
    /// MP4 / MOV / M4V.
    Mp4,
    /// MPEG transport stream.
    MpegTs,
    /// AVI.
    Avi,
    /// Anything else (audio-only formats, images, ...).
    Other,
}

/// Result of ffprobe on one file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaInfo {
    /// File probed.
    pub path: PathBuf,
    /// ffprobe `format_name` (`matroska,webm`, `mov,mp4,m4a,3gp,3g2,mj2`, ...).
    pub format_name: String,
    /// Duration in seconds.
    pub duration_secs: Option<f64>,
    /// File size in bytes.
    pub size: Option<u64>,
    /// Container average bitrate in bit/s.
    pub bit_rate: Option<u64>,
    /// All streams, in file order.
    pub streams: Vec<StreamInfo>,
}

impl MediaInfo {
    /// Container family from `format_name`.
    pub fn container(&self) -> Container {
        let f = self.format_name.as_str();
        if f.contains("matroska") || f.contains("webm") {
            Container::Matroska
        } else if f.contains("mp4") || f.contains("mov") {
            Container::Mp4
        } else if f.contains("mpegts") {
            Container::MpegTs
        } else if f.contains("avi") {
            Container::Avi
        } else {
            Container::Other
        }
    }

    /// Main video stream (first non-cover-art video).
    pub fn video(&self) -> Option<&StreamInfo> {
        self.streams
            .iter()
            .find(|s| s.kind == StreamKind::Video && !s.attached_pic)
    }

    /// All audio streams.
    pub fn audio_streams(&self) -> impl Iterator<Item = &StreamInfo> {
        self.streams.iter().filter(|s| s.kind == StreamKind::Audio)
    }

    /// All subtitle streams.
    pub fn subtitle_streams(&self) -> impl Iterator<Item = &StreamInfo> {
        self.streams.iter().filter(|s| s.kind == StreamKind::Subtitle)
    }

    /// Stream by absolute index.
    pub fn stream(&self, index: u32) -> Option<&StreamInfo> {
        self.streams.iter().find(|s| s.index == index)
    }

    /// The audio track the TV would pick on its own (default disposition, else first).
    pub fn default_audio(&self) -> Option<&StreamInfo> {
        self.audio_streams()
            .find(|s| s.default)
            .or_else(|| self.audio_streams().next())
    }

    /// Duration as a `Duration`.
    pub fn duration(&self) -> Option<Duration> {
        self.duration_secs
            .filter(|d| d.is_finite() && *d > 0.0)
            .map(Duration::from_secs_f64)
    }

    /// Container average bitrate, or size×8/duration, or the sum of known stream rates.
    pub fn average_bitrate(&self) -> Option<u64> {
        self.bit_rate
            .or_else(|| match (self.size, self.duration_secs) {
                (Some(s), Some(d)) if d > 0.0 => Some((s as f64 * 8.0 / d) as u64),
                _ => None,
            })
            .or_else(|| {
                let sum: u64 = self.streams.iter().filter_map(|s| s.bit_rate).sum();
                (sum > 0).then_some(sum)
            })
    }
}

fn s_opt(v: &serde_json::Value) -> Option<String> {
    v.as_str().filter(|s| !s.is_empty()).map(str::to_string)
}

fn num_opt<T: std::str::FromStr>(v: &serde_json::Value) -> Option<T> {
    match v {
        serde_json::Value::String(s) => s.trim().parse().ok(),
        serde_json::Value::Number(n) => n.to_string().parse().ok(),
        _ => None,
    }
}

fn parse_rate(s: &str) -> Option<f64> {
    let (a, b) = s.split_once('/')?;
    let (a, b): (f64, f64) = (a.parse().ok()?, b.parse().ok()?);
    (b > 0.0 && a > 0.0).then(|| a / b)
}

fn tag<'a>(tags: &'a serde_json::Value, key: &str) -> Option<&'a serde_json::Value> {
    // ffprobe keeps tag case from the file; mkv uses BPS / BPS-eng, others lowercase.
    tags.as_object().and_then(|m| {
        m.iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v)
    })
}

/// Parse `ffprobe -print_format json -show_format -show_streams` output.
pub fn parse_ffprobe(json: &str, path: &Path) -> Result<MediaInfo> {
    let v: serde_json::Value =
        serde_json::from_str(json).map_err(|e| Error::Media(format!("ffprobe JSON: {e}")))?;
    let fmt = &v["format"];
    let mut streams = Vec::new();
    for s in v["streams"].as_array().map(Vec::as_slice).unwrap_or(&[]) {
        let kind = match s["codec_type"].as_str().unwrap_or("") {
            "video" => StreamKind::Video,
            "audio" => StreamKind::Audio,
            "subtitle" => StreamKind::Subtitle,
            "attachment" => StreamKind::Attachment,
            "data" => StreamKind::Data,
            _ => StreamKind::Other,
        };
        let tags = &s["tags"];
        let transfer = s_opt(&s["color_transfer"]);
        let hdr = match transfer.as_deref() {
            Some("smpte2084") => HdrKind::Hdr10,
            Some("arib-std-b67") => HdrKind::Hlg,
            _ => HdrKind::Sdr,
        };
        let pix_fmt = s_opt(&s["pix_fmt"]);
        let bit_depth = num_opt::<u32>(&s["bits_per_raw_sample"]).or_else(|| {
            pix_fmt.as_deref().map(|p| {
                if p.contains("12") {
                    12
                } else if p.contains("10") || p.starts_with("p010") {
                    10
                } else {
                    8
                }
            })
        });
        let dv = s["side_data_list"].as_array().and_then(|l| {
            l.iter()
                .find(|d| {
                    d["side_data_type"]
                        .as_str()
                        .is_some_and(|t| t.contains("DOVI"))
                })
                .map(|d| num_opt::<u32>(&d["dv_profile"]).unwrap_or(0))
        });
        let bit_rate = num_opt::<u64>(&s["bit_rate"])
            .or_else(|| tag(tags, "BPS").and_then(num_opt))
            .or_else(|| tag(tags, "BPS-eng").and_then(num_opt))
            .filter(|b| *b > 0);
        let fps = s["avg_frame_rate"]
            .as_str()
            .and_then(parse_rate)
            .or_else(|| s["r_frame_rate"].as_str().and_then(parse_rate));
        streams.push(StreamInfo {
            index: s["index"].as_u64().unwrap_or(streams.len() as u64) as u32,
            kind,
            codec: s["codec_name"].as_str().unwrap_or("unknown").to_string(),
            profile: s_opt(&s["profile"]),
            language: tag(tags, "language").and_then(s_opt).filter(|l| l != "und"),
            title: tag(tags, "title").and_then(s_opt),
            channels: num_opt(&s["channels"]),
            channel_layout: s_opt(&s["channel_layout"]),
            sample_rate: num_opt(&s["sample_rate"]),
            width: num_opt(&s["width"]),
            height: num_opt(&s["height"]),
            fps: if kind == StreamKind::Video { fps } else { None },
            pix_fmt,
            bit_depth: if kind == StreamKind::Video { bit_depth } else { None },
            color_transfer: transfer,
            color_primaries: s_opt(&s["color_primaries"]),
            color_space: s_opt(&s["color_space"]),
            hdr,
            dolby_vision_profile: dv,
            bit_rate,
            default: s["disposition"]["default"].as_i64() == Some(1),
            forced: s["disposition"]["forced"].as_i64() == Some(1),
            attached_pic: s["disposition"]["attached_pic"].as_i64() == Some(1),
        });
    }
    Ok(MediaInfo {
        path: path.to_path_buf(),
        format_name: fmt["format_name"].as_str().unwrap_or("").to_string(),
        duration_secs: num_opt(&fmt["duration"]),
        size: num_opt(&fmt["size"]),
        bit_rate: num_opt::<u64>(&fmt["bit_rate"]).filter(|b| *b > 0),
        streams,
    })
}

/// Run ffprobe on a local file (blocking; usually < 1 s).
pub fn probe(path: &Path) -> Result<MediaInfo> {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-print_format", "json", "-show_format", "-show_streams"])
        .arg(path)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| Error::Media(format!("cannot run ffprobe: {e}")))?;
    if !out.status.success() {
        return Err(Error::Media(format!(
            "ffprobe failed on {}: {}",
            path.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    parse_ffprobe(&String::from_utf8_lossy(&out.stdout), path)
}

// ---------------------------------------------------------------------------
// Compatibility

/// Overall verdict for casting the file *directly* (no conversion).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum CompatLevel {
    /// ✅ Plays as-is.
    Plays,
    /// ⚠ Needs TV-friendly mode (audio conversion, remux, or video re-encode).
    NeedsTvFriendly,
    /// ❌ Cannot be played even with TV-friendly mode.
    Unsupported,
}

/// Severity of one compatibility finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Severity {
    /// Informational.
    Info,
    /// Might not work; untested.
    Warning,
    /// Will not play directly.
    Problem,
}

/// One compatibility finding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompatIssue {
    /// How bad.
    pub severity: Severity,
    /// Stream concerned, if any.
    pub stream: Option<u32>,
    /// Human explanation.
    pub message: String,
    /// True if TV-friendly mode fixes it.
    pub fixable: bool,
}

/// Result of [`check_compat`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompatReport {
    /// Verdict assuming VAAPI re-encoding is available (see [`CompatReport::effective_level`]).
    pub level: CompatLevel,
    /// Findings, worst first.
    pub issues: Vec<CompatIssue>,
    /// Some audio needs converting to AC3 (DTS default track, etc.).
    pub needs_audio_conversion: bool,
    /// A remux is needed (e.g. HEVC in AVI).
    pub needs_remux: bool,
    /// The video must be re-encoded (codec, resolution, fps, or bitrate over the cap).
    pub needs_video_reencode: bool,
}

impl CompatReport {
    /// Level taking VAAPI availability into account: a file that needs a video re-encode
    /// is [`CompatLevel::Unsupported`] when VAAPI is not available.
    pub fn effective_level(&self, vaapi_available: bool) -> CompatLevel {
        if self.needs_video_reencode && !vaapi_available {
            CompatLevel::Unsupported
        } else {
            self.level
        }
    }

    /// Badge text: `✅`, `⚠` or `❌`.
    pub fn badge(&self) -> &'static str {
        match self.level {
            CompatLevel::Plays => "✅",
            CompatLevel::NeedsTvFriendly => "⚠",
            CompatLevel::Unsupported => "❌",
        }
    }
}

/// Estimated video bitrate: stream rate, else container average minus the other streams.
pub fn video_bitrate_estimate(info: &MediaInfo) -> Option<u64> {
    let v = info.video()?;
    if let Some(b) = v.bit_rate {
        return Some(b);
    }
    let total = info.average_bitrate()?;
    let others: u64 = info
        .streams
        .iter()
        .filter(|s| s.index != v.index)
        .map(|s| stream_bitrate_guess(s).unwrap_or(0))
        .sum();
    Some(total.saturating_sub(others))
}

/// Known or guessed bitrate of a non-video stream.
pub fn stream_bitrate_guess(s: &StreamInfo) -> Option<u64> {
    if let Some(b) = s.bit_rate {
        return Some(b);
    }
    match s.kind {
        StreamKind::Audio => {
            let ch = s.channels.unwrap_or(2) as u64;
            Some(match s.codec.as_str() {
                c if is_dts(c) => {
                    if s.profile.as_deref().is_some_and(|p| p.contains("MA")) {
                        3_000_000
                    } else {
                        1_509_000
                    }
                }
                "truehd" | "mlp" => 3_500_000,
                "ac3" => 640_000,
                "eac3" => 768_000,
                "flac" | "alac" => 900_000 * ch.max(2) / 2,
                c if c.starts_with("pcm_") => 1_536_000 * ch.max(2) / 2,
                "opus" | "vorbis" => 64_000 * ch,
                _ => 96_000 * ch,
            })
        }
        StreamKind::Subtitle => Some(if s.is_text_subtitle() { 200 } else { 40_000 }),
        _ => None,
    }
}

/// Compare a file against the TV's limits for a **direct** cast.
pub fn check_compat(info: &MediaInfo) -> CompatReport {
    if let Some(r) = check_still_image(info) {
        return r;
    }
    let mut issues = Vec::new();
    let mut audio_conv = false;
    let mut remux = false;
    let mut reencode = false;
    let container = info.container();

    if let Some(v) = info.video() {
        let codec = v.codec.as_str();
        if is_tested_video_codec(codec) {
            if codec == "hevc"
                && !matches!(container, Container::Matroska | Container::Mp4 | Container::MpegTs)
            {
                remux = true;
                issues.push(CompatIssue {
                    severity: Severity::Problem,
                    stream: Some(v.index),
                    message: format!(
                        "HEVC is only played from MKV, MP4 or TS (this is {}); TV-friendly mode remuxes it",
                        info.format_name
                    ),
                    fixable: true,
                });
            }
        } else if is_untested_video_codec(codec) {
            issues.push(CompatIssue {
                severity: Severity::Warning,
                stream: Some(v.index),
                message: format!("{} video has not been tested on this TV", codec.to_uppercase()),
                fixable: true,
            });
        } else {
            reencode = true;
            issues.push(CompatIssue {
                severity: Severity::Problem,
                stream: Some(v.index),
                message: format!("{} video is not supported; needs re-encoding to HEVC", codec.to_uppercase()),
                fixable: true,
            });
        }
        if let (Some(w), Some(h)) = (v.width, v.height) {
            if w > MAX_WIDTH || h > MAX_HEIGHT {
                reencode = true;
                issues.push(CompatIssue {
                    severity: Severity::Problem,
                    stream: Some(v.index),
                    message: format!("{w}×{h} is above 4K; the TV rejects 8K (re-encode scales to 4K)"),
                    fixable: true,
                });
            }
        }
        if let Some(fps) = v.fps {
            if fps > MAX_FPS {
                reencode = true;
                issues.push(CompatIssue {
                    severity: Severity::Problem,
                    stream: Some(v.index),
                    message: format!("{fps:.0} fps is above 60 fps (4K120 was rejected); re-encode caps it at 60"),
                    fixable: true,
                });
            }
        }
        if let (Some(cap), Some(rate)) = (video_bitrate_cap(codec), video_bitrate_estimate(info)) {
            if rate > cap {
                reencode = true;
                issues.push(CompatIssue {
                    severity: Severity::Problem,
                    stream: Some(v.index),
                    message: format!(
                        "video bitrate {:.1} Mbps is above the TV's {} cap of {} Mbps",
                        rate as f64 / 1e6,
                        codec.to_uppercase(),
                        cap / 1_000_000
                    ),
                    fixable: true,
                });
            }
        }
        if let Some(p) = v.dolby_vision_profile {
            let (sev, msg) = if p == 5 {
                (Severity::Warning, "Dolby Vision profile 5 has no HDR10 base layer; colours will be wrong (the TV has no Dolby Vision)".to_string())
            } else {
                (Severity::Info, format!("Dolby Vision profile {p}: the TV plays the HDR10 base layer"))
            };
            issues.push(CompatIssue { severity: sev, stream: Some(v.index), message: msg, fixable: false });
        }
    }

    let audio: Vec<&StreamInfo> = info.audio_streams().collect();
    if !audio.is_empty() {
        let default = info.default_audio();
        let all_dts = audio.iter().all(|a| is_dts(&a.codec));
        for a in &audio {
            if is_dts(&a.codec) {
                let is_default = default.is_some_and(|d| d.index == a.index);
                // A non-default DTS track still plays the default one directly, but picking
                // it (e.g. the only English track) needs TV-friendly mode: flag it as a
                // fixable problem so the badge says so, without forcing audio conversion.
                let sev = Severity::Problem;
                if is_default || all_dts {
                    audio_conv = true;
                }
                issues.push(CompatIssue {
                    severity: sev,
                    stream: Some(a.index),
                    message: format!(
                        "audio {} is DTS, which the TV cannot decode{}",
                        a.label(),
                        if is_default { " (it is the default track)" } else { "" }
                    ),
                    fixable: true,
                });
            } else if a.codec == "truehd" || a.codec == "mlp" {
                issues.push(CompatIssue {
                    severity: Severity::Warning,
                    stream: Some(a.index),
                    message: format!("audio {} is TrueHD, not verified on this TV", a.label()),
                    fixable: true,
                });
            } else if !is_tv_safe_audio(&a.codec) {
                issues.push(CompatIssue {
                    severity: Severity::Warning,
                    stream: Some(a.index),
                    message: format!("audio codec {} is not verified on this TV", a.codec),
                    fixable: true,
                });
            }
        }
    }

    for s in info.subtitle_streams() {
        if !s.is_text_subtitle() {
            issues.push(CompatIssue {
                severity: Severity::Info,
                stream: Some(s.index),
                message: format!("subtitle {} is image-based; TV-friendly mode can only keep text subtitles", s.label()),
                fixable: false,
            });
        }
    }

    issues.sort_by_key(|i| std::cmp::Reverse(i.severity));
    let level = if issues.iter().any(|i| i.severity == Severity::Problem && !i.fixable) {
        CompatLevel::Unsupported
    } else if issues.iter().any(|i| i.severity == Severity::Problem) {
        CompatLevel::NeedsTvFriendly
    } else {
        CompatLevel::Plays
    };
    CompatReport {
        level,
        issues,
        needs_audio_conversion: audio_conv,
        needs_remux: remux,
        needs_video_reencode: reencode,
    }
}

/// Still images (ffprobe reports them as a one-frame "video" stream). The TV's DLNA
/// renderer lists JPEG, PNG and BMP; GIF/WebP and the rest are not playable.
fn check_still_image(info: &MediaInfo) -> Option<CompatReport> {
    let v = info.video()?;
    let is_image_format = info.format_name.ends_with("_pipe") || info.format_name == "image2";
    if !is_image_format && info.format_name != "gif" {
        return None;
    }
    let (level, issues) = match v.codec.as_str() {
        "mjpeg" | "png" | "bmp" => (CompatLevel::Plays, Vec::new()),
        other => (
            CompatLevel::Unsupported,
            vec![CompatIssue {
                severity: Severity::Problem,
                stream: Some(v.index),
                message: format!("{} images are not supported (the TV shows JPEG, PNG and BMP)", other.to_uppercase()),
                fixable: false,
            }],
        ),
    };
    Some(CompatReport {
        level,
        issues,
        needs_audio_conversion: false,
        needs_remux: false,
        needs_video_reencode: false,
    })
}

// ---------------------------------------------------------------------------
// Cast profile

/// What to do with the chosen audio track in TV-friendly mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum AudioHandling {
    /// Copy if the TV can decode it, else convert to AC3 640k (DTS, TrueHD, ...).
    #[default]
    Auto,
    /// Always copy (even if the TV may not decode it).
    Copy,
    /// Always convert to AC3 640k.
    ConvertAc3,
}

/// Video handling in TV-friendly mode.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
pub enum VideoMode {
    /// Copy the video stream untouched (HDR preserved).
    #[default]
    Copy,
    /// Re-encode to HEVC with VAAPI at the target bitrate (HDR10 signalling kept,
    /// scaled to ≤ 4K, capped at 60 fps). Requires [`vaapi_available`].
    Reduce {
        /// Target video bitrate in Mbit/s (also used as `-maxrate`).
        target_mbps: f64,
    },
}

/// Container produced by ffmpeg for TV-friendly streams.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum StreamContainer {
    /// Matroska (`video/x-mkv`), keeps SRT subtitles. Default.
    #[default]
    Matroska,
    /// MPEG-TS (`video/vnd.dlna.mpeg-tts`), the most stream-friendly format for DLNA
    /// renderers; subtitles are dropped. Fallback if the TV dislikes live mkv.
    MpegTs,
}

impl StreamContainer {
    /// MIME type served for this container.
    pub fn mime(self) -> &'static str {
        match self {
            StreamContainer::Matroska => "video/x-mkv",
            StreamContainer::MpegTs => "video/vnd.dlna.mpeg-tts",
        }
    }

    /// File extension used in the stream URL.
    pub fn extension(self) -> &'static str {
        match self {
            StreamContainer::Matroska => "mkv",
            StreamContainer::MpegTs => "ts",
        }
    }
}

/// Options for a TV-friendly cast (ffmpeg → live stream).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct CastProfile {
    /// Absolute index of the audio stream to send (`None` = no audio).
    pub audio: Option<u32>,
    /// How to treat that audio stream.
    pub audio_handling: AudioHandling,
    /// Absolute index of a text subtitle stream to keep as a soft SRT track.
    pub subtitle: Option<u32>,
    /// Video handling.
    pub video: VideoMode,
    /// Output container.
    pub container: StreamContainer,
}

/// What the chosen audio will be in the output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AudioOut {
    /// Copied as-is.
    Copy,
    /// Converted to AC3 640k.
    Ac3,
}

impl CastProfile {
    /// Sensible defaults: the default audio track (converted if needed), no subtitles,
    /// video copied unless it must be re-encoded **and** `vaapi` is true (then reduced
    /// to 80 % of the codec cap, at most 40 Mbps).
    ///
    /// Pass [`vaapi_available()`] as `vaapi`. Without VAAPI the video is always
    /// [`VideoMode::Copy`], never `Reduce`; if the file needs a re-encode,
    /// [`CompatReport::effective_level`] then reports it as unsupported.
    pub fn recommended(info: &MediaInfo, vaapi: bool) -> CastProfile {
        let report = check_compat(info);
        let video = if report.needs_video_reencode && vaapi {
            let cap = info
                .video()
                .and_then(|v| video_bitrate_cap(&v.codec))
                .unwrap_or(40_000_000) as f64
                / 1e6;
            VideoMode::Reduce {
                target_mbps: (cap * 0.8).min(40.0),
            }
        } else {
            VideoMode::Copy
        };
        CastProfile {
            audio: info.default_audio().map(|a| a.index),
            audio_handling: AudioHandling::Auto,
            subtitle: None,
            video,
            container: StreamContainer::Matroska,
        }
    }

    /// Resolve what happens to the chosen audio stream.
    pub fn audio_out(&self, info: &MediaInfo) -> Option<AudioOut> {
        let a = info.stream(self.audio?)?;
        Some(match self.audio_handling {
            AudioHandling::Copy => AudioOut::Copy,
            AudioHandling::ConvertAc3 => AudioOut::Ac3,
            AudioHandling::Auto => {
                if is_tv_safe_audio(&a.codec) {
                    AudioOut::Copy
                } else {
                    AudioOut::Ac3
                }
            }
        })
    }

    /// Absolute indices of the input streams this profile sends.
    pub fn selected_streams(&self, info: &MediaInfo) -> Vec<u32> {
        let mut v = Vec::new();
        if let Some(s) = info.video() {
            v.push(s.index);
        }
        if let Some(a) = self.audio.filter(|a| info.stream(*a).is_some()) {
            v.push(a);
        }
        if self.container == StreamContainer::Matroska {
            if let Some(s) = self.subtitle.filter(|s| info.stream(*s).is_some_and(|x| x.is_text_subtitle())) {
                v.push(s);
            }
        }
        v
    }
}

/// Estimated output bitrate (bit/s) per component.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct BitrateEstimate {
    /// Video.
    pub video_bps: u64,
    /// Audio (after conversion).
    pub audio_bps: u64,
    /// Subtitles.
    pub subtitle_bps: u64,
    /// Sum.
    pub total_bps: u64,
}

impl BitrateEstimate {
    /// Total in Mbit/s.
    pub fn total_mbps(&self) -> f64 {
        self.total_bps as f64 / 1e6
    }
}

/// Average output bitrate of a TV-friendly cast with this profile.
pub fn estimated_bitrate(info: &MediaInfo, profile: &CastProfile) -> BitrateEstimate {
    let video_bps = match profile.video {
        VideoMode::Copy => video_bitrate_estimate(info).unwrap_or(0),
        VideoMode::Reduce { target_mbps } => (target_mbps * 1e6) as u64,
    };
    let audio_bps = match (profile.audio_out(info), profile.audio.and_then(|a| info.stream(a))) {
        (Some(AudioOut::Ac3), _) => AC3_BITRATE,
        (Some(AudioOut::Copy), Some(a)) => stream_bitrate_guess(a).unwrap_or(0),
        _ => 0,
    };
    let subtitle_bps = profile
        .selected_streams(info)
        .iter()
        .filter_map(|i| info.stream(*i))
        .filter(|s| s.kind == StreamKind::Subtitle)
        .map(|s| stream_bitrate_guess(s).unwrap_or(0))
        .sum();
    BitrateEstimate {
        video_bps,
        audio_bps,
        subtitle_bps,
        total_bps: video_bps + audio_bps + subtitle_bps,
    }
}

/// Average bitrate of a direct cast (the whole file).
pub fn direct_bitrate(info: &MediaInfo) -> u64 {
    info.average_bitrate().unwrap_or(0)
}

/// Output size for a re-encode: scaled to fit 3840×2160 keeping aspect, even dimensions.
pub fn reduce_output_size(w: u32, h: u32) -> Option<(u32, u32)> {
    if w <= 3840 && h <= 2160 {
        return None;
    }
    let scale = (3840.0 / w as f64).min(2160.0 / h as f64);
    let even = |x: f64| ((x / 2.0).round() as u32 * 2).max(2);
    Some((even(w as f64 * scale), even(h as f64 * scale)))
}

/// Build the ffmpeg argument list (without the `ffmpeg` program name) for a
/// TV-friendly stream written to stdout. Pure; unit-tested.
///
/// `vaapi_device` is required when the profile reduces the video.
pub fn ffmpeg_args(
    input: &Path,
    info: &MediaInfo,
    profile: &CastProfile,
    vaapi_device: Option<&Path>,
) -> Result<Vec<String>> {
    let mut a: Vec<String> = ["-hide_banner", "-nostdin", "-loglevel", "error"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let reduce = match profile.video {
        VideoMode::Reduce { target_mbps } => {
            if !(target_mbps.is_finite() && target_mbps > 0.5) {
                return Err(Error::Unsupported(format!("invalid target bitrate {target_mbps} Mbps")));
            }
            let dev = vaapi_device
                .ok_or_else(|| Error::Unsupported("video reduction needs VAAPI, which is unavailable".into()))?;
            a.push("-vaapi_device".into());
            a.push(dev.display().to_string());
            Some(target_mbps)
        }
        VideoMode::Copy => None,
    };
    a.push("-i".into());
    a.push(input.display().to_string());

    let video = info.video();
    if let Some(v) = video {
        a.push("-map".into());
        a.push(format!("0:{}", v.index));
    }
    let audio = profile.audio.and_then(|i| info.stream(i)).filter(|s| s.kind == StreamKind::Audio);
    if let Some(s) = audio {
        a.push("-map".into());
        a.push(format!("0:{}", s.index));
    }
    let sub = if profile.container == StreamContainer::Matroska {
        profile.subtitle.and_then(|i| info.stream(i)).filter(|s| s.is_text_subtitle())
    } else {
        None
    };
    if let Some(s) = sub {
        a.push("-map".into());
        a.push(format!("0:{}", s.index));
    }
    if video.is_none() && audio.is_none() {
        return Err(Error::Unsupported("nothing to send: no video and no audio selected".into()));
    }

    if let Some(v) = video {
        match reduce {
            None => {
                a.extend(["-c:v".into(), "copy".into()]);
            }
            Some(t) => {
                let ten_bit = v.bit_depth.unwrap_or(8) > 8 || v.hdr != HdrKind::Sdr;
                let mut vf = format!("format={},hwupload", if ten_bit { "p010" } else { "nv12" });
                if let Some((w, h)) = v.width.zip(v.height).and_then(|(w, h)| reduce_output_size(w, h)) {
                    vf.push_str(&format!(",scale_vaapi=w={w}:h={h}"));
                }
                let kbps = (t * 1000.0).round() as u64;
                a.extend([
                    "-vf".into(),
                    vf,
                    "-c:v".into(),
                    "hevc_vaapi".into(),
                    "-profile:v".into(),
                    if ten_bit { "main10".into() } else { "main".into() },
                    "-b:v".into(),
                    format!("{kbps}k"),
                    "-maxrate".into(),
                    format!("{kbps}k"),
                    "-bufsize".into(),
                    format!("{}k", kbps * 2),
                ]);
                if v.fps.is_some_and(|f| f > MAX_FPS) {
                    a.extend(["-fpsmax".into(), "60".into()]);
                }
                // Keep the colour signalling (HDR10 / HLG) on the re-encoded stream.
                if let Some(p) = &v.color_primaries {
                    a.extend(["-color_primaries".into(), p.clone()]);
                }
                if let Some(t) = &v.color_transfer {
                    a.extend(["-color_trc".into(), t.clone()]);
                }
                if let Some(c) = &v.color_space {
                    a.extend(["-colorspace".into(), c.clone()]);
                }
            }
        }
    }
    if let Some(s) = audio {
        match profile.audio_out(info) {
            Some(AudioOut::Ac3) => {
                a.extend(["-c:a".into(), "ac3".into(), "-b:a".into(), "640k".into()]);
                if s.channels.unwrap_or(2) > 6 {
                    a.extend(["-ac".into(), "6".into()]);
                }
            }
            _ => a.extend(["-c:a".into(), "copy".into()]),
        }
        a.extend(["-disposition:a:0".into(), "default".into()]);
    }
    if sub.is_some() {
        a.extend([
            "-c:s".into(),
            "srt".into(),
            "-disposition:s:0".into(),
            "default".into(),
        ]);
    }
    a.extend(["-max_muxing_queue_size".into(), "4096".into()]);
    match profile.container {
        StreamContainer::Matroska => a.extend(["-f".into(), "matroska".into()]),
        StreamContainer::MpegTs => a.extend(["-f".into(), "mpegts".into()]),
    }
    a.push("pipe:1".into());
    Ok(a)
}

// ---------------------------------------------------------------------------
// VAAPI

/// First DRM render node (`/dev/dri/renderD128`, ...), if any.
pub fn vaapi_device() -> Option<PathBuf> {
    (128..136)
        .map(|n| PathBuf::from(format!("/dev/dri/renderD{n}")))
        .find(|p| p.exists())
}

/// Whether `hevc_vaapi` encoding works on this machine. The first call runs a tiny
/// local test encode (lavfi source → /dev/null, ~1 s, 20 s timeout); the result is cached
/// for the process lifetime. Blocking — call from a worker thread.
pub fn vaapi_available() -> bool {
    static CACHE: OnceLock<bool> = OnceLock::new();
    *CACHE.get_or_init(|| {
        let Some(dev) = vaapi_device() else { return false };
        let child = Command::new("ffmpeg")
            .args(["-hide_banner", "-nostdin", "-loglevel", "error", "-vaapi_device"])
            .arg(&dev)
            .args([
                "-f", "lavfi", "-i", "testsrc2=size=256x144:rate=30", "-t", "0.3",
                "-vf", "format=p010,hwupload", "-c:v", "hevc_vaapi", "-profile:v", "main10",
                "-f", "null", "-",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        let Ok(mut child) = child else { return false };
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            match child.try_wait() {
                Ok(Some(st)) => return st.success(),
                Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return false;
                }
            }
        }
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A synthetic MediaInfo modelled on the Interstellar UHD remux.
    pub(crate) fn interstellar() -> MediaInfo {
        let mut streams = vec![StreamInfo {
            index: 0,
            kind: StreamKind::Video,
            codec: "hevc".into(),
            profile: Some("Main 10".into()),
            language: None,
            title: None,
            channels: None,
            channel_layout: None,
            sample_rate: None,
            width: Some(3840),
            height: Some(2160),
            fps: Some(23.976),
            pix_fmt: Some("yuv420p10le".into()),
            bit_depth: Some(10),
            color_transfer: Some("smpte2084".into()),
            color_primaries: Some("bt2020".into()),
            color_space: Some("bt2020nc".into()),
            hdr: HdrKind::Hdr10,
            dolby_vision_profile: None,
            bit_rate: Some(51_700_000),
            default: true,
            forced: false,
            attached_pic: false,
        }];
        let audio = |index: u32, codec: &str, lang: &str, br: u64, default: bool| StreamInfo {
            index,
            kind: StreamKind::Audio,
            codec: codec.into(),
            profile: if codec == "dts" { Some("DTS-HD MA".into()) } else { None },
            language: Some(lang.into()),
            title: None,
            channels: Some(6),
            channel_layout: Some("5.1(side)".into()),
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
            bit_rate: Some(br),
            default,
            forced: false,
            attached_pic: false,
        };
        streams.push(audio(1, "ac3", "rus", 640_000, true));
        // 7 DTS tracks totalling ~23.4 Mbps (DTS-HD MA ~3.3 Mbps each).
        for i in 0..7 {
            streams.push(audio(2 + i, "dts", if i == 6 { "eng" } else { "rus" }, 3_340_000, false));
        }
        streams.push(StreamInfo {
            index: 9,
            kind: StreamKind::Subtitle,
            codec: "subrip".into(),
            language: Some("eng".into()),
            bit_rate: Some(100),
            ..audio(9, "subrip", "eng", 100, false)
        });
        MediaInfo {
            path: PathBuf::from("/films/Interstellar.mkv"),
            format_name: "matroska,webm".into(),
            duration_secs: Some(10_140.0),
            size: Some(96_000_000_000),
            bit_rate: Some(75_800_000),
            streams,
        }
    }

    #[test]
    fn parse_ffprobe_sample() {
        let json = r#"{"streams":[
          {"index":0,"codec_name":"hevc","profile":"Main 10","codec_type":"video","width":3840,"height":2160,
           "pix_fmt":"yuv420p10le","color_transfer":"smpte2084","color_primaries":"bt2020","avg_frame_rate":"24000/1001",
           "disposition":{"default":1,"attached_pic":0},"tags":{"BPS":"51700000","title":"Main"}},
          {"index":1,"codec_name":"dts","profile":"DTS-HD MA","codec_type":"audio","channels":8,"channel_layout":"7.1",
           "sample_rate":"48000","disposition":{"default":1},"tags":{"language":"eng","BPS-eng":"4500000"}},
          {"index":2,"codec_name":"hdmv_pgs_subtitle","codec_type":"subtitle","tags":{"language":"fre"}},
          {"index":3,"codec_name":"mjpeg","codec_type":"video","disposition":{"attached_pic":1}}],
         "format":{"format_name":"matroska,webm","duration":"60.5","size":"500000000","bit_rate":"66115702"}}"#;
        let m = parse_ffprobe(json, Path::new("/x.mkv")).unwrap();
        assert_eq!(m.container(), Container::Matroska);
        let v = m.video().unwrap();
        assert_eq!(v.index, 0);
        assert_eq!(v.hdr, HdrKind::Hdr10);
        assert_eq!(v.bit_depth, Some(10));
        assert!((v.fps.unwrap() - 23.976).abs() < 0.01);
        assert_eq!(v.bit_rate, Some(51_700_000));
        let a = m.default_audio().unwrap();
        assert_eq!(a.bit_rate, Some(4_500_000));
        assert_eq!(a.language.as_deref(), Some("eng"));
        assert!(a.label().contains("DTS-HD MA"));
        assert!(m.stream(3).unwrap().attached_pic);
        assert_eq!(m.bit_rate, Some(66_115_702));
        let r = check_compat(&m);
        assert_eq!(r.level, CompatLevel::NeedsTvFriendly);
        assert!(r.needs_audio_conversion);
        assert!(!r.needs_video_reencode);
    }

    #[test]
    fn still_images() {
        let img = |format: &str, codec: &str| {
            let json = format!(
                r#"{{"streams":[{{"index":0,"codec_type":"video","codec_name":"{codec}","width":800,"height":600}}],
                    "format":{{"format_name":"{format}","size":"1000"}}}}"#
            );
            check_compat(&parse_ffprobe(&json, Path::new("x")).unwrap()).level
        };
        assert_eq!(img("png_pipe", "png"), CompatLevel::Plays);
        assert_eq!(img("jpeg_pipe", "mjpeg"), CompatLevel::Plays);
        assert_eq!(img("gif", "gif"), CompatLevel::Unsupported);
        assert_eq!(img("webp_pipe", "webp"), CompatLevel::Unsupported);
    }

    #[test]
    fn compat_rules() {
        let mut m = interstellar();
        let r = check_compat(&m);
        // Default track is AC3: it plays directly, but the DTS tracks (incl. the only
        // English one) need TV-friendly mode → ⚠, without forcing audio conversion.
        assert_eq!(r.level, CompatLevel::NeedsTvFriendly);
        assert!(!r.needs_audio_conversion);
        // Make a DTS track the default → needs TV-friendly.
        m.streams[1].default = false;
        m.streams[8].default = true;
        assert_eq!(check_compat(&m).level, CompatLevel::NeedsTvFriendly);
        // 8K 120 fps over cap → re-encode.
        let mut m = interstellar();
        m.streams[0].width = Some(7680);
        m.streams[0].height = Some(4320);
        m.streams[0].fps = Some(120.0);
        m.streams[0].bit_rate = Some(90_000_000);
        let r = check_compat(&m);
        assert!(r.needs_video_reencode);
        assert_eq!(r.effective_level(false), CompatLevel::Unsupported);
        assert_eq!(r.effective_level(true), CompatLevel::NeedsTvFriendly);
        // HEVC in AVI → remux.
        let mut m = interstellar();
        m.format_name = "avi".into();
        assert!(check_compat(&m).needs_remux);
        // H.264 at 70 Mbps is above the 60 Mbps cap.
        let mut m = interstellar();
        m.streams[0].codec = "h264".into();
        m.streams[0].bit_rate = Some(70_000_000);
        assert!(check_compat(&m).needs_video_reencode);
    }

    #[test]
    fn recommended_never_reduces_without_vaapi() {
        // H.264 at 70 Mbps needs a re-encode.
        let mut m = interstellar();
        m.streams[0].codec = "h264".into();
        m.streams[0].bit_rate = Some(70_000_000);
        assert_eq!(CastProfile::recommended(&m, false).video, VideoMode::Copy);
        assert!(matches!(
            CastProfile::recommended(&m, true).video,
            VideoMode::Reduce { target_mbps } if target_mbps <= 40.0
        ));
        // No re-encode needed: copy either way.
        assert_eq!(CastProfile::recommended(&interstellar(), true).video, VideoMode::Copy);
    }

    #[test]
    fn profile_and_bitrate() {
        let m = interstellar();
        let mut p = CastProfile::recommended(&m, true);
        assert_eq!(p.audio, Some(1));
        assert_eq!(p.video, VideoMode::Copy);
        let e = estimated_bitrate(&m, &p);
        assert_eq!(e.total_bps, 51_700_000 + 640_000);
        p.audio = Some(8); // English DTS
        assert_eq!(p.audio_out(&m), Some(AudioOut::Ac3));
        p.subtitle = Some(9);
        p.video = VideoMode::Reduce { target_mbps: 30.0 };
        let e = estimated_bitrate(&m, &p);
        assert_eq!(e.video_bps, 30_000_000);
        assert_eq!(e.audio_bps, AC3_BITRATE);
        assert_eq!(p.selected_streams(&m), vec![0, 8, 9]);
        assert_eq!(direct_bitrate(&m), 75_800_000);
    }

    #[test]
    fn ffmpeg_args_copy_and_reduce() {
        let m = interstellar();
        let mut p = CastProfile { audio: Some(8), subtitle: Some(9), ..Default::default() };
        let a = ffmpeg_args(Path::new("/in.mkv"), &m, &p, None).unwrap();
        let s = a.join(" ");
        assert!(s.contains("-i /in.mkv -map 0:0 -map 0:8 -map 0:9 -c:v copy"));
        assert!(s.contains("-c:a ac3 -b:a 640k"));
        assert!(!s.contains("-ac 6"));
        assert!(s.contains("-c:s srt"));
        assert!(s.ends_with("-f matroska pipe:1"));

        p.video = VideoMode::Reduce { target_mbps: 25.0 };
        assert!(ffmpeg_args(Path::new("/in.mkv"), &m, &p, None).is_err());
        let a = ffmpeg_args(Path::new("/in.mkv"), &m, &p, Some(Path::new("/dev/dri/renderD128"))).unwrap();
        let s = a.join(" ");
        assert!(s.starts_with("-hide_banner -nostdin -loglevel error -vaapi_device /dev/dri/renderD128 -i"));
        assert!(s.contains("-vf format=p010,hwupload -c:v hevc_vaapi -profile:v main10 -b:v 25000k -maxrate 25000k -bufsize 50000k"));
        assert!(s.contains("-color_trc smpte2084"));

        p.container = StreamContainer::MpegTs;
        p.video = VideoMode::Copy;
        let s = ffmpeg_args(Path::new("/in.mkv"), &m, &p, None).unwrap().join(" ");
        assert!(!s.contains("0:9"));
        assert!(s.ends_with("-f mpegts pipe:1"));

        assert_eq!(reduce_output_size(7680, 4320), Some((3840, 2160)));
        assert_eq!(reduce_output_size(4096, 2160), Some((3840, 2026)));
        assert_eq!(reduce_output_size(3840, 1600), None);
    }
}
