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
use crate::dlna::{Dlna, TransportState};
use crate::error::{Error, Result};
use crate::events::{Event, EventBus};
use crate::server::{local_ip_for, mime_for_path, FileServer, ServeSource, StreamFactory, ThroughputSample};

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

struct SessionInner {
    dlna: Dlna,
    /// A still image: its position never advances, so the stall guards are off.
    still: bool,
    server: Option<FileServer>,
    bus: EventBus,
    status: Mutex<CastStatus>,
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

    /// Pause.
    pub fn pause(&self) -> Result<()> {
        self.inner.dlna.pause()
    }

    /// Resume.
    pub fn resume(&self) -> Result<()> {
        self.inner.dlna.play()
    }

    /// Seek (direct casts only; TV-friendly streams return [`Error::Unsupported`]).
    pub fn seek(&self, to: Duration) -> Result<()> {
        if !self.status().seekable {
            return Err(Error::Unsupported("seeking is not possible in TV-friendly mode".into()));
        }
        self.inner.dlna.seek(to)
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
        if let Ok(mut st) = inner.status.lock() {
            st.state = Some(state.clone());
            if let Some(p) = &pi {
                st.position = p.position;
                if p.duration.is_some_and(|d| !d.is_zero()) {
                    st.duration = p.duration;
                }
            }
            st.started = started || state == TransportState::Playing;
        }
        if let Some(p) = &pi {
            inner.bus.emit(Event::Cast(CastEvent::Position {
                position: p.position,
                duration: p.duration,
            }));
        }

        match state {
            TransportState::Playing => {
                started = true;
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
    fn request_builders() {
        let r = CastRequest::url("http://example.invalid/a/b.mp4?x");
        assert_eq!(r.source, CastSource::Url("http://example.invalid/a/b.mp4?x".into()));
        assert_eq!(CastRequest::file("/x").mode, CastMode::Direct);
    }
}
