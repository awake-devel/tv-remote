//! Minimal HTTP server the TV pulls media from.
//!
//! * [`ServeSource::File`]: one local file with `Range`/206 support so the TV can seek
//!   (port of tv-cast's `RangeHandler`: HTTP/1.0, 1 MiB writes, DLNA headers).
//! * [`ServeSource::Command`]: a live stream from a child process' stdout (ffmpeg),
//!   sent with chunked transfer encoding. Every GET starts a fresh process (the
//!   previous one is killed), so the TV's probe request and its real request both
//!   get the stream from the start. Not seekable.
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
/// `contentFeatures.dlna.org` for live, transcoded, non-seekable streams.
pub const DLNA_FEATURES_STREAM: &str = "DLNA.ORG_OP=00;DLNA.ORG_CI=1";

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

/// Creates the child process for a live stream. Its stdout must be piped.
pub type StreamFactory = Arc<dyn Fn() -> Result<Child> + Send + Sync>;

/// What the server serves.
#[derive(Clone)]
pub enum ServeSource {
    /// A local file, seekable via Range.
    File(PathBuf),
    /// A live stream from a child process' stdout (one process per GET).
    Command(StreamFactory),
}

impl std::fmt::Debug for ServeSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ServeSource::File(p) => f.debug_tuple("File").field(p).finish(),
            ServeSource::Command(_) => f.write_str("Command(..)"),
        }
    }
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
    child: Mutex<Option<(u64, Child)>>,
    generation: AtomicU64,
    log: Mutex<VecDeque<String>>,
}

impl Inner {
    fn kill_child(&self) {
        if let Ok(mut c) = self.child.lock() {
            if let Some((_, mut ch)) = c.take() {
                let _ = ch.kill();
                let _ = ch.wait();
            }
        }
    }

    /// Kill the current child only if it is still generation `gen`.
    fn kill_generation(&self, gen: u64) {
        if let Ok(mut c) = self.child.lock() {
            if c.as_ref().is_some_and(|(g, _)| *g == gen) {
                if let Some((_, mut ch)) = c.take() {
                    let _ = ch.kill();
                    let _ = ch.wait();
                }
            }
        }
    }
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
            child: Mutex::new(None),
            generation: AtomicU64::new(0),
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

    /// True for a live (non-seekable) stream source.
    pub fn is_stream(&self) -> bool {
        matches!(self.inner.source, ServeSource::Command(_))
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
}

fn read_request(stream: &TcpStream) -> Result<Request> {
    let mut r = BufReader::new(stream);
    let mut line = String::new();
    r.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("/").to_string();
    let mut range = None;
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
            }
        }
    }
    Ok(Request {
        method,
        path,
        range,
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
            "{} {} {}",
            req.method,
            req.path,
            req.range.as_deref().unwrap_or("-")
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
        ServeSource::Command(f) => serve_stream(&mut stream, inner, f, head_only),
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
    head_only: bool,
) -> Result<()> {
    let h = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nTransfer-Encoding: chunked\r\nAccept-Ranges: none\r\n\
         transferMode.dlna.org: Streaming\r\ncontentFeatures.dlna.org: {DLNA_FEATURES_STREAM}\r\n\
         Connection: close\r\n\r\n",
        inner.mime
    );
    if head_only {
        // No body for HEAD; don't advertise chunked encoding without one.
        let h = h.replace("Transfer-Encoding: chunked\r\n", "");
        stream.write_all(h.as_bytes())?;
        return Ok(());
    }
    // One producer at a time: kill the previous one.
    inner.kill_child();
    let mut child = factory()?;
    let mut out = child
        .stdout
        .take()
        .ok_or_else(|| Error::Media("stream process has no stdout".into()))?;
    let gen = inner.generation.fetch_add(1, Ordering::SeqCst) + 1;
    if let Ok(mut c) = inner.child.lock() {
        *c = Some((gen, child));
    }
    stream.write_all(h.as_bytes())?;
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        if inner.stop.load(Ordering::Relaxed) {
            break;
        }
        let n = match out.read(&mut buf) {
            Ok(0) => {
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
        let factory: StreamFactory = Arc::new(|| {
            std::process::Command::new("printf")
                .arg("hello stream")
                .stdout(std::process::Stdio::piped())
                .spawn()
                .map_err(Error::from)
        });
        let srv = FileServer::start(
            ServeSource::Command(factory),
            "video/x-mkv",
            "live.mkv",
            IpAddr::V4(Ipv4Addr::LOCALHOST),
        )
        .unwrap();
        let (h, b) = get(srv.port(), "");
        assert!(h.contains("Transfer-Encoding: chunked"));
        let body = String::from_utf8(b).unwrap();
        assert_eq!(body, "C\r\nhello stream\r\n0\r\n\r\n");
    }
}
