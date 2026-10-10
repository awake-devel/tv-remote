//! "Can the network carry this?" — stream demand, network capacity, link hints,
//! and the verdict shown before every cast.
//!
//! 1. **Demand.** [`analyze_in_background`] runs `ffprobe -show_packets` once per file
//!    (minutes for a large remux) and stores per-stream bytes per second
//!    ([`PacketProfile`], cached in `~/.cache/tv-remote/` by path + size + mtime).
//!    [`compute_demand`] then derives the 2 s / 10 s peak for any selection of streams
//!    (direct cast or a TV-friendly profile). Without a profile it returns a
//!    *provisional* estimate: average × 1.5.
//! 2. **Capacity.** [`measure_capacity`] casts an incompressible ~150 Mbps HEVC clip and
//!    measures how fast the TV pulls it for ~10 s. Cached with a timestamp and the
//!    Wi-Fi link signature ([`load_capacity`], [`CapacityMeasurement::is_stale`]).
//! 3. **Link hints.** [`detect_link`] reads `iw dev <iface> link` or `/proc/net/wireless`.
//! 4. **Verdict.** [`verdict`] (pure) applies the thresholds and lists fixes.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::cast::{CastMode, CastProfile, VideoMode};
use crate::config::cache_dir;
use crate::dlna::Dlna;
use crate::error::{Error, Result};
use crate::media::{
    direct_bitrate, estimated_bitrate, stream_bitrate_guess, video_bitrate_estimate, AudioOut,
    MediaInfo, StreamKind, AC3_BITRATE,
};
use crate::server::{local_ip_for, FileServer, ServeSource};

// ---------------------------------------------------------------------------
// Thresholds

/// ✅ when capacity ≥ this × peak demand.
pub const OK_RATIO: f64 = 1.5;
/// Without a measurement, a peak above this (Mbit/s) is ❌ when the TV is on Wi-Fi
/// (or its network type is unknown).
pub const UNMEASURED_BAD_MBPS: f64 = 40.0;
/// Without a measurement, a peak above this (Mbit/s) is ⚠ (TV on Wi-Fi or unknown).
pub const UNMEASURED_RISKY_MBPS: f64 = 20.0;
/// Without a measurement, a peak above this (Mbit/s) is ❌ when the TV is wired: many
/// Samsung TVs have a 100 Mbit/s Ethernet port (~94 Mbps of TCP payload at best).
pub const UNMEASURED_WIRED_BAD_MBPS: f64 = 80.0;
/// Without a measurement, a peak above this (Mbit/s) is ⚠ when the TV is wired.
pub const UNMEASURED_WIRED_RISKY_MBPS: f64 = 50.0;
/// Provisional peak = average × this.
pub const PROVISIONAL_PEAK_FACTOR: f64 = 1.5;
/// A capacity measurement older than this is stale.
pub const CAPACITY_MAX_AGE: Duration = Duration::from_secs(24 * 3600);
/// Bitrate of the probe clip (Mbit/s).
pub const PROBE_CLIP_MBPS: u32 = 150;
/// Length of the probe clip (s).
pub const PROBE_CLIP_SECS: u32 = 20;
/// Measurement window (s).
pub const MEASURE_SECS: u64 = 10;

// ---------------------------------------------------------------------------
// Small helpers

/// Current time as unix seconds (for [`CapacityMeasurement::is_stale`]).
pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// FNV-1a 64-bit hash (stable across Rust versions, used for cache keys).
pub fn fnv1a64(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in data {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn mbps(bps: u64) -> f64 {
    bps as f64 / 1e6
}

// ---------------------------------------------------------------------------
// Packet profile (stream demand)

/// Per-stream bytes in 1-second bins, from `ffprobe -show_packets`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PacketProfile {
    /// Path analysed.
    pub path: PathBuf,
    /// File size at analysis time.
    pub size: u64,
    /// File mtime (ns since epoch) at analysis time.
    pub mtime_ns: u128,
    /// Bin width in seconds (always 1.0).
    pub bin_secs: f64,
    /// stream index → bytes per bin.
    pub bins: BTreeMap<u32, Vec<u64>>,
}

/// Bitrate statistics for a selection of streams.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct WindowStats {
    /// Average (bit/s).
    pub avg_bps: u64,
    /// Max over any 2 s window (bit/s).
    pub peak_2s_bps: u64,
    /// Max over any 10 s window (bit/s).
    pub peak_10s_bps: u64,
}

/// Max average bitrate over a sliding window of `w` bins of 1 s.
fn peak_window(series: &[u64], w: usize) -> u64 {
    if series.is_empty() {
        return 0;
    }
    let w = w.min(series.len()).max(1);
    let mut sum: u64 = series[..w].iter().sum();
    let mut best = sum;
    for i in w..series.len() {
        sum = sum + series[i] - series[i - w];
        best = best.max(sum);
    }
    best * 8 / w as u64
}

/// Stats for a byte-per-second series plus a constant bitrate component (bit/s).
pub fn series_stats(series: &[u64], constant_bps: u64) -> WindowStats {
    let n = series.len().max(1) as u64;
    let total: u64 = series.iter().sum();
    WindowStats {
        avg_bps: total * 8 / n + constant_bps,
        peak_2s_bps: peak_window(series, 2) + constant_bps,
        peak_10s_bps: peak_window(series, 10) + constant_bps,
    }
}

impl PacketProfile {
    /// Sum of the selected streams' bins.
    pub fn combined(&self, streams: &[u32]) -> Vec<u64> {
        let len = streams
            .iter()
            .filter_map(|s| self.bins.get(s))
            .map(Vec::len)
            .max()
            .unwrap_or(0);
        let mut out = vec![0u64; len];
        for s in streams {
            if let Some(b) = self.bins.get(s) {
                for (o, v) in out.iter_mut().zip(b) {
                    *o += v;
                }
            }
        }
        out
    }

    /// Window stats for the selected streams, plus a constant component.
    pub fn stats(&self, streams: &[u32], constant_bps: u64) -> WindowStats {
        series_stats(&self.combined(streams), constant_bps)
    }

    /// All streams that have packets.
    pub fn all_streams(&self) -> Vec<u32> {
        self.bins.keys().copied().collect()
    }
}

fn file_key(path: &Path) -> Result<(u64, u128, PathBuf)> {
    let abs = std::fs::canonicalize(path)?;
    let md = std::fs::metadata(&abs)?;
    let mtime = md
        .modified()
        .ok()
        .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    Ok((md.len(), mtime, abs))
}

fn profile_cache_path(abs: &Path, size: u64, mtime: u128) -> PathBuf {
    let key = fnv1a64(format!("{}|{size}|{mtime}", abs.display()).as_bytes());
    cache_dir().join(format!("demand-{key:016x}.json"))
}

/// Cached packet profile for this file, if the file has not changed since.
pub fn load_cached_profile(path: &Path) -> Option<PacketProfile> {
    let (size, mtime, abs) = file_key(path).ok()?;
    let text = std::fs::read_to_string(profile_cache_path(&abs, size, mtime)).ok()?;
    let p: PacketProfile = serde_json::from_str(&text).ok()?;
    (p.size == size && p.mtime_ns == mtime && p.path == abs).then_some(p)
}

/// Parse one `stream_index,pts_time,dts_time,size` CSV line.
pub fn parse_packet_line(line: &str) -> Option<(u32, Option<f64>, u64)> {
    let mut it = line.trim().split(',');
    let idx: u32 = it.next()?.parse().ok()?;
    let pts = it.next().and_then(|s| s.parse::<f64>().ok());
    let dts = it.next().and_then(|s| s.parse::<f64>().ok());
    let size: u64 = it.next()?.trim().parse().ok()?;
    Some((idx, pts.or(dts), size))
}

/// Run the packet analysis now (blocking). `progress` gets 0.0..=1.0;
/// set `cancel` to abort (returns [`Error::Cancelled`]). Saves to the cache on success.
pub fn analyze_packets(
    path: &Path,
    duration_secs: Option<f64>,
    cancel: &AtomicBool,
    progress: &dyn Fn(f32),
) -> Result<PacketProfile> {
    let (size, mtime, abs) = file_key(path)?;
    let mut child = Command::new("ffprobe")
        .args([
            "-v", "error", "-show_entries", "packet=stream_index,pts_time,dts_time,size",
            "-of", "csv=p=0",
        ])
        .arg(&abs)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| Error::Media(format!("cannot run ffprobe: {e}")))?;
    let out = child
        .stdout
        .take()
        .ok_or_else(|| Error::Media("ffprobe has no stdout".into()))?;
    let mut bins: BTreeMap<u32, Vec<u64>> = BTreeMap::new();
    let mut last_t: BTreeMap<u32, f64> = BTreeMap::new();
    let mut last_report = -1.0f32;
    let mut lines = 0u64;
    for line in BufReader::new(out).lines() {
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(Error::Cancelled);
        }
        let Ok(line) = line else { break };
        let Some((idx, t, sz)) = parse_packet_line(&line) else { continue };
        let t = t.or_else(|| last_t.get(&idx).copied()).unwrap_or(0.0).max(0.0);
        last_t.insert(idx, t);
        let bin = t.floor() as usize;
        if bin > 400_000 {
            continue; // nonsense timestamp (> 4.6 days)
        }
        let v = bins.entry(idx).or_default();
        if v.len() <= bin {
            v.resize(bin + 1, 0);
        }
        v[bin] += sz;
        lines += 1;
        if lines.is_multiple_of(2000) {
            if let Some(d) = duration_secs.filter(|d| *d > 0.0) {
                let p = ((t / d) as f32).clamp(0.0, 0.999);
                if p - last_report >= 0.01 {
                    last_report = p;
                    progress(p);
                }
            }
        }
    }
    let status = child.wait()?;
    if !status.success() && bins.is_empty() {
        return Err(Error::Media("ffprobe packet analysis failed".into()));
    }
    progress(1.0);
    let prof = PacketProfile {
        path: abs.clone(),
        size,
        mtime_ns: mtime,
        bin_secs: 1.0,
        bins,
    };
    if let Ok(text) = serde_json::to_string(&prof) {
        let _ = std::fs::write(profile_cache_path(&abs, size, mtime), text);
    }
    Ok(prof)
}

/// Handle to a background packet analysis.
#[derive(Clone, Debug)]
pub struct DemandJob {
    cancel: Arc<AtomicBool>,
    progress: Arc<Mutex<f32>>,
    done: Arc<AtomicBool>,
}

impl DemandJob {
    /// Abort the analysis (the `on_done` callback receives [`Error::Cancelled`]).
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }

    /// Last reported progress, 0.0..=1.0.
    pub fn progress(&self) -> f32 {
        self.progress.lock().map(|p| *p).unwrap_or(0.0)
    }

    /// True once `on_done` has been called.
    pub fn is_done(&self) -> bool {
        self.done.load(Ordering::SeqCst)
    }
}

/// Start the packet analysis on a worker thread (returns immediately).
///
/// A valid cache entry is returned without running ffprobe. Both callbacks run on the
/// worker thread: forward them to your UI thread.
pub fn analyze_in_background(
    path: PathBuf,
    duration_secs: Option<f64>,
    on_progress: impl Fn(f32) + Send + 'static,
    on_done: impl FnOnce(Result<Arc<PacketProfile>>) + Send + 'static,
) -> DemandJob {
    let job = DemandJob {
        cancel: Arc::new(AtomicBool::new(false)),
        progress: Arc::new(Mutex::new(0.0)),
        done: Arc::new(AtomicBool::new(false)),
    };
    let j = job.clone();
    let spawned = std::thread::Builder::new()
        .name("tv-netcheck-demand".into())
        .spawn(move || {
            let res = match load_cached_profile(&path) {
                Some(p) => Ok(p),
                None => {
                    let pr = j.progress.clone();
                    analyze_packets(&path, duration_secs, &j.cancel, &|p| {
                        if let Ok(mut g) = pr.lock() {
                            *g = p;
                        }
                        on_progress(p);
                    })
                }
            };
            if let Ok(mut g) = j.progress.lock() {
                *g = 1.0;
            }
            j.done.store(true, Ordering::SeqCst);
            on_done(res.map(Arc::new));
        });
    if spawned.is_err() {
        job.done.store(true, Ordering::SeqCst);
    }
    job
}

// ---------------------------------------------------------------------------
// Demand

/// What kind of fix a [`FixCandidate`] / [`Fix`] is.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum FixKind {
    /// TV-friendly mode with only the video and one audio track.
    DropExtraTracks {
        /// The profile that achieves it.
        profile: CastProfile,
    },
    /// TV-friendly mode with the video re-encoded at a lower bitrate (needs VAAPI).
    ReduceVideo {
        /// Target video bitrate.
        target_mbps: f64,
        /// The profile that achieves it.
        profile: CastProfile,
    },
    /// Copy the file to storage attached to the TV and play it there (no streaming).
    CopyToTv,
}

/// A possible alternative and its demand (computed by [`compute_demand`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FixCandidate {
    /// What to do.
    pub kind: FixKind,
    /// Human description.
    pub description: String,
    /// Demand of the alternative.
    pub stats: WindowStats,
    /// True if it needs `hevc_vaapi` (check [`crate::cast::vaapi_available`]).
    pub requires_vaapi: bool,
}

/// Network demand of one cast configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Demand {
    /// Average / peak bitrates.
    pub stats: WindowStats,
    /// True when estimated from averages (packet analysis not finished).
    pub provisional: bool,
    /// Input streams sent.
    pub streams: Vec<u32>,
    /// Lighter alternatives.
    pub fixes: Vec<FixCandidate>,
}

impl Demand {
    /// Peak used for the verdict (10 s window), Mbit/s.
    pub fn peak_mbps(&self) -> f64 {
        mbps(self.stats.peak_10s_bps)
    }
}

/// Demand of a configuration without fixes.
fn scenario_stats(info: &MediaInfo, mode: &CastMode, packets: Option<&PacketProfile>) -> (WindowStats, Vec<u32>, bool) {
    match mode {
        CastMode::Direct => {
            let streams: Vec<u32> = info.streams.iter().map(|s| s.index).collect();
            match packets {
                Some(p) => (p.stats(&p.all_streams(), 0), streams, false),
                None => {
                    let avg = direct_bitrate(info);
                    let peak = (avg as f64 * PROVISIONAL_PEAK_FACTOR) as u64;
                    (WindowStats { avg_bps: avg, peak_2s_bps: peak, peak_10s_bps: peak }, streams, true)
                }
            }
        }
        CastMode::TvFriendly(profile) => {
            let selected = profile.selected_streams(info);
            // Components that are constant bitrate after conversion.
            let mut constant = 0u64;
            let mut measured: Vec<u32> = Vec::new();
            for &i in &selected {
                let Some(s) = info.stream(i) else { continue };
                match s.kind {
                    StreamKind::Video => match profile.video {
                        VideoMode::Reduce { target_mbps } => constant += (target_mbps * 1e6) as u64,
                        VideoMode::Copy => measured.push(i),
                    },
                    StreamKind::Audio => match profile.audio_out(info) {
                        Some(AudioOut::Ac3) => constant += AC3_BITRATE,
                        _ => measured.push(i),
                    },
                    _ => measured.push(i),
                }
            }
            match packets {
                Some(p) => (p.stats(&measured, constant), selected, false),
                None => {
                    let var_avg: u64 = measured
                        .iter()
                        .filter_map(|i| info.stream(*i))
                        .map(|s| {
                            if s.kind == StreamKind::Video {
                                video_bitrate_estimate(info).unwrap_or(0)
                            } else {
                                stream_bitrate_guess(s).unwrap_or(0)
                            }
                        })
                        .sum();
                    let peak = (var_avg as f64 * PROVISIONAL_PEAK_FACTOR) as u64 + constant;
                    let est = estimated_bitrate(info, profile);
                    (
                        WindowStats { avg_bps: est.total_bps, peak_2s_bps: peak, peak_10s_bps: peak },
                        selected,
                        true,
                    )
                }
            }
        }
    }
}

/// Demand for casting `info` with `mode`, using the packet profile when available
/// (else provisional), including lighter alternatives as fix candidates.
pub fn compute_demand(info: &MediaInfo, mode: &CastMode, packets: Option<&PacketProfile>) -> Demand {
    let (stats, streams, provisional) = scenario_stats(info, mode, packets);
    let mut fixes = Vec::new();

    // 1. Video + one audio track only (TV-friendly, video copied).
    let base_profile = match mode {
        CastMode::TvFriendly(p) => CastProfile { video: VideoMode::Copy, subtitle: None, ..p.clone() },
        CastMode::Direct => CastProfile::recommended(info, false),
    };
    let lean = CastProfile { video: VideoMode::Copy, subtitle: None, ..base_profile.clone() };
    let (lean_stats, lean_streams, _) = scenario_stats(info, &CastMode::TvFriendly(lean.clone()), packets);
    if lean_streams.len() < streams.len() && lean_stats.peak_10s_bps < stats.peak_10s_bps {
        let dropped = streams.len() - lean_streams.len();
        let audio_label = lean
            .audio
            .and_then(|a| info.stream(a))
            .map(|a| a.label())
            .unwrap_or_else(|| "no".into());
        fixes.push(FixCandidate {
            kind: FixKind::DropExtraTracks { profile: lean.clone() },
            description: format!(
                "TV-friendly mode with only the video and the {audio_label} audio track ({dropped} other track{} dropped)",
                if dropped == 1 { "" } else { "s" }
            ),
            stats: lean_stats,
            requires_vaapi: false,
        });
    }

    // 1b. From a direct cast, the same lean profile for each other audio language (e.g.
    //     the only English track of a dubbed remux, converted to AC3 if it is DTS).
    if matches!(mode, CastMode::Direct) {
        let chosen_lang = lean.audio.and_then(|a| info.stream(a)).and_then(|a| a.language.clone());
        let mut seen: Vec<Option<String>> = vec![chosen_lang];
        for a in info.audio_streams() {
            if seen.contains(&a.language) || Some(a.index) == lean.audio {
                continue;
            }
            seen.push(a.language.clone());
            let prof = CastProfile { audio: Some(a.index), ..lean.clone() };
            let (st, _, _) = scenario_stats(info, &CastMode::TvFriendly(prof.clone()), packets);
            fixes.push(FixCandidate {
                kind: FixKind::DropExtraTracks { profile: prof },
                description: format!(
                    "TV-friendly mode with only the video and the {} audio track{}",
                    a.label(),
                    if crate::media::is_dts(&a.codec) { " (converted to AC3)" } else { "" }
                ),
                stats: st,
                requires_vaapi: false,
            });
        }
    }

    // 2. Reduced video bitrates.
    let current_video = video_bitrate_estimate(info).unwrap_or(0);
    for target in [40.0f64, 25.0, 15.0] {
        if (target * 1e6) as u64 >= current_video {
            continue;
        }
        if let CastMode::TvFriendly(p) = mode {
            if let VideoMode::Reduce { target_mbps } = p.video {
                if target >= target_mbps {
                    continue;
                }
            }
        }
        let prof = CastProfile { video: VideoMode::Reduce { target_mbps: target }, subtitle: None, ..base_profile.clone() };
        let (st, _, _) = scenario_stats(info, &CastMode::TvFriendly(prof.clone()), packets);
        fixes.push(FixCandidate {
            kind: FixKind::ReduceVideo { target_mbps: target, profile: prof },
            description: format!("Re-encode the video to {target:.0} Mbps HEVC (TV-friendly, one audio track)"),
            stats: st,
            requires_vaapi: true,
        });
    }

    // 3. Play from storage attached to the TV.
    fixes.push(FixCandidate {
        kind: FixKind::CopyToTv,
        description: "Copy the file to a USB drive/SSD plugged into the TV and play it there".into(),
        stats: WindowStats::default(),
        requires_vaapi: false,
    });

    Demand { stats, provisional, streams, fixes }
}

// ---------------------------------------------------------------------------
// Link hints

/// How the TV itself is connected, from [`crate::DeviceInfo::network_type`]
/// (see [`crate::DeviceInfo::tv_network`]). `None` where it is used means unknown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TvNetwork {
    /// `networkType: wireless`.
    Wireless,
    /// `networkType: wired`.
    Wired,
}

impl TvNetwork {
    /// Parse the TV's `networkType` (`wireless` / `wired`, case-insensitive); anything
    /// else is unknown (`None`).
    pub fn from_network_type(s: &str) -> Option<TvNetwork> {
        match s.trim().to_ascii_lowercase().as_str() {
            "wireless" | "wifi" | "wi-fi" => Some(TvNetwork::Wireless),
            "wired" | "ethernet" => Some(TvNetwork::Wired),
            _ => None,
        }
    }
}

/// This PC's network link towards the TV.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct LinkInfo {
    /// Interface name (`wlan0`, `enp3s0`).
    pub iface: String,
    /// True for Wi-Fi.
    pub wireless: bool,
    /// Associated access point BSSID.
    pub bssid: Option<String>,
    /// SSID.
    pub ssid: Option<String>,
    /// Frequency in MHz.
    pub freq_mhz: Option<f64>,
    /// Signal in dBm.
    pub signal_dbm: Option<i32>,
    /// PHY TX rate in Mbit/s.
    pub tx_mbps: Option<f64>,
    /// PHY RX rate in Mbit/s.
    pub rx_mbps: Option<f64>,
}

impl LinkInfo {
    /// `2.4 GHz`, `5 GHz` or `6 GHz` (Wi-Fi only).
    pub fn band(&self) -> Option<&'static str> {
        let f = self.freq_mhz?;
        Some(if f < 3000.0 {
            "2.4 GHz"
        } else if f < 5925.0 {
            "5 GHz"
        } else {
            "6 GHz"
        })
    }

    /// Identity of the link for cache invalidation (iface + AP + band).
    pub fn signature(&self) -> String {
        if self.wireless {
            format!(
                "{}|{}|{}",
                self.iface,
                self.bssid.as_deref().or(self.ssid.as_deref()).unwrap_or("?"),
                self.band().unwrap_or("?")
            )
        } else {
            format!("{}|wired", self.iface)
        }
    }

    /// One-line description, e.g. `Wi-Fi 5 GHz "Home", −52 dBm, 866 Mbit/s`.
    pub fn describe(&self) -> String {
        if !self.wireless {
            return format!("wired ({})", self.iface);
        }
        let mut s = format!("Wi-Fi {}", self.band().unwrap_or(""));
        if let Some(ss) = &self.ssid {
            s.push_str(&format!(" \"{ss}\""));
        }
        if let Some(sig) = self.signal_dbm {
            s.push_str(&format!(", {sig} dBm"));
        }
        if let Some(r) = self.tx_mbps.or(self.rx_mbps) {
            s.push_str(&format!(", {r:.0} Mbit/s PHY"));
        }
        s
    }
}

/// Parse `iw dev <iface> link` output.
pub fn parse_iw_link(iface: &str, text: &str) -> Option<LinkInfo> {
    if text.contains("Not connected") {
        return None;
    }
    let mut l = LinkInfo { iface: iface.to_string(), wireless: true, ..Default::default() };
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("Connected to ") {
            l.bssid = rest.split_whitespace().next().map(str::to_string);
        } else if let Some(v) = line.strip_prefix("SSID:") {
            l.ssid = Some(v.trim().to_string());
        } else if let Some(v) = line.strip_prefix("freq:") {
            l.freq_mhz = v.trim().parse().ok();
        } else if let Some(v) = line.strip_prefix("signal:") {
            l.signal_dbm = v.split_whitespace().next().and_then(|n| n.parse().ok());
        } else if let Some(v) = line.strip_prefix("tx bitrate:") {
            l.tx_mbps = v.split_whitespace().next().and_then(|n| n.parse().ok());
        } else if let Some(v) = line.strip_prefix("rx bitrate:") {
            l.rx_mbps = v.split_whitespace().next().and_then(|n| n.parse().ok());
        }
    }
    l.bssid.as_ref()?;
    Some(l)
}

/// Parse `/proc/net/wireless` for `iface` (signal level only).
pub fn parse_proc_wireless(iface: &str, text: &str) -> Option<LinkInfo> {
    for line in text.lines().skip(2) {
        let (name, rest) = line.trim().split_once(':')?;
        if name.trim() != iface {
            continue;
        }
        let cols: Vec<&str> = rest.split_whitespace().collect();
        let level = cols.get(2)?.trim_end_matches('.').parse::<f64>().ok()?;
        return Some(LinkInfo {
            iface: iface.to_string(),
            wireless: true,
            signal_dbm: Some(level as i32),
            ..Default::default()
        });
    }
    None
}

/// Interface of the default route from `/proc/net/route` text.
pub fn parse_default_route_iface(text: &str) -> Option<String> {
    text.lines()
        .skip(1)
        .map(|l| l.split_whitespace().collect::<Vec<_>>())
        .find(|c| c.len() > 2 && c[1] == "00000000")
        .map(|c| c[0].to_string())
}

/// Detect this PC's link (local only: `/proc`, `/sys`, `iw`; no network traffic).
pub fn detect_link() -> Option<LinkInfo> {
    let route = std::fs::read_to_string("/proc/net/route").ok()?;
    let iface = parse_default_route_iface(&route)?;
    let wireless = Path::new(&format!("/sys/class/net/{iface}/wireless")).exists()
        || std::fs::read_to_string("/proc/net/wireless")
            .map(|t| t.contains(&format!("{iface}:")))
            .unwrap_or(false);
    if !wireless {
        return Some(LinkInfo { iface, wireless: false, ..Default::default() });
    }
    if let Ok(out) = Command::new("iw").args(["dev", &iface, "link"]).stderr(Stdio::null()).output() {
        if let Some(l) = parse_iw_link(&iface, &String::from_utf8_lossy(&out.stdout)) {
            return Some(l);
        }
    }
    std::fs::read_to_string("/proc/net/wireless")
        .ok()
        .and_then(|t| parse_proc_wireless(&iface, &t))
        .or(Some(LinkInfo { iface, wireless: true, ..Default::default() }))
}

// ---------------------------------------------------------------------------
// Capacity

/// Result of a network capacity test.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CapacityMeasurement {
    /// Average rate the TV pulled during the window (Mbit/s).
    pub mbps: f64,
    /// Best 1-second rate (Mbit/s).
    pub peak_mbps: f64,
    /// Bytes pulled in the window.
    pub bytes: u64,
    /// Window length (s).
    pub secs: f64,
    /// When measured (unix seconds).
    pub measured_at: u64,
    /// [`LinkInfo::signature`] at measurement time.
    pub link_signature: Option<String>,
}

impl CapacityMeasurement {
    /// Stale if older than 24 h or measured on a different link.
    pub fn is_stale(&self, now_unix: u64, current: Option<&LinkInfo>) -> bool {
        let old = now_unix.saturating_sub(self.measured_at) > CAPACITY_MAX_AGE.as_secs();
        let changed = match (current, &self.link_signature) {
            (Some(l), Some(sig)) => &l.signature() != sig,
            _ => false,
        };
        old || changed
    }
}

fn capacity_cache_path() -> PathBuf {
    cache_dir().join("capacity.json")
}

/// Last capacity measurement, if any.
pub fn load_capacity() -> Option<CapacityMeasurement> {
    serde_json::from_str(&std::fs::read_to_string(capacity_cache_path()).ok()?).ok()
}

/// Path of the cached probe clip.
pub fn probe_clip_path() -> PathBuf {
    cache_dir().join(format!("netprobe-{PROBE_CLIP_MBPS}M-{PROBE_CLIP_SECS}s.mp4"))
}

/// ffmpeg arguments for generating the probe clip (pure). `vaapi`: Some(device) to
/// encode with `hevc_vaapi`, None for `libx265`.
pub fn probe_clip_args(out: &Path, vaapi: Option<&Path>) -> Vec<String> {
    let rate = format!("{PROBE_CLIP_MBPS}M");
    let mut a: Vec<String> = vec!["-hide_banner".into(), "-nostdin".into(), "-loglevel".into(), "error".into(), "-y".into()];
    if let Some(d) = vaapi {
        a.extend(["-vaapi_device".into(), d.display().to_string()]);
    }
    a.extend([
        "-f".into(),
        "lavfi".into(),
        "-i".into(),
        // Temporal uniform noise: incompressible, so the encoder spends the full bitrate.
        "color=c=gray:s=1920x1080:r=30,noise=alls=100:allf=t+u".into(),
        "-t".into(),
        PROBE_CLIP_SECS.to_string(),
    ]);
    match vaapi {
        Some(_) => a.extend([
            "-vf".into(), "format=nv12,hwupload".into(), "-c:v".into(), "hevc_vaapi".into(),
            "-rc_mode".into(), "CBR".into(),
        ]),
        None => a.extend([
            "-pix_fmt".into(), "yuv420p".into(), "-c:v".into(), "libx265".into(),
            "-preset".into(), "ultrafast".into(), "-x265-params".into(), "log-level=error".into(),
        ]),
    }
    a.extend([
        "-b:v".into(), rate.clone(), "-maxrate".into(), rate.clone(), "-bufsize".into(), rate,
        "-tag:v".into(), "hvc1".into(), "-movflags".into(), "+faststart".into(),
        "-progress".into(), "pipe:1".into(),
    ]);
    a.push(out.display().to_string());
    a
}

/// Progress of [`measure_capacity`].
#[derive(Debug, Clone, PartialEq)]
pub enum CapacityProgress {
    /// Generating the probe clip (first run only), 0.0..=1.0.
    GeneratingClip(f32),
    /// Asking the TV to play the probe clip.
    Starting,
    /// Measuring: elapsed seconds and the current average (Mbit/s).
    Measuring {
        /// Seconds into the window.
        elapsed: f64,
        /// Average so far.
        mbps: f64,
    },
    /// Done.
    Done,
}

fn run_clip_ffmpeg(
    out: &Path,
    vaapi: Option<&Path>,
    cancel: &AtomicBool,
    progress: &dyn Fn(CapacityProgress),
) -> Result<()> {
    let mut child = Command::new("ffmpeg")
        .args(probe_clip_args(out, vaapi))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| Error::Media(format!("cannot run ffmpeg: {e}")))?;
    if let Some(o) = child.stdout.take() {
        let total_us = PROBE_CLIP_SECS as f64 * 1e6;
        for line in BufReader::new(o).lines().map_while(|l| l.ok()) {
            if cancel.load(Ordering::SeqCst) {
                let _ = child.kill();
                let _ = child.wait();
                let _ = std::fs::remove_file(out);
                return Err(Error::Cancelled);
            }
            if let Some(v) = line.strip_prefix("out_time_us=").or_else(|| line.strip_prefix("out_time_ms=")) {
                if let Ok(us) = v.trim().parse::<f64>() {
                    progress(CapacityProgress::GeneratingClip((us / total_us).clamp(0.0, 1.0) as f32));
                }
            }
        }
    }
    let st = child.wait()?;
    if cancel.load(Ordering::SeqCst) {
        let _ = std::fs::remove_file(out);
        return Err(Error::Cancelled);
    }
    let expected = PROBE_CLIP_MBPS as u64 * 1_000_000 / 8 * PROBE_CLIP_SECS as u64;
    let size = std::fs::metadata(out).map(|m| m.len()).unwrap_or(0);
    if !st.success() || size < expected / 2 {
        let _ = std::fs::remove_file(out);
        return Err(Error::Media(format!(
            "probe clip encode failed or too small ({} MB)",
            size / 1_000_000
        )));
    }
    Ok(())
}

/// Generate the probe clip if it is not cached yet (VAAPI first, then libx265).
/// Setting `cancel` kills ffmpeg and returns [`Error::Cancelled`] (no partial clip is kept).
pub fn ensure_probe_clip(cancel: &AtomicBool, progress: &dyn Fn(CapacityProgress)) -> Result<PathBuf> {
    let out = probe_clip_path();
    if out.is_file() {
        return Ok(out);
    }
    let tmp = out.with_extension("part.mp4");
    let mut res = Err(Error::Media("no encoder".into()));
    if crate::media::vaapi_available() {
        if let Some(dev) = crate::media::vaapi_device() {
            res = run_clip_ffmpeg(&tmp, Some(&dev), cancel, progress);
        }
    }
    if res.is_err() && !matches!(res, Err(Error::Cancelled)) {
        res = run_clip_ffmpeg(&tmp, None, cancel, progress);
    }
    res?;
    std::fs::rename(&tmp, &out)?;
    Ok(out)
}

/// Measure how fast the TV can pull data from this PC: casts the probe clip (a noise
/// test pattern appears on the TV), measures ~10 s from the first byte, stops playback,
/// and caches the result. Blocking (~15 s, longer on the first run).
///
/// Cancellable like [`analyze_in_background`]: set `cancel` from another thread and the
/// call returns [`Error::Cancelled`] within ~0.5 s (ffmpeg is killed if the probe clip is
/// still being generated). Playback on the TV is stopped and no result is saved.
pub fn measure_capacity(
    dlna: &Dlna,
    cancel: &AtomicBool,
    progress: &dyn Fn(CapacityProgress),
) -> Result<CapacityMeasurement> {
    let cancelled = || cancel.load(Ordering::SeqCst);
    let clip = ensure_probe_clip(cancel, progress)?;
    if cancelled() {
        return Err(Error::Cancelled);
    }
    let ip = local_ip_for(dlna.host())?;
    let srv = FileServer::start(
        ServeSource::File(clip.clone()),
        "video/mp4",
        "tv-remote-network-test.mp4",
        std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
    )?;
    progress(CapacityProgress::Starting);
    let url = srv.url(ip);
    let result = (|| {
        dlna.play_uri(&url, "Network test", "video/mp4", None)?;
        let wait_start = Instant::now();
        let t0 = loop {
            if cancelled() {
                return Err(Error::Cancelled);
            }
            if let Some(t) = srv.first_byte_at() {
                break t;
            }
            if wait_start.elapsed() > Duration::from_secs(20) {
                return Err(Error::Timeout("the TV did not start downloading the test clip".into()));
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        let b0 = 0u64; // bytes are counted from the first byte on
        let mut peak = 0.0f64;
        let mut last_b = srv.total_bytes();
        let mut last_t = Instant::now();
        while t0.elapsed() < Duration::from_secs(MEASURE_SECS) {
            if cancelled() {
                return Err(Error::Cancelled);
            }
            std::thread::sleep(Duration::from_millis(250));
            let b = srv.total_bytes();
            let now = Instant::now();
            let dt = now.duration_since(last_t).as_secs_f64();
            if dt >= 1.0 {
                peak = peak.max((b - last_b) as f64 * 8.0 / dt / 1e6);
                last_b = b;
                last_t = now;
            }
            let el = t0.elapsed().as_secs_f64();
            progress(CapacityProgress::Measuring { elapsed: el, mbps: (b - b0) as f64 * 8.0 / el.max(0.001) / 1e6 });
            if srv.is_stopped() {
                break;
            }
        }
        let secs = t0.elapsed().as_secs_f64();
        let bytes = srv.total_bytes() - b0;
        Ok(CapacityMeasurement {
            mbps: bytes as f64 * 8.0 / secs.max(0.001) / 1e6,
            peak_mbps: peak,
            bytes,
            secs,
            measured_at: now_unix(),
            link_signature: detect_link().map(|l| l.signature()),
        })
    })();
    let _ = dlna.stop();
    srv.stop();
    let m = result?;
    if cancelled() {
        return Err(Error::Cancelled);
    }
    if let Ok(text) = serde_json::to_string_pretty(&m) {
        let _ = std::fs::write(capacity_cache_path(), text);
    }
    progress(CapacityProgress::Done);
    Ok(m)
}

// ---------------------------------------------------------------------------
// Verdict

/// Verdict level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum VerdictLevel {
    /// ✅ Should play smoothly.
    Ok,
    /// ⚠ Might stutter.
    Risky,
    /// ❌ Will stutter.
    Bad,
}

impl VerdictLevel {
    /// `✅`, `⚠` or `❌`.
    pub fn badge(self) -> &'static str {
        match self {
            VerdictLevel::Ok => "✅",
            VerdictLevel::Risky => "⚠",
            VerdictLevel::Bad => "❌",
        }
    }
}

/// A concrete fix with its resulting bitrate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Fix {
    /// What to do (contains the profile to apply where relevant).
    pub kind: FixKind,
    /// Human description.
    pub description: String,
    /// Peak demand after the fix (Mbit/s, 10 s window).
    pub resulting_mbps: f64,
    /// Level the fix would reach.
    pub resulting_level: VerdictLevel,
    /// Needs VAAPI.
    pub requires_vaapi: bool,
}

/// Result of [`verdict`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Verdict {
    /// Overall level.
    pub level: VerdictLevel,
    /// Multi-sentence explanation for the UI.
    pub explanation: String,
    /// Fixes (only for ⚠/❌), best first.
    pub fixes: Vec<Fix>,
    /// Peak demand used (Mbit/s).
    pub demand_mbps: f64,
    /// Capacity used (Mbit/s), if measured.
    pub capacity_mbps: Option<f64>,
    /// The capacity measurement is stale (> 24 h or another link).
    pub capacity_stale: bool,
}

/// The unmeasured (❌, ⚠) thresholds in Mbit/s for the TV's network type: Wi-Fi
/// ([`UNMEASURED_BAD_MBPS`]) when it is wireless or unknown, else the wired ones.
pub fn unmeasured_thresholds(tv: Option<TvNetwork>) -> (f64, f64) {
    match tv {
        Some(TvNetwork::Wired) => (UNMEASURED_WIRED_BAD_MBPS, UNMEASURED_WIRED_RISKY_MBPS),
        Some(TvNetwork::Wireless) | None => (UNMEASURED_BAD_MBPS, UNMEASURED_RISKY_MBPS),
    }
}

/// Level for given stats against an optional capacity (Mbit/s). Pure.
///
/// * measured: ✅ cap ≥ 1.5 × peak10 (and cap ≥ peak2), ⚠ cap ≥ peak10, ❌ below.
/// * unmeasured, TV on Wi-Fi or unknown (`tv` = `None`): ❌ peak10 > 40 Mbps,
///   ⚠ > 20 Mbps, else ✅.
/// * unmeasured, TV wired: ❌ peak10 > 80 Mbps, ⚠ > 50 Mbps, else ✅.
pub fn level_for(stats: &WindowStats, capacity_mbps: Option<f64>, tv: Option<TvNetwork>) -> VerdictLevel {
    let p10 = mbps(stats.peak_10s_bps);
    let p2 = mbps(stats.peak_2s_bps);
    match capacity_mbps {
        Some(cap) => {
            if p10 <= 0.0 || (cap >= OK_RATIO * p10 && cap >= p2) {
                VerdictLevel::Ok
            } else if cap >= p10 {
                VerdictLevel::Risky
            } else {
                VerdictLevel::Bad
            }
        }
        None => {
            let (bad, risky) = unmeasured_thresholds(tv);
            if p10 > bad {
                VerdictLevel::Bad
            } else if p10 > risky {
                VerdictLevel::Risky
            } else {
                VerdictLevel::Ok
            }
        }
    }
}

/// The verdict shown before casting. Pure except for reading the clock to decide
/// whether `capacity` is stale (a stale measurement is still used, but flagged).
///
/// `link` is this PC's link ([`detect_link`]); `tv` is how the TV is connected
/// ([`crate::DeviceInfo::tv_network`]), `None` if unknown. Without a capacity
/// measurement, a wireless or unknown TV gets the Wi-Fi thresholds; the explanation
/// only states that the TV is on Wi-Fi when it is known to be.
pub fn verdict(
    demand: &Demand,
    capacity: Option<&CapacityMeasurement>,
    link: Option<&LinkInfo>,
    tv: Option<TvNetwork>,
) -> Verdict {
    let cap = capacity.map(|c| c.mbps);
    let stale = capacity.is_some_and(|c| c.is_stale(now_unix(), link));
    let level = level_for(&demand.stats, cap, tv);
    let p10 = demand.peak_mbps();
    let p2 = mbps(demand.stats.peak_2s_bps);
    let avg = mbps(demand.stats.avg_bps);

    let mut ex = if demand.provisional {
        format!("Estimated peak demand {p10:.1} Mbps (average {avg:.1} × {PROVISIONAL_PEAK_FACTOR}; the detailed analysis has not finished).")
    } else {
        format!("Peak demand {p10:.1} Mbps over 10 s ({p2:.1} Mbps over 2 s, average {avg:.1} Mbps).")
    };
    match cap {
        Some(c) => {
            ex.push_str(&format!(" Measured network capacity {c:.1} Mbps ({:.1}× the peak", c / p10.max(0.001)));
            ex.push_str(if stale { "; measurement is stale, re-run the network test)." } else { ")." });
        }
        None => {
            ex.push_str(" Network capacity not measured yet: run the network test.");
            let (bad, _) = unmeasured_thresholds(tv);
            ex.push_str(&match tv {
                Some(TvNetwork::Wireless) => format!(
                    " Until then, more than {bad:.0} Mbps is assumed not to play smoothly over the TV's Wi-Fi."
                ),
                Some(TvNetwork::Wired) => format!(
                    " Until then, more than {bad:.0} Mbps is assumed not to play smoothly (the TV is wired, and many TVs have a 100 Mbit/s Ethernet port)."
                ),
                None => format!(
                    " The TV's connection type is unknown, so it is assumed to be on Wi-Fi, where more than {bad:.0} Mbps usually does not play smoothly."
                ),
            });
        }
    }
    if let Some(l) = link {
        ex.push_str(&format!(" This PC: {}.", l.describe()));
    }
    let pc_wireless = link.is_some_and(|l| l.wireless);
    match (pc_wireless, tv) {
        (true, Some(TvNetwork::Wireless)) => {
            ex.push_str(" The TV is on Wi-Fi too, so the stream crosses the access point twice.")
        }
        (true, Some(TvNetwork::Wired)) => {
            ex.push_str(" The TV is wired, so only this PC's Wi-Fi hop limits the stream.")
        }
        (true, None) => {
            ex.push_str(" If the TV is on Wi-Fi too, the stream crosses the access point twice.")
        }
        (false, Some(TvNetwork::Wireless)) => ex.push_str(" The TV is on Wi-Fi."),
        (false, Some(TvNetwork::Wired)) if link.is_some() => ex.push_str(" The TV is wired too."),
        _ => {}
    }
    if let Some(r) = link.filter(|l| l.wireless).and_then(|l| l.tx_mbps.or(l.rx_mbps)) {
        // Rule of thumb: usable TCP throughput ≈ half the PHY rate, halved again when
        // the TV is on Wi-Fi too (through the AP twice).
        let usable = match tv {
            Some(TvNetwork::Wired) => r / 2.0,
            _ => r / 4.0,
        };
        if usable < p10 {
            let hedge = if tv.is_none() { " if the TV is on Wi-Fi" } else { "" };
            ex.push_str(&format!(" A {r:.0} Mbit/s PHY rate typically delivers about {usable:.0} Mbps here{hedge}."));
        }
    }
    ex.push_str(match level {
        VerdictLevel::Ok => " Should play smoothly.",
        VerdictLevel::Risky => " Might stutter in busy scenes.",
        VerdictLevel::Bad => " Will most likely stutter.",
    });

    let mut fixes = Vec::new();
    if level != VerdictLevel::Ok {
        for f in &demand.fixes {
            let rl = level_for(&f.stats, cap, tv);
            if rl < level || matches!(f.kind, FixKind::CopyToTv) {
                fixes.push(Fix {
                    kind: f.kind.clone(),
                    description: f.description.clone(),
                    resulting_mbps: mbps(f.stats.peak_10s_bps),
                    resulting_level: rl,
                    requires_vaapi: f.requires_vaapi,
                });
            }
        }
        // Best level first; among equals, the one keeping the most quality (highest bitrate).
        fixes.sort_by(|a, b| {
            a.resulting_level
                .cmp(&b.resulting_level)
                .then(b.resulting_mbps.partial_cmp(&a.resulting_mbps).unwrap_or(std::cmp::Ordering::Equal))
        });
        // Keep "copy to the TV" last: it is the most effort.
        fixes.sort_by_key(|f| matches!(f.kind, FixKind::CopyToTv));
    }

    Verdict { level, explanation: ex, fixes, demand_mbps: p10, capacity_mbps: cap, capacity_stale: stale }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::tests::interstellar;

    #[test]
    fn windows() {
        // 10 s at 1 MB/s with a 2 s burst of 5 MB/s.
        let mut s = vec![1_000_000u64; 20];
        s[5] = 5_000_000;
        s[6] = 5_000_000;
        let st = series_stats(&s, 0);
        assert_eq!(st.peak_2s_bps, 40_000_000);
        assert_eq!(st.peak_10s_bps, (18_000_000u64) * 8 / 10);
        assert_eq!(st.avg_bps, 28_000_000 * 8 / 20);
        assert_eq!(series_stats(&[], 640_000).peak_10s_bps, 640_000);
        assert_eq!(parse_packet_line("3,12.5,12.4,4567"), Some((3, Some(12.5), 4567)));
        assert_eq!(parse_packet_line("3,N/A,12.4,4567"), Some((3, Some(12.4), 4567)));
        assert_eq!(parse_packet_line("garbage"), None);
    }

    fn cap(mbps: f64) -> CapacityMeasurement {
        CapacityMeasurement {
            mbps,
            peak_mbps: mbps,
            bytes: 0,
            secs: 10.0,
            measured_at: now_unix(),
            link_signature: None,
        }
    }

    #[test]
    fn interstellar_provisional() {
        let m = interstellar();
        let d = compute_demand(&m, &CastMode::Direct, None);
        assert!(d.provisional);
        assert!((d.peak_mbps() - 113.7).abs() < 0.1, "{}", d.peak_mbps());

        // Not measured → ❌ (provisional > 40 Mbps on Wi-Fi).
        let v = verdict(&d, None, None, None);
        assert_eq!(v.level, VerdictLevel::Bad);

        // Measured 90 Mbps → ❌, with fixes.
        let v = verdict(&d, Some(&cap(90.0)), None, None);
        assert_eq!(v.level, VerdictLevel::Bad);
        let drop = v.fixes.iter().find(|f| matches!(f.kind, FixKind::DropExtraTracks { .. })).unwrap();
        // video 51.7 + AC3 0.64 → ×1.5 provisional ≈ 78.5 Mbps → ⚠ at 90 Mbps.
        assert!((drop.resulting_mbps - 78.5).abs() < 0.2, "{}", drop.resulting_mbps);
        assert_eq!(drop.resulting_level, VerdictLevel::Risky);
        let reduce = v.fixes.iter().find(|f| matches!(f.kind, FixKind::ReduceVideo { target_mbps, .. } if target_mbps == 40.0)).unwrap();
        assert_eq!(reduce.resulting_level, VerdictLevel::Ok);
        assert!((reduce.resulting_mbps - 41.0).abs() < 0.1);
        assert_eq!(v.fixes[0].resulting_level, VerdictLevel::Ok);
        assert!(matches!(v.fixes.last().unwrap().kind, FixKind::CopyToTv));

        // Plenty of capacity → ✅ with no fixes.
        let v = verdict(&d, Some(&cap(300.0)), None, None);
        assert_eq!(v.level, VerdictLevel::Ok);
        assert!(v.fixes.is_empty());
        // 1.0–1.5× → ⚠.
        assert_eq!(verdict(&d, Some(&cap(130.0)), None, None).level, VerdictLevel::Risky);
    }

    #[test]
    fn interstellar_measured_profile() {
        let m = interstellar();
        // Synthetic packet profile: constant rates, video with a 10 s burst at 70 Mbps.
        let secs = 600usize;
        let mut bins = BTreeMap::new();
        let mut video = vec![51_700_000u64 / 8; secs];
        for v in video.iter_mut().skip(100).take(10) {
            *v = 70_000_000 / 8;
        }
        bins.insert(0, video);
        bins.insert(1, vec![640_000 / 8; secs]);
        for i in 2..9 {
            bins.insert(i, vec![3_340_000 / 8; secs]);
        }
        let p = PacketProfile { path: m.path.clone(), size: 0, mtime_ns: 0, bin_secs: 1.0, bins };
        let d = compute_demand(&m, &CastMode::Direct, Some(&p));
        assert!(!d.provisional);
        assert!((d.peak_mbps() - (70.0 + 0.64 + 7.0 * 3.34)).abs() < 0.1);
        // TV-friendly, English DTS → AC3: video peak 70 + 0.64.
        let prof = CastProfile { audio: Some(8), ..CastProfile::recommended(&m, false) };
        let d2 = compute_demand(&m, &CastMode::TvFriendly(prof), Some(&p));
        assert!((d2.peak_mbps() - 70.64).abs() < 0.1);
        assert_eq!(verdict(&d2, Some(&cap(90.0)), None, None).level, VerdictLevel::Risky);
        assert_eq!(verdict(&d2, Some(&cap(110.0)), None, None).level, VerdictLevel::Ok);
    }

    #[test]
    fn unmeasured_thresholds_and_staleness() {
        let s = |m: u64| WindowStats { avg_bps: m * 1_000_000, peak_2s_bps: m * 1_000_000, peak_10s_bps: m * 1_000_000 };
        for tv in [None, Some(TvNetwork::Wireless)] {
            assert_eq!(level_for(&s(15), None, tv), VerdictLevel::Ok);
            assert_eq!(level_for(&s(30), None, tv), VerdictLevel::Risky);
            assert_eq!(level_for(&s(45), None, tv), VerdictLevel::Bad);
        }
        let wired = Some(TvNetwork::Wired);
        assert_eq!(level_for(&s(45), None, wired), VerdictLevel::Ok);
        assert_eq!(level_for(&s(60), None, wired), VerdictLevel::Risky);
        assert_eq!(level_for(&s(90), None, wired), VerdictLevel::Bad);
        // A measurement overrides the network-type assumption.
        assert_eq!(level_for(&s(45), Some(100.0), None), VerdictLevel::Ok);
        let mut c = cap(100.0);
        assert!(!c.is_stale(now_unix(), None));
        c.measured_at -= 25 * 3600;
        assert!(c.is_stale(now_unix(), None));
        let link = LinkInfo { iface: "wlan0".into(), wireless: true, bssid: Some("aa".into()), freq_mhz: Some(5180.0), ..Default::default() };
        let c2 = CapacityMeasurement { link_signature: Some("wlan0|bb|5 GHz".into()), ..cap(100.0) };
        assert!(c2.is_stale(now_unix(), Some(&link)));
    }

    #[test]
    fn verdict_wording_follows_tv_network() {
        let m = interstellar();
        let d = compute_demand(&m, &CastMode::Direct, None);
        let wifi = LinkInfo { iface: "wlan0".into(), wireless: true, tx_mbps: Some(400.0), ..Default::default() };
        let wired_pc = LinkInfo { iface: "enp3s0".into(), wireless: false, ..Default::default() };

        // PC on Wi-Fi, TV type unknown: hedged, Wi-Fi thresholds.
        let v = verdict(&d, None, Some(&wifi), None);
        assert_eq!(v.level, VerdictLevel::Bad);
        assert!(!v.explanation.contains("The TV is on Wi-Fi too"), "{}", v.explanation);
        assert!(v.explanation.contains("If the TV is on Wi-Fi too"), "{}", v.explanation);
        assert!(v.explanation.contains("connection type is unknown"), "{}", v.explanation);
        assert!(v.explanation.contains("about 100 Mbps here if the TV is on Wi-Fi"), "{}", v.explanation);

        // PC on Wi-Fi, TV on Wi-Fi: stated plainly.
        let v = verdict(&d, None, Some(&wifi), Some(TvNetwork::Wireless));
        assert!(v.explanation.contains("The TV is on Wi-Fi too"), "{}", v.explanation);
        assert!(v.explanation.contains("over the TV's Wi-Fi"), "{}", v.explanation);

        // PC on Wi-Fi, TV wired: one hop, wired thresholds (113.7 Mbps is still ❌).
        let v = verdict(&d, None, Some(&wifi), Some(TvNetwork::Wired));
        assert_eq!(v.level, VerdictLevel::Bad);
        assert!(!v.explanation.contains("Wi-Fi too"), "{}", v.explanation);
        assert!(v.explanation.contains("The TV is wired"), "{}", v.explanation);
        assert!(!v.explanation.contains("PHY rate"), "{}", v.explanation); // 400/2 ≥ 113.7

        // PC wired, TV on Wi-Fi.
        let v = verdict(&d, None, Some(&wired_pc), Some(TvNetwork::Wireless));
        assert!(v.explanation.contains("The TV is on Wi-Fi."), "{}", v.explanation);
        assert!(!v.explanation.contains("too"), "{}", v.explanation);

        assert_eq!(TvNetwork::from_network_type("Wireless"), Some(TvNetwork::Wireless));
        assert_eq!(TvNetwork::from_network_type("wired"), Some(TvNetwork::Wired));
        assert_eq!(TvNetwork::from_network_type("?"), None);
    }

    #[test]
    fn link_parsing() {
        let iw = "Connected to 11:22:33:44:55:66 (on wlan0)\n\tSSID: Maison\n\tfreq: 5180.0\n\tRX: 1 bytes (1 packets)\n\
                  \tsignal: -52 dBm\n\trx bitrate: 866.7 MBit/s VHT-MCS 9 80MHz\n\ttx bitrate: 780.0 MBit/s VHT-MCS 8\n";
        let l = parse_iw_link("wlan0", iw).unwrap();
        assert_eq!(l.ssid.as_deref(), Some("Maison"));
        assert_eq!(l.band(), Some("5 GHz"));
        assert_eq!(l.signal_dbm, Some(-52));
        assert_eq!(l.tx_mbps, Some(780.0));
        assert_eq!(l.signature(), "wlan0|11:22:33:44:55:66|5 GHz");
        assert!(parse_iw_link("wlan0", "Not connected.").is_none());
        let pw = "Inter-| sta-|   Quality        |   Discarded packets               | Missed | WE\n \
                  face | tus | link level noise |  nwid  crypt   frag  retry   misc | beacon | 22\n\
                  wlan0: 0000   58.  -52.  -256        0      0      0      0     17        0\n";
        assert_eq!(parse_proc_wireless("wlan0", pw).unwrap().signal_dbm, Some(-52));
        let route = "Iface\tDestination\tGateway\tFlags\nwlan0\t0000A8C0\t00000000\t0001\nwlan0\t00000000\t0100A8C0\t0003\n";
        assert_eq!(parse_default_route_iface(route).as_deref(), Some("wlan0"));
        let args = probe_clip_args(Path::new("/tmp/x.mp4"), None).join(" ");
        assert!(args.contains("-c:v libx265") && args.contains("-b:v 150M") && args.ends_with("/tmp/x.mp4"));
    }
}
