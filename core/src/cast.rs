//! Casting: direct (file server + DLNA, seekable) or TV-friendly (ffmpeg → live
//! chunked stream), plus the [`CastSession`] handle used to monitor and control it.
//!
//! Media analysis types (`MediaInfo`, `check_compat`, `CastProfile`, ...) live in
//! [`crate::media`] and are re-exported here.
//!
//! Session lifecycle (port of tv-cast's `cast_file`): after `Play` succeeds, a monitor
//! thread polls the transport every second. Playback is reported as ended **only after
//! `PLAYING` has been seen**; if the TV never reaches `PLAYING` within 30 s the session
//! ends with [`CastEvent::NeverStarted`] (format probably unsupported).
//!
//! The session keeps running (server and ffmpeg alive) until playback ends or
//! [`CastSession::stop`] is called, even if every handle is dropped.

use std::fs::File;
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::config::cache_dir;
use crate::dlna::{Dlna, PositionInfo, TransportState};
use crate::error::{Error, Result};
use crate::events::{Event, EventBus};
use crate::server::{
    local_ip_for, mime_for_path, url_unquote, FileServer, ServeSource, StreamFactory, ThroughputSample,
};

pub use crate::media::*;

/// What to cast.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum CastSource {
    /// A local file, served by this PC.
    File(PathBuf),
    /// An http(s) URL the TV fetches itself (always direct).
    Url(String),
}

/// How to cast a local file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum CastMode {
    /// Serve the file as-is (seekable).
    Direct,
    /// Convert on the fly with ffmpeg according to the profile (not seekable).
    TvFriendly(CastProfile),
}

/// A cast request for [`crate::Tv::cast`] / [`start_cast`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CastRequest {
    /// What to play.
    pub source: CastSource,
    /// Direct or TV-friendly (ignored for URLs).
    pub mode: CastMode,
    /// Title shown on the TV (defaults to the file name).
    pub title: Option<String>,
    /// Pre-probed media info (probed automatically for TV-friendly mode if `None`).
    pub media: Option<MediaInfo>,
}

impl CastRequest {
    /// Direct cast of a local file.
    pub fn file(path: impl Into<PathBuf>) -> Self {
        CastRequest {
            source: CastSource::File(path.into()),
            mode: CastMode::Direct,
            title: None,
            media: None,
        }
    }

    /// TV-friendly cast of a local file.
    pub fn tv_friendly(path: impl Into<PathBuf>, profile: CastProfile, media: Option<MediaInfo>) -> Self {
        CastRequest {
            source: CastSource::File(path.into()),
            mode: CastMode::TvFriendly(profile),
            title: None,
            media,
        }
    }

    /// Play a remote URL.
    pub fn url(url: impl Into<String>) -> Self {
        CastRequest {
            source: CastSource::Url(url.into()),
            mode: CastMode::Direct,
            title: None,
            media: None,
        }
    }
}

/// Events emitted on the bus as `Event::Cast(..)` during a session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum CastEvent {
    /// `Play` was accepted; the TV is loading `title`.
    Loading {
        /// Title.
        title: String,
    },
    /// Transport state changed.
    State(TransportState),
    /// Position update (every second while the session runs).
    Position {
        /// Elapsed.
        position: Option<Duration>,
        /// Length.
        duration: Option<Duration>,
    },
    /// One throughput sample (bytes/s pulled by the TV during the last second).
    Throughput(ThroughputSample),
    /// Live guard: playback stalled (position frozen or back to TRANSITIONING).
    Stall {
        /// What happened.
        message: String,
        /// What to try (lighter profile).
        suggestion: String,
    },
    /// Playback finished (PLAYING was seen, then STOPPED / NO_MEDIA_PRESENT).
    Ended,
    /// The TV never reached PLAYING within 30 s; the format is probably unsupported.
    NeverStarted,
    /// The session was stopped by [`CastSession::stop`].
    Stopped,
    /// A non-fatal error while monitoring (e.g. the TV stopped answering).
    Error(String),
}

/// How a session finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CastOutcome {
    /// Playback reached its end (or the TV stopped it after it played).
    Ended,
    /// Never started playing.
    NeverStarted,
    /// Stopped via [`CastSession::stop`].
    Stopped,
    /// The TV became unreachable.
    Lost,
}

/// Snapshot of a session for display.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CastStatus {
    /// Title.
    pub title: String,
    /// URL the TV fetches.
    pub url: String,
    /// True when seeking is possible (direct casts).
    pub seekable: bool,
    /// True for TV-friendly (ffmpeg) casts.
    pub tv_friendly: bool,
    /// Last transport state seen.
    pub state: Option<TransportState>,
    /// Elapsed time.
    pub position: Option<Duration>,
    /// Track length (from the TV, else from ffprobe).
    pub duration: Option<Duration>,
    /// PLAYING has been seen at least once.
    pub started: bool,
    /// Set once the session is over.
    pub outcome: Option<CastOutcome>,
    /// Bytes served so far (0 for URL casts).
    pub total_bytes: u64,
    /// Last 1-second throughput sample, Mbit/s.
    pub last_mbps: Option<f64>,
}

impl CastStatus {
    /// Status of a cast started elsewhere (another process, `tv-cast`, a phone), read
    /// from the TV's renderer: `None` when nothing is loaded or it is stopped. Pure.
    pub fn from_renderer(state: &TransportState, pos: &PositionInfo) -> Option<CastStatus> {
        let seekable = renderer_activity(state, pos)?;
        Some(CastStatus {
            title: uri_title(&pos.track_uri),
            url: pos.track_uri.clone(),
            seekable,
            // A TV-friendly cast from another instance, or another live stream.
            tv_friendly: is_live_stream(pos),
            state: Some(state.clone()),
            position: pos.position,
            duration: pos.duration.filter(|d| !d.is_zero()),
            started: true,
            outcome: None,
            total_bytes: 0,
            last_mbps: None,
        })
    }

    /// Still loading: buffering before the TV knows the length (it reports 0:00:00 for
    /// the first seconds of every cast). Not a live stream yet, whatever `seekable` says.
    pub fn is_loading(&self) -> bool {
        matches!(self.state, None | Some(TransportState::Transitioning)) && self.duration.is_none_or(|d| d.is_zero())
    }

    /// Why this cast can't seek: a live stream, or still loading (the TV knows no
    /// length yet). Pure.
    pub fn no_seek_error(&self) -> Error {
        if !self.tv_friendly && self.is_loading() {
            still_loading()
        } else {
            Error::Playback("this cast is a live stream: seeking is not possible".into())
        }
    }
}

/// The TV refuses `Pause` on a live (TV-friendly) stream outright: say so at once
/// instead of retrying.
pub(crate) fn live_pause() -> Error {
    Error::Playback("this cast is a live stream: pausing is not possible".into())
}

pub(crate) fn still_loading() -> Error {
    Error::Playback("the cast is still loading: try again in a moment".into())
}

/// Has playback begun? PLAYING or PAUSED_PLAYBACK (a cast can be paused during its
/// start-up buffering and never be seen PLAYING), or any other state but buffering with
/// a position past 0 (the TV may still report a stale one while buffering). Pure.
pub fn has_started(state: &TransportState, position: Option<Duration>) -> bool {
    match state {
        TransportState::Playing | TransportState::Paused => true,
        TransportState::Transitioning => false,
        _ => position.is_some_and(|p| !p.is_zero()),
    }
}

/// Step of the rewind / fast-forward keys (and ← → while casting).
pub const SEEK_STEP: Duration = Duration::from_secs(10);

/// How far before the end a seek may land: the TV refuses a target equal to the length
/// (UPnP 711 *Illegal seek target*) but accepts one 1 s before it.
pub const SEEK_END_MARGIN: Duration = Duration::from_secs(1);

/// Keep a seek target inside the media: at most `duration` − [`SEEK_END_MARGIN`] when
/// the length is known (a zero length counts as unknown). Pure.
pub fn clamp_target(to: Duration, duration: Option<Duration>) -> Duration {
    match duration.filter(|d| !d.is_zero()) {
        Some(d) => to.min(d.saturating_sub(SEEK_END_MARGIN)),
        None => to,
    }
}

/// Target of a relative seek: `position` ± `delta`, ≥ 0 and kept inside the media when
/// the length is known ([`clamp_target`]). Pure.
pub fn relative_target(position: Duration, delta: Duration, forward: bool, duration: Option<Duration>) -> Duration {
    let t = if forward {
        position.saturating_add(delta)
    } else {
        position.saturating_sub(delta)
    };
    clamp_target(t, duration)
}

/// The point at `fraction` (clamped to 0.0-1.0) of `duration`, kept inside the media
/// ([`clamp_target`]). Pure.
pub fn fraction_target(duration: Duration, fraction: f64) -> Duration {
    let f = if fraction.is_finite() { fraction.clamp(0.0, 1.0) } else { 0.0 };
    clamp_target(duration.mul_f64(f), Some(duration))
}

/// Is something casting, judging from the renderer alone? `Some(seekable)` while it
/// plays, pauses or buffers. Seekable when the TV reports a length and it is not a live
/// stream ([`is_live_stream`]): with the duration hint of a TV-friendly cast, the TV
/// reports a length and even lists `Seek`, then refuses every seek. Pure.
pub fn renderer_activity(state: &TransportState, pos: &PositionInfo) -> Option<bool> {
    match state {
        TransportState::Playing | TransportState::Paused | TransportState::Transitioning => {
            Some(pos.duration.is_some_and(|d| !d.is_zero()) && !is_live_stream(pos))
        }
        _ => None,
    }
}

/// A live stream the TV can't seek in: a TV-friendly cast from this app (any process;
/// its URL ends in `<name>.tv.mkv` / `.tv.ts`, see [`start_cast`]), or a track whose
/// DIDL says the server allows neither time nor byte seeks (`DLNA.ORG_OP=00`). Pure.
pub fn is_live_stream(pos: &PositionInfo) -> bool {
    let name = uri_title(&pos.track_uri).to_ascii_lowercase();
    let friendly = [StreamContainer::Matroska, StreamContainer::MpegTs]
        .iter()
        .any(|c| name.ends_with(&format!(".tv.{}", c.extension())));
    friendly || pos.track_metadata.contains("DLNA.ORG_OP=00")
}

/// Display title for a media URI: its decoded file name. Pure.
pub fn uri_title(uri: &str) -> String {
    let u = uri.trim();
    let name = u
        .split(['?', '#'])
        .next()
        .unwrap_or(u)
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .map(url_unquote)
        .unwrap_or_default();
    if name.is_empty() {
        "Media cast from another device".into()
    } else {
        name
    }
}

/// How long the target of the last seek stands in for the reported position. Right
/// after a seek the TV still reports the old position for a moment, so quick repeated
/// ±10 s presses must add up instead of each starting from the stale position.
const SEEK_MEMO: Duration = Duration::from_secs(3);

/// The last seek target and when it was sent.
#[derive(Default)]
pub(crate) struct SeekMemo(Mutex<Option<(Duration, Instant)>>);

impl SeekMemo {
    pub(crate) fn record(&self, to: Duration) {
        if let Ok(mut m) = self.0.lock() {
            *m = Some((to, Instant::now()));
        }
    }

    /// Where a relative seek starts: the last target while fresh, else `reported`.
    pub(crate) fn base(&self, reported: Option<Duration>) -> Option<Duration> {
        let recent = self
            .0
            .lock()
            .ok()
            .and_then(|m| *m)
            .filter(|(_, at)| at.elapsed() < SEEK_MEMO)
            .map(|(to, _)| to);
        recent.or(reported)
    }
}

/// Error for a relative seek when the TV gives no position.
pub(crate) fn position_unknown() -> Error {
    Error::Playback("the TV does not report the playback position".into())
}

struct SessionInner {
    dlna: Dlna,
    /// A still image: its position never advances, so the stall guards are off.
    still: bool,
    server: Option<FileServer>,
    bus: EventBus,
    status: Mutex<CastStatus>,
    seek_memo: SeekMemo,
    stop_flag: AtomicBool,
    done: (Mutex<bool>, Condvar),
}

impl SessionInner {
    fn finish(&self, outcome: CastOutcome) {
        if let Ok(mut s) = self.status.lock() {
            if s.outcome.is_some() {
                return;
            }
            s.outcome = Some(outcome);
        }
        self.stop_flag.store(true, Ordering::SeqCst);
        if let Some(srv) = &self.server {
            srv.stop();
        }
        let ev = match outcome {
            CastOutcome::Ended => CastEvent::Ended,
            CastOutcome::NeverStarted => CastEvent::NeverStarted,
            CastOutcome::Stopped => CastEvent::Stopped,
            CastOutcome::Lost => CastEvent::Error("lost contact with the TV".into()),
        };
        self.bus.emit(Event::Cast(ev));
        let (m, c) = &self.done;
        if let Ok(mut d) = m.lock() {
            *d = true;
        }
        c.notify_all();
    }
}

/// Handle to a running cast. Clone freely; `Send + Sync`.
#[derive(Clone)]
pub struct CastSession {
    inner: Arc<SessionInner>,
}

impl std::fmt::Debug for CastSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CastSession").field("status", &self.status()).finish()
    }
}

impl CastSession {
    /// Current status snapshot.
    pub fn status(&self) -> CastStatus {
        self.inner
            .status
            .lock()
            .map(|s| s.clone())
            .unwrap_or_else(|e| e.into_inner().clone())
    }

    /// Per-second throughput samples since the start (empty for URL casts).
    pub fn throughput(&self) -> Vec<ThroughputSample> {
        self.inner.server.as_ref().map(|s| s.samples()).unwrap_or_default()
    }

    /// True once the session is over.
    pub fn is_finished(&self) -> bool {
        self.status().outcome.is_some()
    }

    /// Stop playback on the TV and shut down the server / ffmpeg.
    /// The SOAP `Stop` error (if any) is returned, but the session is finished regardless.
    pub fn stop(&self) -> Result<()> {
        let r = self.inner.dlna.stop();
        self.inner.finish(CastOutcome::Stopped);
        r
    }

    /// Pause (a live TV-friendly stream can't: [`Error::Playback`] without asking the TV).
    pub fn pause(&self) -> Result<()> {
        if self.status().tv_friendly {
            return Err(live_pause());
        }
        self.inner.dlna.pause()
    }

    /// Resume.
    pub fn resume(&self) -> Result<()> {
        self.inner.dlna.play()
    }

    /// Pause when playing (or buffering), else resume. Returns the new state (`Playing`
    /// / `Paused`).
    pub fn play_pause(&self) -> Result<TransportState> {
        let playing = matches!(
            self.status().state,
            Some(TransportState::Playing | TransportState::Transitioning)
        );
        let r = if playing {
            self.pause().map(|_| TransportState::Paused)
        } else {
            self.resume().map(|_| TransportState::Playing)
        };
        r.map_err(crate::dlna::friendly)
    }

    /// Seek (direct casts only; TV-friendly streams return [`Error::Playback`]). The
    /// target is kept inside the media ([`clamp_target`]).
    pub fn seek(&self, to: Duration) -> Result<()> {
        let st = self.status();
        if !st.seekable {
            return Err(st.no_seek_error());
        }
        let to = clamp_target(to, st.duration);
        self.inner.dlna.seek(to).map_err(crate::dlna::friendly)?;
        self.inner.seek_memo.record(to);
        Ok(())
    }

    /// Seek `delta` forward or back from the current position (see [`relative_target`]).
    /// Returns the target. Quick repeated calls add up (the last target counts as the
    /// position for a few seconds).
    pub fn seek_relative(&self, delta: Duration, forward: bool) -> Result<Duration> {
        let st = self.status();
        if !st.seekable {
            return Err(st.no_seek_error());
        }
        let pos = self.inner.seek_memo.base(st.position).ok_or_else(position_unknown)?;
        let to = relative_target(pos, delta, forward, st.duration);
        self.seek(to)?;
        Ok(to)
    }

    /// Seek to `fraction` (0.0-1.0) of the length. Returns the target.
    pub fn seek_fraction(&self, fraction: f64) -> Result<Duration> {
        let d = self
            .status()
            .duration
            .filter(|d| !d.is_zero())
            .ok_or_else(|| Error::Playback("the length of this media is not known yet".into()))?;
        let to = fraction_target(d, fraction);
        self.seek(to)?;
        Ok(to)
    }

    /// Block until the session finishes (or `timeout` elapses → `None`).
    pub fn wait(&self, timeout: Option<Duration>) -> Option<CastOutcome> {
        let (m, c) = &self.inner.done;
        let deadline = timeout.map(|t| Instant::now() + t);
        let mut done = m.lock().ok()?;
        while !*done {
            match deadline {
                Some(d) => {
                    let now = Instant::now();
                    if now >= d {
                        return None;
                    }
                    done = c.wait_timeout(done, d - now).ok()?.0;
                }
                None => done = c.wait(done).ok()?,
            }
        }
        drop(done);
        self.status().outcome
    }
}

/// Path of the log file that receives the stderr of the last TV-friendly ffmpeg.
pub fn ffmpeg_log_path() -> PathBuf {
    cache_dir().join("ffmpeg-last.log")
}

/// Build the stream factory for a TV-friendly cast (spawns ffmpeg per request).
pub fn ffmpeg_factory(args: Vec<String>) -> StreamFactory {
    Arc::new(move || {
        let log = File::create(ffmpeg_log_path())
            .map(Stdio::from)
            .unwrap_or_else(|_| Stdio::null());
        Command::new("ffmpeg")
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(log)
            .spawn()
            .map_err(|e| Error::Media(format!("cannot start ffmpeg: {e}")))
    })
}

fn file_title(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| p.display().to_string())
}

fn didl_duration(d: Duration) -> String {
    let ms = d.as_millis();
    format!(
        "{}:{:02}:{:02}.{:03}",
        ms / 3_600_000,
        (ms / 60_000) % 60,
        (ms / 1000) % 60,
        ms % 1000
    )
}

/// Start a cast (blocking until the TV accepted `Play`, up to ~15 s).
pub fn start_cast(dlna: &Dlna, req: CastRequest, bus: EventBus) -> Result<CastSession> {
    let (server, url, title, mime, seekable, tv_friendly, dur_hint) = match &req.source {
        CastSource::Url(u) => {
            let name = u
                .split(['?', '#'])
                .next()
                .unwrap_or(u)
                .rsplit('/')
                .next()
                .filter(|s| !s.is_empty())
                .unwrap_or(u)
                .to_string();
            let mime = mime_for_path(Path::new(&name));
            (None, u.clone(), req.title.clone().unwrap_or(name), mime.to_string(), true, false, None)
        }
        CastSource::File(path) => {
            let path = std::fs::canonicalize(path)?;
            let ip = local_ip_for(dlna.host())?;
            let title = req.title.clone().unwrap_or_else(|| file_title(&path));
            match &req.mode {
                CastMode::Direct => {
                    let mime = mime_for_path(&path).to_string();
                    let srv = FileServer::start(
                        ServeSource::File(path.clone()),
                        &mime,
                        &file_title(&path),
                        IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                    )?;
                    let url = srv.url(ip);
                    (Some(srv), url, title, mime, true, false, None)
                }
                CastMode::TvFriendly(profile) => {
                    let info = match &req.media {
                        Some(m) => m.clone(),
                        None => probe(&path)?,
                    };
                    let dev = match profile.video {
                        VideoMode::Reduce { .. } => {
                            if !vaapi_available() {
                                return Err(Error::Unsupported(
                                    "video reduction needs VAAPI (hevc_vaapi), which is unavailable".into(),
                                ));
                            }
                            vaapi_device()
                        }
                        VideoMode::Copy => None,
                    };
                    let args = ffmpeg_args(&path, &info, profile, dev.as_deref())?;
                    let mime = profile.container.mime().to_string();
                    let stem = path
                        .file_stem()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_else(|| "stream".into());
                    let name = format!("{stem}.tv.{}", profile.container.extension());
                    let srv = FileServer::start(
                        ServeSource::Command(ffmpeg_factory(args)),
                        &mime,
                        &name,
                        IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                    )?;
                    let url = srv.url(ip);
                    (Some(srv), url, title, mime, false, true, info.duration())
                }
            }
        }
    };

    let dur_str = dur_hint.map(didl_duration);
    dlna.play_uri(&url, &title, &mime, dur_str.as_deref())?;

    let inner = Arc::new(SessionInner {
        dlna: dlna.clone(),
        still: !watches_progress(&mime),
        server,
        bus: bus.clone(),
        status: Mutex::new(CastStatus {
            title: title.clone(),
            url,
            seekable,
            tv_friendly,
            state: None,
            position: None,
            duration: dur_hint,
            started: false,
            outcome: None,
            total_bytes: 0,
            last_mbps: None,
        }),
        seek_memo: SeekMemo::default(),
        stop_flag: AtomicBool::new(false),
        done: (Mutex::new(false), Condvar::new()),
    });
    bus.emit(Event::Cast(CastEvent::Loading { title }));
    let mon = inner.clone();
    std::thread::Builder::new()
        .name("tv-cast-monitor".into())
        .spawn(move || monitor(mon))?;
    Ok(CastSession { inner })
}

/// Does a cast of this MIME type have a playback position that should advance? False
/// for images: the TV shows a photo as PLAYING at 0:00 forever, which is not a stall.
/// Pure.
pub fn watches_progress(mime: &str) -> bool {
    !mime.trim().to_ascii_lowercase().starts_with("image/")
}

/// Seconds without position progress while PLAYING before the live guard fires.
const STALL_SECS: u32 = 4;
/// Minimum seconds between two stall warnings.
const STALL_REPEAT: Duration = Duration::from_secs(30);
/// Time allowed to reach PLAYING (as in tv-cast).
const START_DEADLINE: Duration = Duration::from_secs(30);

fn monitor(inner: Arc<SessionInner>) {
    let t0 = Instant::now();
    let mut started = false;
    let mut last_state: Option<TransportState> = None;
    let mut last_pos: Option<Duration> = None;
    let mut frozen = 0u32;
    let mut failures = 0u32;
    let mut last_stall: Option<Instant> = None;
    let mut trans_polls = 0u32;
    let mut trans_is_seek = false;
    let mut last_sample_t = -1.0f64;
    let suggestion = if inner.status.lock().map(|s| s.tv_friendly).unwrap_or(false) {
        "Try a lighter profile: reduce the video bitrate or drop extra tracks."
    } else {
        "Try TV-friendly mode with only one audio track, or reduce the video bitrate."
    };

    while !inner.stop_flag.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_secs(1));
        if inner.stop_flag.load(Ordering::SeqCst) {
            break;
        }
        // Throughput.
        if let Some(srv) = &inner.server {
            if let Some(s) = srv.samples().last().copied() {
                if s.t_secs > last_sample_t {
                    last_sample_t = s.t_secs;
                    if let Ok(mut st) = inner.status.lock() {
                        st.last_mbps = Some(s.mbps());
                        st.total_bytes = srv.total_bytes();
                    }
                    inner.bus.emit(Event::Cast(CastEvent::Throughput(s)));
                }
            }
        }
        // Transport.
        let ti = match inner.dlna.transport_info() {
            Ok(t) => {
                failures = 0;
                t
            }
            Err(e) => {
                failures += 1;
                if failures == 3 {
                    inner.bus.emit(Event::Cast(CastEvent::Error(e.to_string())));
                }
                if failures >= 15 {
                    inner.finish(CastOutcome::Lost);
                    return;
                }
                continue;
            }
        };
        let pi = inner.dlna.position_info().ok();
        let state = ti.state.clone();
        if last_state.as_ref() != Some(&state) {
            inner.bus.emit(Event::Cast(CastEvent::State(state.clone())));
        }
        started = started || has_started(&state, pi.as_ref().and_then(|p| p.position));
        if let Ok(mut st) = inner.status.lock() {
            st.state = Some(state.clone());
            if let Some(p) = &pi {
                st.position = p.position;
                if p.duration.is_some_and(|d| !d.is_zero()) {
                    st.duration = p.duration;
                }
            }
            st.started = started;
        }
        if let Some(p) = &pi {
            inner.bus.emit(Event::Cast(CastEvent::Position {
                position: p.position,
                duration: p.duration,
            }));
        }

        match state {
            TransportState::Playing => {
                // Live guard: position frozen while PLAYING.
                let pos = pi.as_ref().and_then(|p| p.position);
                if pos.is_some() && pos == last_pos {
                    frozen += 1;
                } else {
                    frozen = 0;
                }
                last_pos = pos;
                if !inner.still && frozen == STALL_SECS && last_stall.is_none_or(|t| t.elapsed() > STALL_REPEAT) {
                    last_stall = Some(Instant::now());
                    inner.bus.emit(Event::Cast(CastEvent::Stall {
                        message: format!("Playback has not advanced for {STALL_SECS} s (buffering)."),
                        suggestion: suggestion.into(),
                    }));
                }
            }
            TransportState::Transitioning if started => {
                // A seek (from any client) also passes through TRANSITIONING; the
                // position then jumps instead of advancing ~1 s. Only a buffering spell
                // without a jump that lasts 2+ polls is a stall.
                let pos = pi.as_ref().and_then(|p| p.position);
                if last_state.as_ref() != Some(&TransportState::Transitioning) {
                    trans_polls = 0;
                    trans_is_seek = match (last_pos, pos) {
                        (Some(a), Some(b)) => {
                            let diff = b.as_secs_f64() - a.as_secs_f64();
                            !(-0.5..=3.0).contains(&diff)
                        }
                        _ => false,
                    };
                }
                trans_polls += 1;
                last_pos = pos;
                if trans_polls == 2
                    && !inner.still
                    && !trans_is_seek
                    && last_stall.is_none_or(|t| t.elapsed() > STALL_REPEAT)
                {
                    last_stall = Some(Instant::now());
                    inner.bus.emit(Event::Cast(CastEvent::Stall {
                        message: "The TV went back to buffering during playback.".into(),
                        suggestion: suggestion.into(),
                    }));
                }
            }
            ref s if s.is_idle() => {
                if started {
                    inner.finish(CastOutcome::Ended);
                    return;
                }
                if t0.elapsed() > START_DEADLINE {
                    inner.finish(CastOutcome::NeverStarted);
                    return;
                }
            }
            _ => {}
        }
        last_state = Some(state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn images_have_no_progress_to_watch() {
        // Live 2025 TV: a PNG stays PLAYING at 0:00; that must not read as buffering.
        assert!(!watches_progress("image/png"));
        assert!(!watches_progress("Image/JPEG"));
        assert!(watches_progress("video/x-matroska"));
        assert!(watches_progress("audio/mpeg"));
    }

    #[test]
    fn didl_duration_format() {
        assert_eq!(didl_duration(Duration::from_millis(3_723_456)), "1:02:03.456");
    }

    #[test]
    fn relative_seek_is_clamped() {
        let s = Duration::from_secs;
        assert_eq!(relative_target(s(100), SEEK_STEP, true, Some(s(600))), s(110));
        assert_eq!(relative_target(s(100), SEEK_STEP, false, Some(s(600))), s(90));
        assert_eq!(relative_target(s(5), SEEK_STEP, false, Some(s(600))), s(0), "never below 0");
        // The TV refuses a target equal to the length (711) but takes one 1 s before.
        assert_eq!(relative_target(s(595), SEEK_STEP, true, Some(s(600))), s(599), "never at the end");
        assert_eq!(clamp_target(s(600), Some(s(600))), s(599));
        assert_eq!(clamp_target(s(900), Some(s(600))), s(599));
        assert_eq!(clamp_target(s(299), Some(s(300))), s(299));
        assert_eq!(clamp_target(s(900), None), s(900));
        assert_eq!(clamp_target(s(5), Some(Duration::from_millis(500))), s(0), "tiny media");
        // Unknown (or zero, as some TVs report for streams) length: only the floor applies.
        assert_eq!(relative_target(s(595), SEEK_STEP, true, None), s(605));
        assert_eq!(relative_target(s(595), SEEK_STEP, true, Some(s(0))), s(605));
        assert_eq!(relative_target(s(3), s(90), false, None), s(0));
    }

    #[test]
    fn fraction_targets() {
        let d = Duration::from_secs(600);
        assert_eq!(fraction_target(d, 0.5), Duration::from_secs(300));
        assert_eq!(fraction_target(d, 0.0), Duration::ZERO);
        assert_eq!(fraction_target(d, 1.0), Duration::from_secs(599), "last bar column");
        assert_eq!(fraction_target(d, 1.7), Duration::from_secs(599));
        assert_eq!(fraction_target(d, -1.0), Duration::ZERO);
        assert_eq!(fraction_target(d, f64::NAN), Duration::ZERO);
    }

    #[test]
    fn seek_memo_stands_in_for_a_stale_position() {
        let m = SeekMemo::default();
        assert_eq!(m.base(Some(Duration::from_secs(7))), Some(Duration::from_secs(7)));
        assert_eq!(m.base(None), None);
        m.record(Duration::from_secs(40));
        assert_eq!(m.base(Some(Duration::from_secs(30))), Some(Duration::from_secs(40)));
        assert_eq!(m.base(None), Some(Duration::from_secs(40)));
    }

    #[test]
    fn renderer_status_of_a_cast_started_elsewhere() {
        let pos = PositionInfo {
            position: Some(Duration::from_secs(12)),
            duration: Some(Duration::from_secs(3600)),
            track_uri: "http://192.0.2.10:8000/Le%20film.mkv?x=1".into(),
            track_metadata: String::new(),
        };
        let st = CastStatus::from_renderer(&TransportState::Paused, &pos).unwrap();
        assert_eq!(st.title, "Le film.mkv");
        assert!(st.seekable);
        assert_eq!(st.position, Some(Duration::from_secs(12)));
        assert!(CastStatus::from_renderer(&TransportState::Stopped, &pos).is_none());
        assert!(CastStatus::from_renderer(&TransportState::NoMedia, &PositionInfo::default()).is_none());
        // A live stream reports no length (or 0:00:00): not seekable.
        let live = PositionInfo { duration: Some(Duration::ZERO), ..pos.clone() };
        assert_eq!(renderer_activity(&TransportState::Playing, &live), Some(false));
        assert_eq!(CastStatus::from_renderer(&TransportState::Playing, &live).unwrap().duration, None);
        // At the start of every cast the TV buffers with a 0:00:00 length: loading, not live.
        let start = CastStatus::from_renderer(&TransportState::Transitioning, &live).unwrap();
        assert!(start.is_loading());
        assert!(!st.is_loading());
        assert_eq!(uri_title(""), "Media cast from another device");
    }

    /// A session that never touches the network (TEST-NET address, no server, no monitor).
    fn offline_session(tv_friendly: bool, state: TransportState) -> CastSession {
        let status = CastStatus {
            title: "x".into(),
            url: "http://192.0.2.10:1/x.tv.mkv".into(),
            seekable: !tv_friendly,
            tv_friendly,
            state: Some(state),
            position: Some(Duration::from_secs(5)),
            duration: Some(Duration::from_secs(300)),
            started: true,
            outcome: None,
            total_bytes: 0,
            last_mbps: None,
        };
        CastSession {
            inner: Arc::new(SessionInner {
                dlna: Dlna::new("192.0.2.1"),
                still: false,
                server: None,
                bus: EventBus::new(),
                status: Mutex::new(status),
                seek_memo: SeekMemo::default(),
                stop_flag: AtomicBool::new(true),
                done: (Mutex::new(false), Condvar::new()),
            }),
        }
    }

    #[test]
    fn live_streams_refuse_pause_and_seek_at_once() {
        let s = offline_session(true, TransportState::Playing);
        let t0 = Instant::now();
        let msg = |r: Result<()>| r.unwrap_err().to_string();
        assert_eq!(msg(s.pause()), "this cast is a live stream: pausing is not possible");
        assert_eq!(
            s.play_pause().unwrap_err().to_string(),
            "this cast is a live stream: pausing is not possible"
        );
        assert_eq!(msg(s.seek(Duration::from_secs(9))), "this cast is a live stream: seeking is not possible");
        assert!(t0.elapsed() < Duration::from_millis(100), "no retries, no network");
    }

    #[test]
    fn playback_started_even_if_never_seen_playing() {
        let s = Duration::from_secs;
        assert!(has_started(&TransportState::Playing, None));
        // Paused during the start-up buffering: the TV goes straight to PAUSED at 0:00.
        assert!(has_started(&TransportState::Paused, Some(Duration::ZERO)));
        assert!(has_started(&TransportState::Other("X".into()), Some(s(3))));
        assert!(!has_started(&TransportState::Transitioning, Some(s(3))), "maybe a stale position");
        assert!(!has_started(&TransportState::Stopped, Some(Duration::ZERO)));
        assert!(!has_started(&TransportState::NoMedia, None));
    }

    #[test]
    fn no_seek_says_loading_or_live() {
        let mut st = offline_session(false, TransportState::Transitioning).status();
        st.seekable = false;
        st.duration = None;
        assert_eq!(st.no_seek_error().to_string(), "the cast is still loading: try again in a moment");
        st.state = Some(TransportState::Playing);
        assert_eq!(st.no_seek_error().to_string(), "this cast is a live stream: seeking is not possible");
        // A TV-friendly stream is live even while it loads.
        let mut f = offline_session(true, TransportState::Transitioning).status();
        f.duration = None;
        assert!(f.no_seek_error().to_string().contains("live stream"));
        // External: a direct cast at start-up (0:00:00 length) vs a stream recognised by its URL.
        let pos = PositionInfo { duration: Some(Duration::ZERO), track_uri: "http://a/f.mp4".into(), ..Default::default() };
        let loading = CastStatus::from_renderer(&TransportState::Transitioning, &pos).unwrap();
        assert!(loading.no_seek_error().to_string().contains("still loading"));
        let live = PositionInfo { track_uri: "http://a/f.tv.mkv".into(), ..pos };
        let live = CastStatus::from_renderer(&TransportState::Transitioning, &live).unwrap();
        assert!(live.tv_friendly);
        assert!(live.no_seek_error().to_string().contains("live stream"));
    }

    #[test]
    fn tv_friendly_streams_are_never_seekable() {
        // A TV-friendly cast from any process: the duration hint makes the TV report a
        // length (and list Seek), but every seek is refused.
        let friendly = PositionInfo {
            position: Some(Duration::from_secs(3)),
            duration: Some(Duration::from_secs(300)),
            track_uri: "http://192.0.2.10:41234/Le%20film.tv.mkv".into(),
            track_metadata: String::new(),
        };
        assert!(is_live_stream(&friendly));
        assert_eq!(renderer_activity(&TransportState::Playing, &friendly), Some(false));
        let ts = PositionInfo { track_uri: "http://192.0.2.10:1/x.TV.ts".into(), ..friendly.clone() };
        assert!(is_live_stream(&ts));
        let direct = PositionInfo { track_uri: "http://192.0.2.10:1/tv.mkv".into(), ..friendly.clone() };
        assert!(!is_live_stream(&direct), "a file named tv.mkv is not a stream");
        assert_eq!(renderer_activity(&TransportState::Paused, &direct), Some(true));
        let op00 = PositionInfo {
            track_metadata: "<res protocolInfo=\"http-get:*:video/mp4:DLNA.ORG_OP=00;DLNA.ORG_CI=0\">".into(),
            ..direct
        };
        assert!(is_live_stream(&op00));
    }

    #[test]
    fn request_builders() {
        let r = CastRequest::url("http://example.invalid/a/b.mp4?x");
        assert_eq!(r.source, CastSource::Url("http://example.invalid/a/b.mp4?x".into()));
        assert_eq!(CastRequest::file("/x").mode, CastMode::Direct);
    }
}
