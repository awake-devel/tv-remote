//! Minimal HTTP server the TV pulls media from.
//!
//! * [`ServeSource::File`]: one local file with `Range`/206 support so the TV can seek
//!   (port of tv-cast's `RangeHandler`: HTTP/1.0, 1 MiB writes, DLNA headers).
//! * [`ServeSource::Command`]: a stream from a child process' stdout (ffmpeg), sent
//!   with chunked transfer encoding. Every GET starts a fresh process (the previous one
//!   is killed and reaped first), so the TV's probe request and its real request both
//!   get a whole stream. Seekable by time, the DLNA way: a request with
//!   `TimeSeekRange.dlna.org: npt=<start>-` starts the process `start` into the media
//!   (answered with the range served), and a `?t=<secs>` query (the app's own restart,
//!   see `CastSession::seek`) shifts the whole stream: see [`stream_start`].
//!
//! Every byte sent is counted; a sampler thread records bytes/s once per second
//! (see [`FileServer::samples`]), which feeds the cast throughput graph and the
//! network capacity test.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// `contentFeatures.dlna.org` for seekable files (as in tv-cast).
pub const DLNA_FEATURES_FILE: &str = "DLNA.ORG_OP=01;DLNA.ORG_CI=0";
/// `contentFeatures.dlna.org` for transcoded streams: time seek, HTTP stalling (see
/// [`crate::dlna::DLNA_FLAGS_STREAM`]).
pub const DLNA_FEATURES_STREAM: &str = crate::dlna::DLNA_FLAGS_STREAM;

/// MIME type for a media path (tv-cast's table plus common extras; default `video/mp4`).
pub fn mime_for_path(path: &Path) -> &'static str {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "mkv" => "video/x-mkv",
        "webm" => "video/webm",
        "flac" => "audio/x-flac",
        "m4a" => "audio/mp4",
        "ts" | "m2ts" | "mts" => "video/vnd.dlna.mpeg-tts",
        "avi" => "video/x-msvideo",
        "mp4" | "m4v" => "video/mp4",
        "mov" => "video/quicktime",
        "mpg" | "mpeg" => "video/mpeg",
        "wmv" => "video/x-ms-wmv",
        "mp3" => "audio/mpeg",
        "aac" => "audio/aac",
        "wav" => "audio/wav",
        "ogg" | "oga" => "audio/ogg",
        "opus" => "audio/ogg",
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => "video/mp4",
    }
}

/// Percent-encode a file name for a URL path (like Python's `urllib.parse.quote`).
pub fn url_quote(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~/".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Decode `%XX` escapes (invalid escapes are kept as they are; invalid UTF-8 is
/// replaced). Pure.
pub fn url_unquote(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = b
            .get(i + 1..i + 3)
            .filter(|h| h.iter().all(u8::is_ascii_hexdigit))
            .and_then(|h| std::str::from_utf8(h).ok());
        match (b[i], hex.and_then(|h| u8::from_str_radix(h, 16).ok())) {
            (b'%', Some(v)) => {
                out.push(v);
                i += 3;
            }
            (c, _) => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The local IP address used to reach `host` (UDP connect trick; sends no packet).
pub fn local_ip_for(host: &str) -> Result<IpAddr> {
    let s = UdpSocket::bind("0.0.0.0:0")?;
    s.connect((host, 9))?;
    Ok(s.local_addr()?.ip())
}

/// Result of parsing a `Range` header against a file size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeSpec {
    /// No (usable) range: send the whole file with 200.
    Full,
    /// Send bytes `start..=end` with 206.
    Partial {
        /// First byte.
        start: u64,
        /// Last byte (inclusive).
        end: u64,
    },
    /// Range starts beyond the end: 416.
    Unsatisfiable,
}

/// Parse `bytes=START-END`, `bytes=START-` or `bytes=-SUFFIX` like tv-cast
/// (only the first range of a multi-range header is honoured; `end` is clamped).
pub fn parse_range(header: Option<&str>, size: u64) -> RangeSpec {
    let Some(h) = header else { return RangeSpec::Full };
    let Some(spec) = h.trim().strip_prefix("bytes=") else {
        return RangeSpec::Full;
    };
    let first = spec.split(',').next().unwrap_or("");
    let Some((a, b)) = first.split_once('-') else {
        return RangeSpec::Full;
    };
    let (a, b) = (a.trim(), b.trim());
    let digits = |s: &str| s.chars().all(|c| c.is_ascii_digit());
    if !digits(a) || !digits(b) || (a.is_empty() && b.is_empty()) {
        return RangeSpec::Full;
    }
    if size == 0 {
        return RangeSpec::Unsatisfiable;
    }
    let last = size - 1;
    if !a.is_empty() {
        let Ok(start) = a.parse::<u64>() else { return RangeSpec::Full };
        let end = if b.is_empty() {
            last
        } else {
            b.parse::<u64>().unwrap_or(last).min(last)
        };
        if start > last {
            return RangeSpec::Unsatisfiable;
        }
        if end < start {
            return RangeSpec::Full;
        }
        RangeSpec::Partial { start, end }
    } else {
        let n = b.parse::<u64>().unwrap_or(0);
        if n == 0 {
            return RangeSpec::Unsatisfiable;
        }
        RangeSpec::Partial {
            start: size.saturating_sub(n),
            end: last,
        }
    }
}

/// Where a stream process starts ([`StreamFactory`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StreamStart {
    /// Position in the media to start from.
    pub at: Duration,
    /// Make the output timestamps start here (`-output_ts_offset`): the `npt` start the
    /// TV asked for, in its own timeline. `None`: they start at 0.
    pub ts_offset: Option<Duration>,
}

/// Creates the child process for a stream starting at the given point. Its stdout
/// must be piped.
pub type StreamFactory = Arc<dyn Fn(StreamStart) -> Result<Child> + Send + Sync>;

/// What the server serves.
#[derive(Clone)]
pub enum ServeSource {
    /// A local file, seekable via Range.
    File(PathBuf),
    /// A stream from a child process' stdout (one process per GET), seekable by time.
    Command {
        /// Starts the process.
        factory: StreamFactory,
        /// Length of the whole media, if known (for `TimeSeekRange` answers).
        length: Option<Duration>,
    },
}

impl std::fmt::Debug for ServeSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ServeSource::File(p) => f.debug_tuple("File").field(p).finish(),
            ServeSource::Command { length, .. } => f.debug_struct("Command").field("length", length).finish(),
        }
    }
}

/// Parse one npt time: seconds (`90`, `90.5`) or `H:MM:SS[.fff]`. Pure.
fn parse_npt_time(s: &str) -> Option<Duration> {
    let s = s.trim();
    let plain = |p: &str, frac: bool| {
        !p.is_empty()
            && p.chars().all(|c| c.is_ascii_digit() || (frac && c == '.'))
            && p.matches('.').count() <= 1
            && p.chars().any(|c| c.is_ascii_digit())
    };
    let parts: Vec<&str> = s.split(':').collect();
    let secs = match parts.as_slice() {
        [sec] if plain(sec, true) => sec.parse::<f64>().ok()?,
        [h, m, sec] if plain(h, false) && plain(m, false) && plain(sec, true) => {
            let (h, m, sec): (f64, f64, f64) = (h.parse().ok()?, m.parse().ok()?, sec.parse().ok()?);
            if m >= 60.0 || sec >= 60.0 {
                return None;
            }
            h * 3600.0 + m * 60.0 + sec
        }
        _ => return None,
    };
    Some(Duration::from_secs_f64(secs))
}

/// Parse a `TimeSeekRange.dlna.org` request value, `npt=<start>-[<end>]` (times in
/// seconds or `H:MM:SS[.fff]`); `None` if malformed (e.g. `npt=now-`). Pure.
pub fn parse_time_seek(value: &str) -> Option<(Duration, Option<Duration>)> {
    let v = value.trim();
    let spec = v.get(..4).filter(|p| p.eq_ignore_ascii_case("npt="))?;
    let (a, b) = v[spec.len()..].split_once('-')?;
    let start = parse_npt_time(a)?;
    let end = if b.trim().is_empty() { None } else { Some(parse_npt_time(b)?) };
    if end.is_some_and(|e| e < start) {
        return None;
    }
    Some((start, end))
}

/// `H:MM:SS.mmm`, as npt times are written in answers. Pure.
pub fn format_npt(d: Duration) -> String {
    let ms = d.as_millis();
    format!("{}:{:02}:{:02}.{:03}", ms / 3_600_000, (ms / 60_000) % 60, (ms / 1000) % 60, ms % 1000)
}

/// The `TimeSeekRange.dlna.org` answer: `npt=<start>-<end>/<length>`, with `end` =
/// the length (the stream runs to the end) and `*` for an unknown length. Pure.
pub fn time_seek_response(start: Duration, length: Option<Duration>) -> String {
    match length {
        Some(l) => format!("npt={}-{}/{}", format_npt(start), format_npt(l), format_npt(l)),
        None => format!("npt={}-/*", format_npt(start)),
    }
}

/// The shortest end of a stream worth starting. Measured on the 2025 TV: asked to seek
/// to 4:59 of a 5:00 TV-friendly cast, it fetched `npt=295` (it rounds the target down,
/// to whole seconds and a keyframe), sat TRANSITIONING on the 5 s tail and stopped
/// answering DLNA; a TV that has to buffer before playing needs more than that. 10 s
/// leaves room for its rounding (seen up to 4 s) plus a tail it can buffer.
pub const MIN_STREAM_TAIL: Duration = Duration::from_secs(10);

/// A time-seek start kept at least [`MIN_STREAM_TAIL`] before the end (`length`, on
/// the TV's timeline; unknown: unchanged). Pure.
pub fn keep_tail(npt: Duration, length: Option<Duration>) -> Duration {
    match length.filter(|l| !l.is_zero()) {
        Some(l) => npt.min(l.saturating_sub(MIN_STREAM_TAIL)),
        None => npt,
    }
}

/// Restart offset in a stream URL or request path: the `t=<secs>` query parameter
/// (`/film.tv.mkv?t=90.5`), 0 when absent or malformed. Pure.
pub fn start_offset(path_or_url: &str) -> Duration {
    let Some((_, q)) = path_or_url.split_once('?') else { return Duration::ZERO };
    q.split('&')
        .filter_map(|kv| kv.strip_prefix("t="))
        .find_map(|v| {
            let ok = !v.is_empty() && v.chars().all(|c| c.is_ascii_digit() || c == '.') && v.matches('.').count() <= 1;
            ok.then(|| v.parse::<f64>().ok()).flatten()
        })
        .map(Duration::from_secs_f64)
        .unwrap_or_default()
}

/// Where a stream request starts, and the length the TV sees: `restart` (the `?t=`
/// offset) shifts the TV's whole timeline, then `npt` (from `TimeSeekRange`) is a point
/// on that timeline. Returns the process start and the TV-side length (`length` −
/// `restart`). Pure.
pub fn stream_start(restart: Duration, npt: Option<Duration>, length: Option<Duration>) -> (StreamStart, Option<Duration>) {
    let start = StreamStart {
        at: restart + npt.unwrap_or_default(),
        ts_offset: npt,
    };
    (start, length.map(|l| l.saturating_sub(restart)))
}

/// One throughput sample.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ThroughputSample {
    /// Seconds since the server started.
    pub t_secs: f64,
    /// Bytes sent during the last second.
    pub bytes_per_sec: u64,
}

impl ThroughputSample {
    /// Throughput in Mbit/s.
    pub fn mbps(&self) -> f64 {
        self.bytes_per_sec as f64 * 8.0 / 1e6
    }
}

/// Maximum number of 1 s samples kept (10 minutes).
const MAX_SAMPLES: usize = 600;

struct Inner {
    source: ServeSource,
    mime: String,
    stop: AtomicBool,
    total: AtomicU64,
    active: AtomicUsize,
    requests: AtomicU64,
    started: Instant,
    first_byte: Mutex<Option<Instant>>,
    samples: Mutex<VecDeque<ThroughputSample>>,
    /// Stream processes still producing: (generation, started, process).
    children: Mutex<Vec<(u64, Instant, Child)>>,
    generation: AtomicU64,
    /// Generation of the last stream process that reached its end (ffmpeg done).
    done_gen: AtomicU64,
    /// Requests that carried `TimeSeekRange.dlna.org`.
    time_seeks: AtomicU64,
    /// The last of them: when, and where its stream starts in the media.
    last_time_seek: Mutex<Option<(Instant, Duration)>>,
    log: Mutex<VecDeque<String>>,
}

fn reap(mut ch: Child) {
    let _ = ch.kill();
    let _ = ch.wait();
}

impl Inner {
    fn kill_child(&self) {
        if let Ok(mut c) = self.children.lock() {
            c.drain(..).for_each(|(_, _, ch)| reap(ch));
        }
    }

    /// Kill the stream process of generation `gen` (its connection is over).
    fn kill_generation(&self, gen: u64) {
        if let Ok(mut c) = self.children.lock() {
            if let Some(i) = c.iter().position(|(g, _, _)| *g == gen) {
                reap(c.remove(i).2);
            }
        }
    }
}

/// Requests that arrive this close together are siblings of one fetch (a TV often
/// opens a probe connection next to the real one, in either order), so neither stops
/// the other; an older stream is replaced by a new request (the TV moved on: a seek).
pub const SIBLING_WINDOW: Duration = Duration::from_secs(2);

/// Does a new request replace a stream process started `age` ago? Pure.
pub fn supersedes(age: Duration) -> bool {
    age >= SIBLING_WINDOW
}

/// A running media server. Stops when [`FileServer::stop`] is called or the last
/// clone is dropped.
#[derive(Clone)]
pub struct FileServer {
    inner: Arc<Inner>,
    port: u16,
    name: String,
    _guard: Arc<StopGuard>,
}

struct StopGuard(Arc<Inner>);

impl Drop for StopGuard {
    fn drop(&mut self) {
        self.0.stop.store(true, Ordering::SeqCst);
        self.0.kill_child();
    }
}

impl std::fmt::Debug for FileServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileServer")
            .field("port", &self.port)
            .field("source", &self.inner.source)
            .finish()
    }
}

impl FileServer {
    /// Bind `bind_ip:0` (use `0.0.0.0` for the TV, `127.0.0.1` in tests) and start
    /// serving. `name` is the URL path component (usually the file name), `mime`
    /// the Content-Type.
    pub fn start(source: ServeSource, mime: &str, name: &str, bind_ip: IpAddr) -> Result<FileServer> {
        if let ServeSource::File(p) = &source {
            if !p.is_file() {
                return Err(Error::Io(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("{} is not a file", p.display()),
                )));
            }
        }
        let listener = TcpListener::bind(SocketAddr::new(bind_ip, 0))?;
        listener.set_nonblocking(true)?;
        let port = listener.local_addr()?.port();
        let inner = Arc::new(Inner {
            source,
            mime: mime.to_string(),
            stop: AtomicBool::new(false),
            total: AtomicU64::new(0),
            active: AtomicUsize::new(0),
            requests: AtomicU64::new(0),
            started: Instant::now(),
            first_byte: Mutex::new(None),
            samples: Mutex::new(VecDeque::new()),
            children: Mutex::new(Vec::new()),
            generation: AtomicU64::new(0),
            done_gen: AtomicU64::new(0),
            time_seeks: AtomicU64::new(0),
            last_time_seek: Mutex::new(None),
            log: Mutex::new(VecDeque::new()),
        });
        let acc = inner.clone();
        std::thread::Builder::new()
            .name("tv-media-accept".into())
            .spawn(move || accept_loop(listener, acc))?;
        let smp = inner.clone();
        std::thread::Builder::new()
            .name("tv-media-sampler".into())
            .spawn(move || sampler(smp))?;
        Ok(FileServer {
            _guard: Arc::new(StopGuard(inner.clone())),
            inner,
            port,
            name: name.to_string(),
        })
    }

    /// Listening port.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// URL the TV should fetch, given this PC's LAN address.
    pub fn url(&self, local_ip: IpAddr) -> String {
        format!("http://{}:{}/{}", local_ip, self.port, url_quote(&self.name))
    }

    /// Content-Type served.
    pub fn mime(&self) -> &str {
        &self.inner.mime
    }

    /// True for a process stream source (TV-friendly).
    pub fn is_stream(&self) -> bool {
        matches!(self.inner.source, ServeSource::Command { .. })
    }

    /// Number of stream requests so far that carried `TimeSeekRange.dlna.org` (the TV
    /// seeking natively).
    pub fn time_seek_count(&self) -> u64 {
        self.inner.time_seeks.load(Ordering::SeqCst)
    }

    /// The last `TimeSeekRange` fetch (from whoever sent the TV its `Seek`): when, and
    /// where in the media its stream starts.
    pub fn last_time_seek(&self) -> Option<(Instant, Duration)> {
        self.inner.last_time_seek.lock().ok().and_then(|g| *g)
    }

    /// The newest stream process has produced everything (ffmpeg reached the end of
    /// the media); false while one is running.
    pub fn stream_done(&self) -> bool {
        let g = self.inner.generation.load(Ordering::SeqCst);
        g > 0 && self.inner.done_gen.load(Ordering::SeqCst) == g
    }

    /// Total bytes sent so far.
    pub fn total_bytes(&self) -> u64 {
        self.inner.total.load(Ordering::Relaxed)
    }

    /// Number of requests received so far.
    pub fn request_count(&self) -> u64 {
        self.inner.requests.load(Ordering::Relaxed)
    }

    /// Connections currently being served.
    pub fn active_connections(&self) -> usize {
        self.inner.active.load(Ordering::Relaxed)
    }

    /// When the first body byte was sent (the TV started pulling).
    pub fn first_byte_at(&self) -> Option<Instant> {
        self.inner.first_byte.lock().ok().and_then(|g| *g)
    }

    /// Per-second throughput samples (oldest first, up to 10 minutes).
    pub fn samples(&self) -> Vec<ThroughputSample> {
        self.inner
            .samples
            .lock()
            .map(|s| s.iter().copied().collect())
            .unwrap_or_default()
    }

    /// Last request lines (method, path, Range header) for debugging.
    pub fn request_log(&self) -> Vec<String> {
        self.inner
            .log
            .lock()
            .map(|l| l.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Stop accepting, abort transfers, kill any stream process.
    pub fn stop(&self) {
        self.inner.stop.store(true, Ordering::SeqCst);
        self.inner.kill_child();
    }

    /// True once stopped.
    pub fn is_stopped(&self) -> bool {
        self.inner.stop.load(Ordering::SeqCst)
    }
}

fn accept_loop(listener: TcpListener, inner: Arc<Inner>) {
    while !inner.stop.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => {
                let i = inner.clone();
                let _ = std::thread::Builder::new()
                    .name("tv-media-conn".into())
                    .spawn(move || {
                        i.active.fetch_add(1, Ordering::SeqCst);
                        let _ = handle(stream, &i);
                        i.active.fetch_sub(1, Ordering::SeqCst);
                    });
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => std::thread::sleep(Duration::from_millis(100)),
        }
    }
}

fn sampler(inner: Arc<Inner>) {
    let mut last = 0u64;
    let mut tick = Instant::now();
    while !inner.stop.load(Ordering::SeqCst) {
        tick += Duration::from_secs(1);
        let now = Instant::now();
        if tick > now {
            std::thread::sleep(tick - now);
        } else {
            tick = now;
        }
        let total = inner.total.load(Ordering::Relaxed);
        let s = ThroughputSample {
            t_secs: inner.started.elapsed().as_secs_f64(),
            bytes_per_sec: total - last,
        };
        last = total;
        if let Ok(mut v) = inner.samples.lock() {
            v.push_back(s);
            while v.len() > MAX_SAMPLES {
                v.pop_front();
            }
        }
    }
}

struct Request {
    method: String,
    path: String,
    range: Option<String>,
    time_seek: Option<String>,
}

fn read_request(stream: &TcpStream) -> Result<Request> {
    let mut r = BufReader::new(stream);
    let mut line = String::new();
    r.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("/").to_string();
    let mut range = None;
    let mut time_seek = None;
    let mut total = line.len();
    loop {
        let mut h = String::new();
        let n = r.read_line(&mut h)?;
        total += n;
        if n == 0 || h == "\r\n" || h == "\n" || total > 32 * 1024 {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            if k.trim().eq_ignore_ascii_case("range") {
                range = Some(v.trim().to_string());
            } else if k.trim().eq_ignore_ascii_case("timeseekrange.dlna.org") {
                time_seek = Some(v.trim().to_string());
            }
        }
    }
    Ok(Request {
        method,
        path,
        range,
        time_seek,
    })
}

fn count(inner: &Inner, n: usize) {
    if n > 0 {
        inner.total.fetch_add(n as u64, Ordering::Relaxed);
        if let Ok(mut f) = inner.first_byte.lock() {
            if f.is_none() {
                *f = Some(Instant::now());
            }
        }
    }
}

fn handle(mut stream: TcpStream, inner: &Inner) -> Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(15)))?;
    stream.set_write_timeout(Some(Duration::from_secs(30)))?;
    let req = read_request(&stream)?;
    inner.requests.fetch_add(1, Ordering::Relaxed);
    if let Ok(mut l) = inner.log.lock() {
        l.push_back(format!(
            "{} {} {} {}",
            req.method,
            req.path,
            req.range.as_deref().unwrap_or("-"),
            req.time_seek.as_deref().map(|t| format!("TimeSeekRange {t}")).unwrap_or_default()
        ));
        while l.len() > 100 {
            l.pop_front();
        }
    }
    let head_only = match req.method.as_str() {
        "HEAD" => true,
        "GET" => false,
        _ => {
            stream.write_all(b"HTTP/1.0 405 Method Not Allowed\r\nContent-Length: 0\r\n\r\n")?;
            return Ok(());
        }
    };
    match &inner.source {
        ServeSource::File(p) => serve_file(&mut stream, inner, p, req.range.as_deref(), head_only),
        ServeSource::Command { factory, length } => {
            serve_stream(&mut stream, inner, factory, *length, &req, head_only)
        }
    }
}

fn serve_file(
    stream: &mut TcpStream,
    inner: &Inner,
    path: &Path,
    range: Option<&str>,
    head_only: bool,
) -> Result<()> {
    let mut f = File::open(path)?;
    let size = f.metadata()?.len();
    let (status, start, end) = match parse_range(range, size) {
        RangeSpec::Full => ("200 OK", 0, size.saturating_sub(1)),
        RangeSpec::Partial { start, end } => ("206 Partial Content", start, end),
        RangeSpec::Unsatisfiable => {
            let h = format!(
                "HTTP/1.0 416 Requested Range Not Satisfiable\r\nContent-Range: bytes */{size}\r\nContent-Length: 0\r\n\r\n"
            );
            stream.write_all(h.as_bytes())?;
            return Ok(());
        }
    };
    let len = if size == 0 { 0 } else { end - start + 1 };
    let mut h = format!("HTTP/1.0 {status}\r\n");
    if status.starts_with("206") {
        h.push_str(&format!("Content-Range: bytes {start}-{end}/{size}\r\n"));
    }
    h.push_str(&format!(
        "Content-Type: {}\r\nContent-Length: {len}\r\nAccept-Ranges: bytes\r\n\
         transferMode.dlna.org: Streaming\r\ncontentFeatures.dlna.org: {DLNA_FEATURES_FILE}\r\n\
         Connection: close\r\n\r\n",
        inner.mime
    ));
    stream.write_all(h.as_bytes())?;
    if head_only || len == 0 {
        return Ok(());
    }
    f.seek(SeekFrom::Start(start))?;
    let mut left = len;
    let mut buf = vec![0u8; 1 << 20];
    while left > 0 && !inner.stop.load(Ordering::Relaxed) {
        let want = buf.len().min(left as usize);
        let n = f.read(&mut buf[..want])?;
        if n == 0 {
            break;
        }
        if stream.write_all(&buf[..n]).is_err() {
            break; // TV closed the connection (seek / stop): normal.
        }
        count(inner, n);
        left -= n as u64;
    }
    Ok(())
}

fn serve_stream(
    stream: &mut TcpStream,
    inner: &Inner,
    factory: &StreamFactory,
    length: Option<Duration>,
    req: &Request,
    head_only: bool,
) -> Result<()> {
    let restart = start_offset(&req.path);
    let tv_length = length.map(|l| l.saturating_sub(restart));
    // Never serve a tail of a few seconds: the TV can't start one (see `MIN_STREAM_TAIL`).
    let npt = req
        .time_seek
        .as_deref()
        .and_then(parse_time_seek)
        .map(|(s, _)| keep_tail(s, tv_length));
    let (start, tv_length) = stream_start(restart, npt, length);
    // Only a GET fetches the new point (a HEAD is a probe).
    if npt.is_some() && !head_only {
        inner.time_seeks.fetch_add(1, Ordering::SeqCst);
        if let Ok(mut l) = inner.last_time_seek.lock() {
            *l = Some((Instant::now(), start.at));
        }
    }
    // DLNA answers a time seek with 200 and the range served (as minidlna does).
    let seek_line = npt
        .map(|n| format!("TimeSeekRange.dlna.org: {}\r\n", time_seek_response(n, tv_length)))
        .unwrap_or_default();
    let h = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nTransfer-Encoding: chunked\r\nAccept-Ranges: none\r\n\
         {seek_line}transferMode.dlna.org: Streaming\r\ncontentFeatures.dlna.org: {DLNA_FEATURES_STREAM}\r\n\
         Connection: close\r\n\r\n",
        inner.mime
    );
    if head_only {
        // No body for HEAD; don't advertise chunked encoding without one.
        let h = h.replace("Transfer-Encoding: chunked\r\n", "");
        stream.write_all(h.as_bytes())?;
        return Ok(());
    }
    // Replace older producers (the TV moved on), but not a sibling started moments ago:
    // killing the connection the TV is about to play from leaves it waiting forever on
    // the other one, which it never reads (see `SIBLING_WINDOW`). Done under one lock,
    // so racing requests can never leave an orphan process behind; a sibling the TV
    // drops ends with its connection (or the next seek).
    let (gen, mut out) = {
        let mut kids = inner.children.lock().map_err(|_| Error::Media("stream state poisoned".into()))?;
        let now = Instant::now();
        let (old, keep): (Vec<_>, Vec<_>) = kids.drain(..).partition(|(_, t, _)| supersedes(now - *t));
        *kids = keep;
        old.into_iter().for_each(|(_, _, ch)| reap(ch));
        let mut child = factory(start)?;
        let out = child
            .stdout
            .take()
            .ok_or_else(|| Error::Media("stream process has no stdout".into()));
        let out = match out {
            Ok(o) => o,
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(e);
            }
        };
        let gen = inner.generation.fetch_add(1, Ordering::SeqCst) + 1;
        kids.push((gen, now, child));
        (gen, out)
    };
    if let Err(e) = stream.write_all(h.as_bytes()) {
        inner.kill_generation(gen);
        return Err(e.into());
    }
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        if inner.stop.load(Ordering::Relaxed) {
            break;
        }
        let n = match out.read(&mut buf) {
            Ok(0) => {
                // The end of the media (not a kill: that ends the read too, but a newer
                // generation exists by then, so `stream_done` stays false).
                inner.done_gen.store(gen, Ordering::SeqCst);
                let _ = stream.write_all(b"0\r\n\r\n");
                break;
            }
            Ok(n) => n,
            Err(_) => break,
        };
        let ok = stream
            .write_all(format!("{n:X}\r\n").as_bytes())
            .and_then(|_| stream.write_all(&buf[..n]))
            .and_then(|_| stream.write_all(b"\r\n"));
        if ok.is_err() {
            // The TV hung up; stop this producer (not a newer one).
            inner.kill_generation(gen);
            break;
        }
        count(inner, n);
    }
    // Reap this producer (no-op if a newer request already replaced it).
    inner.kill_generation(gen);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn range_parsing() {
        assert_eq!(parse_range(None, 100), RangeSpec::Full);
        assert_eq!(parse_range(Some("bytes=-"), 100), RangeSpec::Full);
        assert_eq!(parse_range(Some("bytes=0-"), 100), RangeSpec::Partial { start: 0, end: 99 });
        assert_eq!(parse_range(Some("bytes=10-19"), 100), RangeSpec::Partial { start: 10, end: 19 });
        assert_eq!(parse_range(Some("bytes=10-500"), 100), RangeSpec::Partial { start: 10, end: 99 });
        assert_eq!(parse_range(Some("bytes=-30"), 100), RangeSpec::Partial { start: 70, end: 99 });
        assert_eq!(parse_range(Some("bytes=-300"), 100), RangeSpec::Partial { start: 0, end: 99 });
        assert_eq!(parse_range(Some("bytes=100-"), 100), RangeSpec::Unsatisfiable);
        assert_eq!(parse_range(Some("items=1-2"), 100), RangeSpec::Full);
        assert_eq!(parse_range(Some("bytes=5-9,20-30"), 100), RangeSpec::Partial { start: 5, end: 9 });
    }

    #[test]
    fn misc_helpers() {
        assert_eq!(url_quote("a b/é.mkv"), "a%20b/%C3%A9.mkv");
        assert_eq!(url_unquote("a%20b/%C3%A9.mkv"), "a b/é.mkv");
        assert_eq!(url_unquote("100%+1%2"), "100%+1%2", "invalid escapes are kept");
        assert_eq!(mime_for_path(Path::new("x.MKV")), "video/x-mkv");
        assert_eq!(mime_for_path(Path::new("x.flac")), "audio/x-flac");
        assert_eq!(mime_for_path(Path::new("x")), "video/mp4");
    }

    fn get(port: u16, extra: &str) -> (String, Vec<u8>) {
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(s, "GET /f HTTP/1.1\r\nHost: x\r\n{extra}\r\n").unwrap();
        let mut all = Vec::new();
        s.read_to_end(&mut all).unwrap();
        let pos = all.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        (
            String::from_utf8_lossy(&all[..pos]).to_string(),
            all[pos + 4..].to_vec(),
        )
    }

    #[test]
    fn serves_ranges_on_localhost() {
        let dir = std::env::temp_dir().join(format!("tvremote-srv-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("f.mkv");
        let data: Vec<u8> = (0..5000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&p, &data).unwrap();
        let srv = FileServer::start(
            ServeSource::File(p.clone()),
            "video/x-mkv",
            "f.mkv",
            IpAddr::V4(Ipv4Addr::LOCALHOST),
        )
        .unwrap();
        let (h, b) = get(srv.port(), "");
        assert!(h.starts_with("HTTP/1.0 200"));
        assert!(h.contains("Content-Length: 5000"));
        assert!(h.contains("contentFeatures.dlna.org: DLNA.ORG_OP=01;DLNA.ORG_CI=0"));
        assert_eq!(b, data);
        let (h, b) = get(srv.port(), "Range: bytes=100-199\r\n");
        assert!(h.starts_with("HTTP/1.0 206"));
        assert!(h.contains("Content-Range: bytes 100-199/5000"));
        assert_eq!(b, &data[100..200]);
        let (h, _) = get(srv.port(), "Range: bytes=9000-\r\n");
        assert!(h.starts_with("HTTP/1.0 416"));
        assert_eq!(srv.total_bytes(), 5100);
        assert!(srv.first_byte_at().is_some());
        srv.stop();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn serves_chunked_stream_on_localhost() {
        let starts: Arc<Mutex<Vec<StreamStart>>> = Arc::default();
        let seen = starts.clone();
        let factory: StreamFactory = Arc::new(move |start| {
            seen.lock().unwrap().push(start);
            std::process::Command::new("printf")
                .arg("hello stream")
                .stdout(std::process::Stdio::piped())
                .spawn()
                .map_err(Error::from)
        });
        let srv = FileServer::start(
            ServeSource::Command { factory, length: Some(Duration::from_secs(300)) },
            "video/x-mkv",
            "live.tv.mkv",
            IpAddr::V4(Ipv4Addr::LOCALHOST),
        )
        .unwrap();
        let (h, b) = get(srv.port(), "");
        assert!(h.contains("Transfer-Encoding: chunked"));
        assert!(h.contains("contentFeatures.dlna.org: DLNA.ORG_OP=10;DLNA.ORG_CI=1;DLNA.ORG_FLAGS=0170"));
        assert!(!h.contains("TimeSeekRange"));
        let body = String::from_utf8(b).unwrap();
        assert_eq!(body, "C\r\nhello stream\r\n0\r\n\r\n");
        // The TV seeks natively.
        let (h, _) = get(srv.port(), "TimeSeekRange.dlna.org: npt=60.5-\r\n");
        assert!(h.starts_with("HTTP/1.1 200"));
        assert!(h.contains("TimeSeekRange.dlna.org: npt=0:01:00.500-0:05:00.000/0:05:00.000\r\n"), "{h}");
        assert_eq!(srv.time_seek_count(), 1);
        // Near (or past) the end: served from 10 s before it, and the answer says so.
        let (h, _) = get(srv.port(), "TimeSeekRange.dlna.org: npt=295-\r\n");
        assert!(h.starts_with("HTTP/1.1 200"));
        assert!(h.contains("TimeSeekRange.dlna.org: npt=0:04:50.000-0:05:00.000/0:05:00.000\r\n"), "{h}");
        let (h, _) = get(srv.port(), "TimeSeekRange.dlna.org: npt=0:06:00-\r\n");
        assert!(h.contains("npt=0:04:50.000-"), "{h}");
        let s = starts.lock().unwrap().clone();
        assert_eq!(s[0], StreamStart::default());
        assert_eq!(s[1], StreamStart { at: Duration::from_millis(60_500), ts_offset: Some(Duration::from_millis(60_500)) });
        assert_eq!(s[2], StreamStart { at: Duration::from_secs(290), ts_offset: Some(Duration::from_secs(290)) });
        assert_eq!(s.len(), 4);
        assert!(srv.stream_done(), "printf reached its end");
    }

    #[test]
    fn a_new_request_replaces_the_previous_stream_process() {
        // A long-running fake ffmpeg: says hello, then waits.
        let factory: StreamFactory = Arc::new(|_| {
            std::process::Command::new("sh")
                .args(["-c", "printf hello; exec sleep 30"])
                .stdout(std::process::Stdio::piped())
                .spawn()
                .map_err(Error::from)
        });
        let srv = FileServer::start(
            ServeSource::Command { factory, length: None },
            "video/x-mkv",
            "x.tv.mkv",
            IpAddr::V4(Ipv4Addr::LOCALHOST),
        )
        .unwrap();
        let open = |extra: &str| {
            let mut s = TcpStream::connect(("127.0.0.1", srv.port())).unwrap();
            write!(s, "GET /x.tv.mkv HTTP/1.1\r\nHost: x\r\n{extra}\r\n").unwrap();
            s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            let mut got = Vec::new();
            let mut buf = [0u8; 512];
            while !got.windows(5).any(|w| w == b"hello") {
                let n = s.read(&mut buf).unwrap();
                assert!(n > 0, "stream ended early");
                got.extend_from_slice(&buf[..n]);
            }
            s
        };
        // Still streaming: nothing more but the rest of the first chunk, no end chunk.
        let streaming = |s: &mut TcpStream| {
            s.set_read_timeout(Some(Duration::from_millis(300))).unwrap();
            let mut got = Vec::new();
            let mut b = [0u8; 64];
            loop {
                match s.read(&mut b) {
                    Ok(0) => return false,
                    Ok(n) => got.extend_from_slice(&b[..n]),
                    Err(_) => return !got.windows(5).any(|w| w == b"0\r\n\r\n"),
                }
            }
        };
        let mut first = open("");
        // A sibling request right after (the TV's probe + real fetch): both stay up.
        let mut sibling = open("");
        assert!(streaming(&mut sibling));
        assert!(streaming(&mut first), "the sibling did not stop the first");
        // A later request (a seek) replaces both.
        std::thread::sleep(SIBLING_WINDOW + Duration::from_millis(100));
        let t0 = Instant::now();
        let _seek = open("TimeSeekRange.dlna.org: npt=10-\r\n");
        for mut old in [first, sibling] {
            old.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            let mut rest = Vec::new();
            old.read_to_end(&mut rest).unwrap();
            assert!(rest.ends_with(b"0\r\n\r\n"), "{:?}", String::from_utf8_lossy(&rest));
        }
        assert!(t0.elapsed() < Duration::from_secs(5), "not after the 30 s sleep");
        assert!(srv.last_time_seek().is_some_and(|(_, at)| at == Duration::from_secs(10)));
        assert!(!srv.stream_done(), "killed, not finished");
        srv.stop();
    }

    #[test]
    fn stream_tails_are_never_tiny() {
        let s = Duration::from_secs;
        assert_eq!(keep_tail(s(295), Some(s(300))), s(290), "seen live: npt=295 of 5:00");
        assert_eq!(keep_tail(s(290), Some(s(300))), s(290));
        assert_eq!(keep_tail(s(120), Some(s(300))), s(120));
        assert_eq!(keep_tail(s(400), Some(s(300))), s(290), "past the end");
        assert_eq!(keep_tail(s(5), Some(s(8))), s(0), "a media shorter than the tail");
        assert_eq!(keep_tail(s(295), None), s(295), "unknown length");
        // After an app restart at 4:00 the TV's timeline is 60 s long.
        assert_eq!(keep_tail(s(58), Some(s(60))), s(50));
    }

    #[test]
    fn sibling_requests() {
        assert!(!supersedes(Duration::from_millis(30)), "probe + real fetch");
        assert!(!supersedes(SIBLING_WINDOW - Duration::from_millis(1)));
        assert!(supersedes(SIBLING_WINDOW));
        assert!(supersedes(Duration::from_secs(60)));
    }

    #[test]
    fn time_seek_headers() {
        let s = Duration::from_secs;
        assert_eq!(parse_time_seek("npt=10-"), Some((s(10), None)));
        assert_eq!(parse_time_seek("NPT=10.5-20"), Some((Duration::from_millis(10_500), Some(s(20)))));
        assert_eq!(parse_time_seek("npt=0:01:30.250-"), Some((Duration::from_millis(90_250), None)));
        assert_eq!(parse_time_seek(" npt=00:00:05-00:01:00 "), Some((s(5), Some(s(60)))));
        for bad in ["npt=now-", "npt=-10", "bytes=0-", "npt=10", "npt=20-10", "npt=1e2-", "npt=0:61:00-", "npt=inf-", ""] {
            assert_eq!(parse_time_seek(bad), None, "{bad}");
        }
        assert_eq!(format_npt(Duration::from_millis(3_723_045)), "1:02:03.045");
        assert_eq!(time_seek_response(s(60), Some(s(300))), "npt=0:01:00.000-0:05:00.000/0:05:00.000");
        assert_eq!(time_seek_response(s(60), None), "npt=0:01:00.000-/*");
    }

    #[test]
    fn restart_offsets() {
        let s = Duration::from_secs;
        assert_eq!(start_offset("/film.tv.mkv?t=90.5"), Duration::from_millis(90_500));
        assert_eq!(start_offset("http://192.0.2.1:1/a%20b.tv.mkv?x=1&t=60"), s(60));
        assert_eq!(start_offset("/film.tv.mkv"), Duration::ZERO);
        assert_eq!(start_offset("/f?t=-5"), Duration::ZERO);
        assert_eq!(start_offset("/f?t=nan"), Duration::ZERO);
        // Restarted at 60 s, then the TV seeks natively to 30 s of its own timeline.
        let (st, tv_len) = stream_start(s(60), Some(s(30)), Some(s(300)));
        assert_eq!(st, StreamStart { at: s(90), ts_offset: Some(s(30)) });
        assert_eq!(tv_len, Some(s(240)));
        let (st, _) = stream_start(s(60), None, None);
        assert_eq!(st, StreamStart { at: s(60), ts_offset: None });
    }
}
