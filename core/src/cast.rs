//! Casting: direct (file server + DLNA) or TV-friendly (ffmpeg → chunked stream), plus
//! the [`CastSession`] handle used to monitor and control it. Both can seek and pause.
//!
//! TV-friendly seeking and pausing, in order of preference:
//! 1. **The TV does it** (DLNA time seek): the stream is advertised with `DLNA.ORG_OP=10`
//!    and the HTTP-stalling flag, so after a `Seek` the TV may fetch the stream again
//!    with `TimeSeekRange.dlna.org` and the server restarts ffmpeg there (see
//!    [`crate::server`]); `Pause` may simply stall the connection.
//! 2. **The app does it**: if the TV refuses the `Seek`, or accepts it without fetching
//!    the new point, the session restarts the stream at the target (a `?t=<secs>` URL,
//!    `SetAVTransportURI` + `Play`) and from then on reports offset + `RelTime`. If the
//!    TV refuses `Pause`, the session stops the TV and resumes with such a restart.
//!
//! Which path was taken is reported as [`CastEvent::Note`].
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
use crate::dlna::{Dlna, PositionInfo, TransportState, DLNA_FLAGS_STREAM};
use crate::error::{Error, Result};
use crate::events::{Event, EventBus};
use crate::server::{
    format_npt, local_ip_for, mime_for_path, start_offset, url_unquote, FileServer, ServeSource, StreamFactory,
    StreamStart, ThroughputSample,
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
    /// Convert on the fly with ffmpeg according to the profile (seekable by time).
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
    /// What the session just did, for the curious and for testing (e.g. "Seek to 1:00:
    /// the TV fetched the new point itself (TimeSeekRange)").
    Note(String),
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
    /// True when seeking is possible (every cast from a file; not a live URL).
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
    /// A TV-friendly stream restarted by its app (`?t=<secs>` in the URL) is shown on
    /// the media's own timeline: offset + what the TV reports.
    pub fn from_renderer(state: &TransportState, pos: &PositionInfo) -> Option<CastStatus> {
        let seekable = renderer_activity(state, pos)?;
        let offset = start_offset(&pos.track_uri);
        Some(CastStatus {
            title: uri_title(&pos.track_uri),
            url: pos.track_uri.clone(),
            seekable,
            tv_friendly: is_tv_friendly(pos),
            state: Some(state.clone()),
            position: true_position(offset, pos.position),
            duration: pos.duration.filter(|d| !d.is_zero()).map(|d| d + offset),
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

    /// Why this cast can't seek: still loading (the TV knows no length yet), or a live
    /// stream (a radio-like URL). Pure.
    pub fn no_seek_error(&self) -> Error {
        if self.is_loading() {
            still_loading()
        } else {
            Error::Playback("this cast is a live stream: seeking is not possible".into())
        }
    }
}

/// A TV-friendly cast started by another process refused a seek or a pause: only the
/// app streaming it can restart the stream (`what`: "seek", "pause").
pub(crate) fn owner_only(what: &str) -> Error {
    Error::Playback(format!("the TV can't {what} this TV-friendly cast by itself: {what} it from the app that started it"))
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

/// The same for a TV-friendly (streamed) cast: the TV can't start a stream tail of a
/// few seconds (see [`crate::server::MIN_STREAM_TAIL`]: a seek to 4:59 of 5:00 wedged it).
pub const STREAM_END_MARGIN: Duration = crate::server::MIN_STREAM_TAIL;

/// The end margin of a cast: [`STREAM_END_MARGIN`] for a TV-friendly stream, else
/// [`SEEK_END_MARGIN`]. Pure.
pub fn end_margin(tv_friendly: bool) -> Duration {
    if tv_friendly {
        STREAM_END_MARGIN
    } else {
        SEEK_END_MARGIN
    }
}

/// Keep a seek target inside the media: at most `duration` − [`SEEK_END_MARGIN`] when
/// the length is known (a zero length counts as unknown). Pure.
pub fn clamp_target(to: Duration, duration: Option<Duration>) -> Duration {
    clamp_end(to, duration, false)
}

/// [`clamp_target`] with the margin of the cast's kind ([`end_margin`]). Pure.
pub fn clamp_end(to: Duration, duration: Option<Duration>, tv_friendly: bool) -> Duration {
    match duration.filter(|d| !d.is_zero()) {
        Some(d) => to.min(d.saturating_sub(end_margin(tv_friendly))),
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
/// stream ([`is_live_stream`]). Pure.
pub fn renderer_activity(state: &TransportState, pos: &PositionInfo) -> Option<bool> {
    match state {
        TransportState::Playing | TransportState::Paused | TransportState::Transitioning => {
            Some(pos.duration.is_some_and(|d| !d.is_zero()) && !is_live_stream(pos))
        }
        _ => None,
    }
}

/// A TV-friendly cast from this app (any process): its URL ends in `<name>.tv.mkv` /
/// `.tv.ts` (see [`start_cast`]), possibly with a `?t=` restart offset. Pure.
pub fn is_tv_friendly(pos: &PositionInfo) -> bool {
    let name = uri_title(&pos.track_uri).to_ascii_lowercase();
    [StreamContainer::Matroska, StreamContainer::MpegTs]
        .iter()
        .any(|c| name.ends_with(&format!(".tv.{}", c.extension())))
}

/// A live stream nobody can seek in: its DIDL says the server allows neither time nor
/// byte seeks (`DLNA.ORG_OP=00`). TV-friendly casts are not (they time-seek). Pure.
pub fn is_live_stream(pos: &PositionInfo) -> bool {
    pos.track_metadata.contains("DLNA.ORG_OP=00")
}

/// Position on the media's own timeline: the stream's start offset (after an app
/// restart) + what the TV reports. Pure.
pub fn true_position(offset: Duration, reported: Option<Duration>) -> Option<Duration> {
    reported.map(|r| offset + r)
}

/// URL of a TV-friendly stream restarted `at` into the media (the server starts ffmpeg
/// there; see [`crate::server::start_offset`]). Pure.
pub fn restart_url(url: &str, at: Duration) -> String {
    if at.is_zero() {
        url.to_string()
    } else {
        format!("{url}?t={:.3}", at.as_secs_f64())
    }
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

    /// The last target and when it was sent.
    pub(crate) fn last(&self) -> Option<(Duration, Instant)> {
        self.0.lock().ok().and_then(|m| *m)
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

/// A seek counts as recent this long after it was sent (or fetched).
const SEEK_RECENT: Duration = Duration::from_secs(5);
/// How long the TV may stay TRANSITIONING without progress after a seek before the
/// watchdog restarts a TV-friendly stream ([`stuck_after_seek`]).
pub const SEEK_WATCHDOG: Duration = Duration::from_secs(5);

/// Is a TRANSITIONING spell a seek rather than buffering? Yes if a seek was sent (or
/// the TV fetched one) `seek_ago` recently, whatever its distance (a seek of 2 s
/// forward looks like normal progress); else if the position jumped (outside −0.5 …
/// +3 s), e.g. a seek from a client that leaves no trace here. Pure.
pub fn is_seek_transition(last_pos: Option<Duration>, pos: Option<Duration>, seek_ago: Option<Duration>) -> bool {
    if seek_ago.is_some_and(|a| a <= SEEK_RECENT) {
        return true;
    }
    match (last_pos, pos) {
        (Some(a), Some(b)) => !(-0.5..=3.0).contains(&(b.as_secs_f64() - a.as_secs_f64())),
        _ => false,
    }
}

/// Is a TV-friendly cast stuck after a seek? TRANSITIONING for `transitioning_for`
/// (≥ [`SEEK_WATCHDOG`]) without the position `advancing`, with a seek sent or fetched
/// `seek_ago` (shortly before or during that spell). Pure.
pub fn stuck_after_seek(transitioning_for: Duration, seek_ago: Option<Duration>, advancing: bool) -> bool {
    !advancing
        && transitioning_for >= SEEK_WATCHDOG
        && seek_ago.is_some_and(|a| a <= transitioning_for + SEEK_RECENT)
}

/// Has a TV-friendly stream played to its end? ffmpeg produced everything
/// (`producer_done`), and the position sits at the end (within 2 s of `length`, the
/// TV's own length after a restart) and has not moved for 2+ polls: the TV then stays
/// PLAYING at the last frame instead of going STOPPED. Pure.
pub fn stream_end_reached(position: Option<Duration>, length: Option<Duration>, frozen_polls: u32, producer_done: bool) -> bool {
    let at_end = match (position, length.filter(|l| !l.is_zero())) {
        (Some(p), Some(l)) => p + Duration::from_secs(2) >= l,
        _ => false,
    };
    producer_done && at_end && frozen_polls >= 2
}

/// Does the TV's `TrackURI` (`reported`) name the URI we `sent` (maybe with escapes
/// undone)? Pure.
pub fn took_uri(reported: &str, sent: &str) -> bool {
    let r = reported.trim();
    !r.is_empty() && (r == sent || url_unquote(r) == url_unquote(sent))
}

/// What the watchdog does for a TV-friendly cast stuck after a seek to `target`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchdogAction {
    /// Restart the stream here (the target, kept [`STREAM_END_MARGIN`] before the end).
    Restart(Duration),
    /// The target is within [`STREAM_END_MARGIN`] of the end: end the cast as finished
    /// rather than restart a tail the TV can't play.
    End,
}

/// The watchdog's decision. Pure.
pub fn watchdog_action(target: Duration, length: Option<Duration>) -> WatchdogAction {
    match length.filter(|l| !l.is_zero()) {
        Some(l) if target + STREAM_END_MARGIN > l => WatchdogAction::End,
        _ => WatchdogAction::Restart(target),
    }
}

/// After a seek the TV plays (it re-fetches the stream); a cast that was paused before
/// must be paused again, unless the app holds it paused itself (the seek only moved its
/// resume point). Pure.
fn repause_needed(before: Option<&TransportState>, hold: Hold) -> bool {
    before == Some(&TransportState::Paused) && !matches!(hold, Hold::Paused(_))
}

/// Wait (up to 10 s) for the TV to settle after a seek, then pause it if it plays.
pub(crate) fn repause_after_seek(dlna: &Dlna) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        match dlna.transport_info()?.state {
            TransportState::Playing => return dlna.pause(),
            TransportState::Paused => return Ok(()),
            s if s.is_idle() => return Ok(()),
            _ => std::thread::sleep(Duration::from_millis(300)),
        }
    }
    Ok(())
}

/// What a TV-friendly cast needs to restart its stream at another point.
struct Restart {
    /// Stream URL, without the `?t=` query.
    url: String,
    title: String,
    mime: String,
    /// Length of the whole media, if known.
    length: Option<Duration>,
}

/// While the app drives a TV-friendly cast itself, the TV's STOPPED is not the end.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Hold {
    /// The TV plays what it is given.
    None,
    /// Restarting the stream; until the TV plays again (or the deadline passes).
    Restarting(Instant),
    /// Paused by the app (the TV refused `Pause`): the TV is stopped; resume restarts
    /// at this position.
    Paused(Duration),
}

/// How long a TV that accepted `Seek` gets to fetch the new point (`TimeSeekRange`)
/// before the app restarts the stream itself.
const NATIVE_SEEK_WAIT: Duration = Duration::from_millis(2500);

struct SessionInner {
    dlna: Dlna,
    /// A still image: its position never advances, so the stall guards are off.
    still: bool,
    server: Option<FileServer>,
    bus: EventBus,
    status: Mutex<CastStatus>,
    seek_memo: SeekMemo,
    /// TV-friendly casts: how to restart the stream.
    restart: Option<Restart>,
    /// Where the TV's timeline starts in the media (the last app restart).
    base: Mutex<Duration>,
    hold: Mutex<Hold>,
    /// One seek / pause / resume at a time.
    control: Mutex<()>,
    stop_flag: AtomicBool,
    done: (Mutex<bool>, Condvar),
}

impl SessionInner {
    fn base(&self) -> Duration {
        self.base.lock().map(|b| *b).unwrap_or_default()
    }

    fn hold(&self) -> Hold {
        self.hold.lock().map(|h| *h).unwrap_or(Hold::None)
    }

    fn set_hold(&self, h: Hold) {
        if let Ok(mut g) = self.hold.lock() {
            *g = h;
        }
    }

    fn note(&self, msg: String) {
        self.bus.emit(Event::Cast(CastEvent::Note(msg)));
    }

    /// Restart the TV-friendly stream `at` into the media (call with `control` held).
    fn restart_at(&self, at: Duration, why: &str) -> Result<()> {
        let Some(r) = &self.restart else {
            return Err(Error::Unsupported("only a TV-friendly stream can be restarted".into()));
        };
        self.set_hold(Hold::Restarting(Instant::now() + START_DEADLINE));
        if let Ok(mut b) = self.base.lock() {
            *b = at;
        }
        let url = restart_url(&r.url, at);
        let tv_length = r.length.map(|l| didl_duration(l.saturating_sub(at)));
        let mut res = self
            .dlna
            .play_uri_with(&url, &r.title, &r.mime, tv_length.as_deref(), DLNA_FLAGS_STREAM);
        // A TV busy buffering may answer too late (seen: the reply timed out, yet the
        // TV had taken the new URI): check what it plays before calling it a failure.
        if res.is_err() && self.tv_took_uri(&url) {
            res = Ok(());
        }
        if let Ok(mut st) = self.status.lock() {
            st.position = Some(at);
            st.state = Some(TransportState::Transitioning);
            st.url = url;
        }
        match &res {
            Ok(()) => self.note(format!("{why}: restarted the stream at {}", format_npt(at))),
            Err(e) => {
                self.set_hold(Hold::None);
                self.note(format!("{why}: restarting the stream at {} failed: {e}", format_npt(at)));
            }
        }
        res.map_err(crate::dlna::friendly)
    }

    /// Does the TV play `url` (asked for up to ~8 s, while it may not answer)?
    fn tv_took_uri(&self, url: &str) -> bool {
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            if let Ok(p) = self.dlna.position_info() {
                return took_uri(&p.track_uri, url);
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    }

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

    /// Pause. A TV-friendly cast whose TV refuses `Pause` is paused by the app instead:
    /// the TV is stopped and [`CastSession::resume`] restarts the stream where it was.
    pub fn pause(&self) -> Result<()> {
        if self.inner.restart.is_none() {
            return self.inner.dlna.pause();
        }
        let _g = self.inner.control.lock();
        if matches!(self.inner.hold(), Hold::Paused(_)) {
            return Ok(());
        }
        match self.inner.dlna.pause() {
            Ok(()) => {
                self.inner.note("Pause: the TV paused the stream itself (HTTP stalling)".into());
                Ok(())
            }
            Err(e @ Error::Soap { .. }) => {
                let at = self.status().position.unwrap_or_else(|| self.inner.base());
                self.inner.set_hold(Hold::Paused(at));
                let _ = self.inner.dlna.stop();
                if let Ok(mut st) = self.inner.status.lock() {
                    st.state = Some(TransportState::Paused);
                    st.position = Some(at);
                }
                self.inner.bus.emit(Event::Cast(CastEvent::State(TransportState::Paused)));
                self.inner.note(format!(
                    "Pause: the TV refused ({e}); stopped the stream at {}, resume restarts it there",
                    format_npt(at)
                ));
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    /// Resume (after an app-side pause: restart the stream where it was paused).
    pub fn resume(&self) -> Result<()> {
        if self.inner.restart.is_some() {
            let _g = self.inner.control.lock();
            if let Hold::Paused(at) = self.inner.hold() {
                return self.inner.restart_at(at, "Resume after the app-side pause");
            }
        }
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
        let to = clamp_end(to, st.duration, st.tv_friendly);
        let repause = repause_needed(st.state.as_ref(), self.inner.hold());
        if self.inner.restart.is_some() {
            self.friendly_seek(to)?;
        } else {
            self.inner.dlna.seek(to).map_err(crate::dlna::friendly)?;
        }
        self.inner.seek_memo.record(to);
        if repause {
            // The TV plays after a seek; keep the cast paused (in the background: the TV
            // first fetches the new point).
            let inner = self.inner.clone();
            let _ = std::thread::Builder::new().name("tv-cast-repause".into()).spawn(move || {
                if let Err(e) = repause_after_seek(&inner.dlna) {
                    inner.note(format!("Seek while paused: pausing again failed: {e}"));
                }
            });
        }
        Ok(())
    }

    /// Seek a TV-friendly cast: the TV's own time seek if it fetches the new point,
    /// else an app restart of the stream there (see the module docs).
    fn friendly_seek(&self, to: Duration) -> Result<()> {
        let _g = self.inner.control.lock();
        if let Hold::Paused(_) = self.inner.hold() {
            // Paused by the app: just move the point resume restarts from.
            self.inner.set_hold(Hold::Paused(to));
            if let Ok(mut st) = self.inner.status.lock() {
                st.position = Some(to);
            }
            self.inner.note(format!("Seek while paused: resume restarts at {}", format_npt(to)));
            return Ok(());
        }
        let base = self.inner.base();
        let Some(srv) = &self.inner.server else {
            return Err(Error::Media("the stream server is gone".into()));
        };
        // Before the start of the TV's timeline (after a restart): only a restart can.
        let why = match to.checked_sub(base) {
            None => "Seek before the restart point".to_string(),
            Some(tv_to) => {
                let before = srv.time_seek_count();
                match self.inner.dlna.seek(tv_to) {
                    Ok(()) => {
                        let deadline = Instant::now() + NATIVE_SEEK_WAIT;
                        while srv.time_seek_count() == before && Instant::now() < deadline {
                            std::thread::sleep(Duration::from_millis(100));
                        }
                        if srv.time_seek_count() > before {
                            self.inner.note(format!(
                                "Seek to {}: the TV fetched the new point itself (TimeSeekRange)",
                                format_npt(to)
                            ));
                            return Ok(());
                        }
                        format!("Seek to {}: the TV accepted Seek but did not fetch the new point", format_npt(to))
                    }
                    Err(e @ Error::Soap { .. }) => format!("Seek to {}: the TV refused ({e})", format_npt(to)),
                    Err(e) => return Err(crate::dlna::friendly(e)),
                }
            }
        };
        self.inner.restart_at(to, &why)
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
        let to = clamp_end(relative_target(pos, delta, forward, st.duration), st.duration, st.tv_friendly);
        self.seek(to)?;
        Ok(to)
    }

    /// Seek to `fraction` (0.0-1.0) of the length. Returns the target.
    pub fn seek_fraction(&self, fraction: f64) -> Result<Duration> {
        let st = self.status();
        let d = st
            .duration
            .filter(|d| !d.is_zero())
            .ok_or_else(|| Error::Playback("the length of this media is not known yet".into()))?;
        let to = clamp_end(fraction_target(d, fraction), Some(d), st.tv_friendly);
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

/// Build the stream factory for a TV-friendly cast (spawns ffmpeg per request, starting
/// where the request asks: see [`ffmpeg_args_from`]).
pub fn ffmpeg_factory(args: Vec<String>) -> StreamFactory {
    Arc::new(move |start: StreamStart| {
        let log = File::create(ffmpeg_log_path())
            .map(Stdio::from)
            .unwrap_or_else(|_| Stdio::null());
        Command::new("ffmpeg")
            .args(ffmpeg_args_from(&args, start.at, start.ts_offset))
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
                        ServeSource::Command { factory: ffmpeg_factory(args), length: info.duration() },
                        &mime,
                        &name,
                        IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                    )?;
                    let url = srv.url(ip);
                    (Some(srv), url, title, mime, true, true, info.duration())
                }
            }
        }
    };

    let dur_str = dur_hint.map(didl_duration);
    let features = if tv_friendly { DLNA_FLAGS_STREAM } else { crate::dlna::DLNA_FLAGS };
    dlna.play_uri_with(&url, &title, &mime, dur_str.as_deref(), features)?;
    let restart = tv_friendly.then(|| Restart {
        url: url.clone(),
        title: title.clone(),
        mime: mime.clone(),
        length: dur_hint,
    });

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
        restart,
        base: Mutex::new(Duration::ZERO),
        hold: Mutex::new(Hold::None),
        control: Mutex::new(()),
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

/// The latest seek: sent by this session, or fetched by the TV with `TimeSeekRange`
/// (whoever sent it the `Seek`, e.g. another process). Target on the media's timeline,
/// and when.
fn latest_seek(inner: &SessionInner) -> Option<(Duration, Instant)> {
    let fetched = inner
        .server
        .as_ref()
        .and_then(|s| s.last_time_seek())
        .map(|(t, at)| (at, t));
    match (inner.seek_memo.last(), fetched) {
        (Some(a), Some(b)) => Some(if b.1 > a.1 { b } else { a }),
        (a, b) => a.or(b),
    }
}

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
    // The current TRANSITIONING spell: since when, from which position.
    let mut trans_since = Instant::now();
    let mut trans_from: Option<Duration> = None;
    // The seek the watchdog last recovered from (once per seek).
    let mut watchdog_for: Option<Instant> = None;
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
        // Paused by the app: the TV is stopped on purpose; nothing to read.
        if let Hold::Paused(at) = inner.hold() {
            if let Ok(mut st) = inner.status.lock() {
                st.state = Some(TransportState::Paused);
                st.position = Some(at);
            }
            last_state = Some(TransportState::Paused);
            frozen = 0;
            continue;
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
        let mut state = ti.state.clone();
        // Restarting the stream: the TV passes through STOPPED; that is not the end.
        let restarting = match inner.hold() {
            Hold::Restarting(deadline) => {
                if matches!(state, TransportState::Playing | TransportState::Paused) || Instant::now() > deadline {
                    inner.set_hold(Hold::None);
                    false
                } else {
                    true
                }
            }
            _ => false,
        };
        if restarting && state.is_idle() {
            state = TransportState::Transitioning;
        }
        if last_state.as_ref() != Some(&state) {
            inner.bus.emit(Event::Cast(CastEvent::State(state.clone())));
        }
        started = started || has_started(&state, pi.as_ref().and_then(|p| p.position));
        // On the media's own timeline (after an app restart the TV counts from there).
        let base = inner.base();
        let position = pi.as_ref().and_then(|p| true_position(base, p.position));
        let tv_length = pi.as_ref().and_then(|p| p.duration).filter(|d| !d.is_zero());
        let length = inner.restart.as_ref().and_then(|r| r.length).or(tv_length.map(|d| d + base));
        // While restarting, the TV may still report the old stream's position: keep the
        // target shown by `restart_at`.
        let fresh = pi.is_some() && !restarting;
        if let Ok(mut st) = inner.status.lock() {
            st.state = Some(state.clone());
            if fresh {
                st.position = position;
                if length.is_some() {
                    st.duration = length;
                }
            }
            st.started = started;
        }
        if fresh {
            inner.bus.emit(Event::Cast(CastEvent::Position { position, duration: length }));
        }
        if restarting {
            frozen = 0;
            last_pos = None;
            last_state = Some(state);
            continue;
        }
        let last_seek = latest_seek(&inner);
        let seek_ago = last_seek.map(|(_, at)| at.elapsed());

        match state {
            TransportState::Playing => {
                // Live guard: position frozen while PLAYING.
                let pos = position;
                if pos.is_some() && pos == last_pos {
                    frozen += 1;
                } else {
                    frozen = 0;
                }
                last_pos = pos;
                // A TV-friendly stream at its end: the TV stays PLAYING on the last
                // frame (seen after a restart) instead of going STOPPED.
                let done = inner.restart.is_some() && inner.server.as_ref().is_some_and(|s| s.stream_done());
                if stream_end_reached(pos, length, frozen, done) {
                    inner.finish(CastOutcome::Ended);
                    return;
                }
                if !inner.still && frozen == STALL_SECS && last_stall.is_none_or(|t| t.elapsed() > STALL_REPEAT) {
                    last_stall = Some(Instant::now());
                    inner.bus.emit(Event::Cast(CastEvent::Stall {
                        message: format!("Playback has not advanced for {STALL_SECS} s (buffering)."),
                        suggestion: suggestion.into(),
                    }));
                }
            }
            TransportState::Transitioning if started => {
                // A seek (from any client) also passes through TRANSITIONING. Only a
                // buffering spell that is not a seek and lasts 2+ polls is a stall.
                let pos = position;
                if last_state.as_ref() != Some(&TransportState::Transitioning) {
                    trans_polls = 0;
                    trans_is_seek = is_seek_transition(last_pos, pos, seek_ago);
                    trans_since = Instant::now();
                    trans_from = pos;
                }
                trans_polls += 1;
                last_pos = pos;
                // Watchdog: a TV-friendly stream the TV fetched after a seek but never
                // starts playing (seen with seeks from another process): restart it there.
                let since = trans_since.elapsed();
                if let (Some(_), Some((fetched, at))) = (&inner.restart, last_seek) {
                    if watchdog_for != Some(at) && stuck_after_seek(since, seek_ago, pos != trans_from) {
                        watchdog_for = Some(at);
                        // The user's target: this session's own seek, else (a seek from
                        // another process) where the TV waits, which is that target; the
                        // TV's fetch is rounded down (4:59 became npt=295).
                        let own = inner.seek_memo.last().is_some_and(|(_, t)| t == at);
                        let target = if own { fetched } else { pos.unwrap_or(fetched) };
                        let mut why = format!(
                            "Watchdog: the TV stayed buffering for {} s after the seek to {}",
                            since.as_secs(),
                            format_npt(target)
                        );
                        if fetched.as_secs() != target.as_secs() {
                            why.push_str(&format!(" (it fetched from {})", format_npt(fetched)));
                        }
                        match watchdog_action(target, length) {
                            WatchdogAction::End => {
                                inner.note(format!("{why}: that is the end of the media, so the cast ends"));
                                let _ = inner.dlna.stop();
                                inner.finish(CastOutcome::Ended);
                                return;
                            }
                            WatchdogAction::Restart(to) => {
                                if let Ok(_g) = inner.control.try_lock() {
                                    let _ = inner.restart_at(to, &why);
                                }
                            }
                        }
                        last_state = Some(state);
                        continue;
                    }
                }
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
            seekable: true,
            tv_friendly,
            state: Some(state),
            position: Some(Duration::from_secs(5)),
            duration: Some(Duration::from_secs(300)),
            started: true,
            outcome: None,
            total_bytes: 0,
            last_mbps: None,
        };
        let restart = tv_friendly.then(|| Restart {
            url: status.url.clone(),
            title: "x".into(),
            mime: "video/x-matroska".into(),
            length: Some(Duration::from_secs(300)),
        });
        CastSession {
            inner: Arc::new(SessionInner {
                dlna: Dlna::new("192.0.2.1"),
                still: false,
                server: None,
                bus: EventBus::new(),
                status: Mutex::new(status),
                seek_memo: SeekMemo::default(),
                restart,
                base: Mutex::new(Duration::ZERO),
                hold: Mutex::new(Hold::None),
                control: Mutex::new(()),
                stop_flag: AtomicBool::new(true),
                done: (Mutex::new(false), Condvar::new()),
            }),
        }
    }

    #[test]
    fn seeking_while_paused_by_the_app_moves_the_resume_point() {
        // The TV refused Pause on the stream, so the app stopped it at 0:05.
        let s = offline_session(true, TransportState::Paused);
        s.inner.set_hold(Hold::Paused(Duration::from_secs(5)));
        let rx = s.inner.bus.subscribe();
        let t0 = Instant::now();
        assert_eq!(s.seek_relative(SEEK_STEP, true).unwrap(), Duration::from_secs(15));
        assert_eq!(s.inner.hold(), Hold::Paused(Duration::from_secs(15)));
        assert_eq!(s.status().position, Some(Duration::from_secs(15)));
        s.seek(Duration::from_secs(900)).unwrap();
        assert_eq!(s.inner.hold(), Hold::Paused(Duration::from_secs(290)), "kept 10 s before the end of a stream");
        // Pausing again changes nothing (no request to the TV either).
        s.pause().unwrap();
        assert!(t0.elapsed() < Duration::from_millis(500), "no network");
        assert!(matches!(rx.try_recv(), Ok(Event::Cast(CastEvent::Note(n))) if n.contains("resume restarts at")));
    }

    #[test]
    fn positions_on_the_media_timeline() {
        let s = Duration::from_secs;
        assert_eq!(true_position(s(60), Some(s(5))), Some(s(65)));
        assert_eq!(true_position(s(60), None), None);
        assert_eq!(restart_url("http://192.0.2.10:4000/a%20b.tv.mkv", s(0)), "http://192.0.2.10:4000/a%20b.tv.mkv");
        let url = restart_url("http://192.0.2.10:4000/a%20b.tv.mkv", Duration::from_millis(90_500));
        assert_eq!(url, "http://192.0.2.10:4000/a%20b.tv.mkv?t=90.500");
        assert_eq!(start_offset(&url), Duration::from_millis(90_500));
        // Another process sees the restarted stream on the media's timeline too.
        let pos = PositionInfo {
            position: Some(s(10)),
            duration: Some(s(210)),
            track_uri: restart_url("http://192.0.2.10:4000/film.tv.mkv", s(90)),
            track_metadata: String::new(),
        };
        let st = CastStatus::from_renderer(&TransportState::Playing, &pos).unwrap();
        assert_eq!((st.position, st.duration), (Some(s(100)), Some(s(300))));
        assert_eq!(st.title, "film.tv.mkv");
        assert!(st.tv_friendly && st.seekable);
    }

    #[test]
    fn short_seeks_are_seeks_not_buffering() {
        let s = Duration::from_secs;
        // `seek 100%` from 4:57 jumps 2 s: normal-looking progress, but a seek.
        assert!(is_seek_transition(Some(s(297)), Some(s(299)), Some(s(1))));
        assert!(is_seek_transition(Some(s(10)), Some(s(10)), Some(s(4))), "seek to the same point");
        // No recent seek: only a jump counts.
        assert!(!is_seek_transition(Some(s(297)), Some(s(299)), None));
        assert!(!is_seek_transition(Some(s(297)), Some(s(299)), Some(s(30))));
        assert!(is_seek_transition(Some(s(10)), Some(s(120)), None));
        assert!(is_seek_transition(Some(s(120)), Some(s(10)), None));
        assert!(!is_seek_transition(None, Some(s(10)), None));
    }

    #[test]
    fn watchdog_restarts_only_a_stream_stuck_after_a_seek() {
        let s = Duration::from_secs;
        // Seen live: TRANSITIONING at exactly the target for 45-80 s after a seek.
        assert!(stuck_after_seek(s(6), Some(s(7)), false));
        assert!(stuck_after_seek(s(5), Some(s(5)), false));
        assert!(!stuck_after_seek(s(4), Some(s(5)), false), "give the TV 5 s");
        assert!(!stuck_after_seek(s(30), Some(s(7)), true), "the position moves: just slow");
        assert!(!stuck_after_seek(s(30), None, false), "no seek: plain buffering");
        assert!(!stuck_after_seek(s(6), Some(s(60)), false), "a seek long before this spell");
    }

    #[test]
    fn streamed_casts_keep_away_from_the_end() {
        let s = Duration::from_secs;
        let len = Some(s(300));
        // Direct casts played fine at 4:59; a TV-friendly cast wedged the TV there.
        assert_eq!(clamp_end(s(300), len, false), s(299));
        assert_eq!(clamp_end(s(300), len, true), s(290));
        assert_eq!(clamp_end(s(297), len, true), s(290), "`seek 4:57`");
        assert_eq!(clamp_end(s(200), len, true), s(200));
        assert_eq!(clamp_end(s(500), None, true), s(500), "unknown length");
        assert_eq!(clamp_end(fraction_target(s(300), 1.0), len, true), s(290), "`seek 100%`, last bar column");
        assert_eq!(end_margin(true), crate::server::MIN_STREAM_TAIL);
        assert_eq!(end_margin(false), SEEK_END_MARGIN);
    }

    #[test]
    fn watchdog_decisions() {
        let s = Duration::from_secs;
        assert_eq!(watchdog_action(s(120), Some(s(300))), WatchdogAction::Restart(s(120)));
        assert_eq!(watchdog_action(s(290), Some(s(300))), WatchdogAction::Restart(s(290)));
        assert_eq!(watchdog_action(s(299), Some(s(300))), WatchdogAction::End, "seen live: stuck at 4:59");
        assert_eq!(watchdog_action(s(299), None), WatchdogAction::Restart(s(299)));
        // The restart reached the TV even though its reply timed out.
        let url = "http://192.0.2.10:4000/a%20b.tv.mkv?t=295.000";
        assert!(took_uri(url, url));
        assert!(took_uri("http://192.0.2.10:4000/a b.tv.mkv?t=295.000 ", url), "unescaped by the TV");
        assert!(!took_uri("http://192.0.2.10:4000/a%20b.tv.mkv", url), "still the old stream");
        assert!(!took_uri("", url));
    }

    #[test]
    fn end_of_a_restarted_stream() {
        let s = Duration::from_secs;
        // Restarted at 3:25 of 5:00: the TV sits at 1:35 / 1:35, ffmpeg done.
        let len = Some(s(300));
        assert!(stream_end_reached(true_position(s(205), Some(s(95))), len, 2, true));
        assert!(stream_end_reached(Some(s(299)), len, 3, true), "within 2 s of the end");
        assert!(!stream_end_reached(Some(s(300)), len, 1, true), "one frozen poll is not enough");
        assert!(!stream_end_reached(Some(s(300)), len, 5, false), "ffmpeg still running");
        assert!(!stream_end_reached(Some(s(200)), len, 5, true), "frozen mid-way is a stall");
        assert!(!stream_end_reached(Some(s(300)), None, 5, true), "unknown length");
    }

    #[test]
    fn seeks_while_paused_stay_paused() {
        let paused = TransportState::Paused;
        assert!(repause_needed(Some(&paused), Hold::None), "the TV plays after its own seek");
        assert!(!repause_needed(Some(&paused), Hold::Paused(Duration::from_secs(5))), "the app holds it");
        assert!(!repause_needed(Some(&TransportState::Playing), Hold::None));
        assert!(!repause_needed(None, Hold::None));
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
        // External: a cast at start-up (0:00:00 length), TV-friendly or not, is loading.
        let pos = PositionInfo { duration: Some(Duration::ZERO), track_uri: "http://a/f.tv.mkv".into(), ..Default::default() };
        let loading = CastStatus::from_renderer(&TransportState::Transitioning, &pos).unwrap();
        assert!(!loading.seekable && loading.tv_friendly);
        assert!(loading.no_seek_error().to_string().contains("still loading"));
        assert!(owner_only("seek").to_string().contains("seek it from the app that started it"));
    }

    #[test]
    fn tv_friendly_streams_seek_genuinely_live_ones_do_not() {
        let friendly = PositionInfo {
            position: Some(Duration::from_secs(3)),
            duration: Some(Duration::from_secs(300)),
            track_uri: "http://192.0.2.10:41234/Le%20film.tv.mkv".into(),
            track_metadata: String::new(),
        };
        assert!(is_tv_friendly(&friendly) && !is_live_stream(&friendly));
        assert_eq!(renderer_activity(&TransportState::Playing, &friendly), Some(true));
        let ts = PositionInfo { track_uri: "http://192.0.2.10:1/x.TV.ts?t=60".into(), ..friendly.clone() };
        assert!(is_tv_friendly(&ts));
        let direct = PositionInfo { track_uri: "http://192.0.2.10:1/tv.mkv".into(), ..friendly.clone() };
        assert!(!is_tv_friendly(&direct), "a file named tv.mkv is not a stream");
        let op00 = PositionInfo {
            track_metadata: "<res protocolInfo=\"http-get:*:audio/mpeg:DLNA.ORG_OP=00;DLNA.ORG_CI=0\">".into(),
            ..direct
        };
        assert!(is_live_stream(&op00));
        assert_eq!(renderer_activity(&TransportState::Playing, &op00), Some(false));
        // Our own stream metadata time-seeks.
        let ours = PositionInfo { track_metadata: crate::dlna::DLNA_FLAGS_STREAM.into(), ..friendly };
        assert!(!is_live_stream(&ours));
    }

    #[test]
    fn request_builders() {
        let r = CastRequest::url("http://example.invalid/a/b.mp4?x");
        assert_eq!(r.source, CastSource::Url("http://example.invalid/a/b.mp4?x".into()));
        assert_eq!(CastRequest::file("/x").mode, CastMode::Direct);
    }
}
