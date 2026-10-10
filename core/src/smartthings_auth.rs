//! SmartThings OAuth (authorization-code flow + refresh tokens).
//!
//! Personal access tokens created after 30 Dec 2024 expire after 24 h, so long-lived
//! access needs an OAuth app. The user registers one once with the SmartThings CLI
//! (`apps:create`, template in `docs/smartthings-app.json`) and imports its output file
//! ([`StAuth::import_app`], default [`default_app_file`]). Then:
//!
//! 1. **Login**: open [`StAuth::authorize_url`] in a browser. SmartThings redirects to
//!    [`LOCAL_REDIRECT`], answered by a one-shot [`CallbackListener`] on
//!    `127.0.0.1:47811`, or, when that port is busy, to [`PASTE_REDIRECT`] (httpbin),
//!    whose URL the user pastes back ([`code_from_input`]). The code (valid ~5 min) is
//!    exchanged with [`StAuth::exchange_code`].
//! 2. **Use**: [`StAuth::bearer`] returns a valid access token (24 h), refreshing it
//!    first when it expires within [`REFRESH_MARGIN_SECS`]. Every SmartThings call in
//!    this crate goes through it; on HTTP 401 the client refreshes once and retries once
//!    ([`StAuth::after_unauthorized`]).
//! 3. **Refresh**: refresh tokens last ~30 days and are **single-use**: each refresh
//!    returns a new pair and invalidates the old refresh token and the current access
//!    token. Refreshes are serialized (process mutex + `flock` on the config's lock
//!    file, see [`SharedConfig::with_oauth_locked`]), the config is re-read inside the
//!    lock (another process may have rotated the pair already), and the new pair is
//!    saved before the access token is used. A daily systemd user timer runs
//!    `tv-remote-tui smartthings refresh --quiet` to keep the 30-day window rolling.
//!
//! Token endpoint: `POST https://api.smartthings.com/oauth/token`, form-encoded body,
//! HTTP Basic `client_id:client_secret` (credentials in the body give `invalid_grant`).
//!
//! Errors: [`Error::SmartThingsNotConnected`] (nothing set up),
//! [`Error::SmartThingsLoginExpired`] (`invalid_grant` on refresh: log in again),
//! [`Error::Http`] (network), [`Error::SmartThings`] (anything else).

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::{AppConfig, Config, SharedConfig, StOAuth};
use crate::error::{Error, Result};
use crate::http::{status_and_body, tls_agent};
use crate::smartthings::SmartThings;

/// Authorization endpoint (opened in the browser).
pub const AUTHORIZE_URL: &str = "https://api.smartthings.com/oauth/authorize";
/// Token endpoint (code exchange and refresh).
pub const TOKEN_URL: &str = "https://api.smartthings.com/oauth/token";
/// Port of the one-shot local callback listener.
pub const CALLBACK_PORT: u16 = 47811;
/// Redirect URI answered by [`CallbackListener`] (registered with the app).
pub const LOCAL_REDIRECT: &str = "http://localhost:47811/callback";
/// Fallback redirect URI (registered with the app): the user pastes the URL it shows.
pub const PASTE_REDIRECT: &str = "https://httpbin.org/get";
/// Scopes requested at login (those registered with the app).
pub const SCOPES: &str = "r:devices:* w:devices:* x:devices:* r:locations:*";
/// [`StAuth::bearer`] refreshes when the access token expires within this many seconds.
pub const REFRESH_MARGIN_SECS: u64 = 600;
/// Nominal refresh-token lifetime (30 days); some die earlier, hence the daily refresh.
pub const REFRESH_TOKEN_LIFETIME_SECS: u64 = 30 * 24 * 3600;
/// How long the login waits for the browser redirect.
pub const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);
/// Access-token lifetime assumed when the response has no `expires_in`.
const DEFAULT_EXPIRES_IN: u64 = 86_399;

fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// `~/.config/samsung-tv/st-app-created.json`: where the setup instructions save the
/// output of `smartthings apps:create … -o`.
pub fn default_app_file() -> PathBuf {
    Config::default_path().with_file_name("st-app-created.json")
}

// ---------------------------------------------------------------------------------------
// Pure helpers: encoding, request builders, response parsing

/// Percent-encode for a query string or a form body (RFC 3986 unreserved kept). Pure.
pub fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Decode `%XX` and `+` (form encoding). Invalid escapes are kept as-is. Pure.
pub fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 3 <= b.len() => {
                match u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("zz"), 16) {
                    Ok(v) => {
                        out.push(v);
                        i += 2;
                    }
                    Err(_) => out.push(b'%'),
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Parse `a=1&b=x%20y` into decoded pairs. Pure.
pub fn parse_query(q: &str) -> Vec<(String, String)> {
    q.split('&')
        .filter(|p| !p.is_empty())
        .map(|p| match p.split_once('=') {
            Some((k, v)) => (percent_decode(k), percent_decode(v)),
            None => (percent_decode(p), String::new()),
        })
        .collect()
}

fn query_get<'a>(pairs: &'a [(String, String)], key: &str) -> Option<&'a str> {
    pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

/// The authorization URL for `client_id`. Pure.
pub fn build_authorize_url(client_id: &str, redirect: &str, state: &str) -> String {
    format!(
        "{AUTHORIZE_URL}?client_id={}&response_type=code&redirect_uri={}&scope={}&state={}",
        percent_encode(client_id),
        percent_encode(redirect),
        percent_encode(SCOPES),
        percent_encode(state)
    )
}

/// A token-endpoint request: `POST url`, `Authorization: <authorization>`,
/// `Content-Type: application/x-www-form-urlencoded`, `body`. `Debug` redacts it.
#[derive(Clone, PartialEq, Eq)]
pub struct TokenRequest {
    /// Always [`TOKEN_URL`].
    pub url: String,
    /// `Basic base64(client_id:client_secret)`.
    pub authorization: String,
    /// Form-encoded body.
    pub body: String,
}

impl std::fmt::Debug for TokenRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let grant = parse_query(&self.body).into_iter().find(|(k, _)| k == "grant_type").map(|(_, v)| v);
        f.debug_struct("TokenRequest")
            .field("url", &self.url)
            .field("grant_type", &grant)
            .field("authorization", &"<redacted>")
            .finish()
    }
}

fn basic_auth(app: &StOAuth) -> String {
    let raw = format!("{}:{}", app.client_id.trim(), app.client_secret.trim());
    format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(raw.as_bytes()))
}

fn form(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// The code-exchange request. Pure.
pub fn exchange_request(app: &StOAuth, code: &str, redirect: &str) -> TokenRequest {
    TokenRequest {
        url: TOKEN_URL.into(),
        authorization: basic_auth(app),
        body: form(&[
            ("grant_type", "authorization_code"),
            ("client_id", app.client_id.trim()),
            ("code", code.trim()),
            ("redirect_uri", redirect),
        ]),
    }
}

/// The refresh request. Pure.
pub fn refresh_request(app: &StOAuth, refresh_token: &str) -> TokenRequest {
    TokenRequest {
        url: TOKEN_URL.into(),
        authorization: basic_auth(app),
        body: form(&[
            ("grant_type", "refresh_token"),
            ("client_id", app.client_id.trim()),
            ("refresh_token", refresh_token.trim()),
        ]),
    }
}

/// A successful token response. `Debug` redacts the tokens.
#[derive(Clone, PartialEq, Eq)]
pub struct TokenResponse {
    /// New access token.
    pub access_token: String,
    /// New refresh token (always sent by SmartThings; `None` keeps the old one).
    pub refresh_token: Option<String>,
    /// Lifetime of the access token in seconds (~86399).
    pub expires_in: Option<u64>,
    /// Granted scopes.
    pub scope: Option<String>,
}

impl std::fmt::Debug for TokenResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenResponse")
            .field("access_token", &"<redacted>")
            .field("refresh_token", &self.refresh_token.as_ref().map(|_| "<redacted>"))
            .field("expires_in", &self.expires_in)
            .field("scope", &self.scope)
            .finish()
    }
}

/// Parse a token-endpoint answer. `invalid_grant` → [`Error::SmartThingsLoginExpired`];
/// `invalid_client` → [`Error::SmartThings`] (re-import the app file); 5xx →
/// [`Error::Http`]. Pure.
pub fn parse_token_response(status: u16, body: &str) -> Result<TokenResponse> {
    let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    if (200..300).contains(&status) {
        let access = v["access_token"].as_str().filter(|s| !s.is_empty()).ok_or_else(|| {
            Error::SmartThings("the token response has no access_token".into())
        })?;
        return Ok(TokenResponse {
            access_token: access.to_string(),
            refresh_token: v["refresh_token"].as_str().filter(|s| !s.is_empty()).map(str::to_string),
            expires_in: v["expires_in"].as_u64().or_else(|| v["expires_in"].as_str().and_then(|s| s.parse().ok())),
            scope: v["scope"].as_str().map(str::to_string),
        });
    }
    let err = v["error"].as_str().unwrap_or("");
    let desc = v["error_description"].as_str().unwrap_or("");
    match err {
        "invalid_grant" => Err(Error::SmartThingsLoginExpired),
        "invalid_client" | "unauthorized_client" => Err(Error::SmartThings(format!(
            "the OAuth app credentials were rejected ({err}): import the app file again"
        ))),
        _ if status >= 500 => Err(Error::Http(format!("SmartThings token endpoint: HTTP {status}"))),
        "" => Err(Error::SmartThings(format!("token request failed: HTTP {status}"))),
        _ => Err(Error::SmartThings(format!(
            "token request failed: {err}{}",
            if desc.is_empty() { String::new() } else { format!(" ({desc})") }
        ))),
    }
}

/// Store a token response (the access token, its expiry, the rotated refresh token and
/// `refreshed_at`). Pure.
pub fn apply_token_response(o: &mut StOAuth, r: &TokenResponse, now: u64) {
    o.access_token = Some(r.access_token.clone());
    o.access_expires_at = Some(now + r.expires_in.unwrap_or(DEFAULT_EXPIRES_IN));
    if let Some(rt) = &r.refresh_token {
        o.refresh_token = Some(rt.clone());
        o.refreshed_at = Some(now);
    }
    if r.scope.is_some() {
        o.scope = r.scope.clone();
    }
}

/// True if the access token is missing or expires within [`REFRESH_MARGIN_SECS`]. Pure.
pub fn needs_refresh(o: &StOAuth, now: u64) -> bool {
    let has = o.access_token.as_deref().is_some_and(|t| !t.is_empty());
    !has || o.access_expires_at.is_none_or(|e| e <= now + REFRESH_MARGIN_SECS)
}

/// What the SmartThings CLI wrote with `apps:create -o <file> -j`. `Debug` redacts
/// the secret.
#[derive(Clone, PartialEq, Eq)]
pub struct ImportedApp {
    /// `app.appName`.
    pub app_name: Option<String>,
    /// `app.appId`.
    pub app_id: Option<String>,
    /// `oauthClientId`.
    pub client_id: String,
    /// `oauthClientSecret`. Never log this.
    pub client_secret: String,
    /// Redirect URIs found in the file (if the CLI included them).
    pub redirect_uris: Vec<String>,
}

impl std::fmt::Debug for ImportedApp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImportedApp")
            .field("app_name", &self.app_name)
            .field("app_id", &self.app_id)
            .field("client_secret", &"<redacted>")
            .field("redirect_uris", &self.redirect_uris)
            .finish()
    }
}

/// Parse the `apps:create` output (`{"app": {"appName", "appId", …}, "oauthClientId",
/// "oauthClientSecret"}`). Pure.
pub fn parse_app_file(v: &Value) -> Result<ImportedApp> {
    let s = |x: &Value| x.as_str().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
    let client_id = s(&v["oauthClientId"]).ok_or_else(|| {
        Error::Config("not a SmartThings `apps:create` output file: no oauthClientId".into())
    })?;
    let client_secret = s(&v["oauthClientSecret"])
        .ok_or_else(|| Error::Config("the app file has no oauthClientSecret".into()))?;
    let mut redirect_uris = Vec::new();
    collect_redirects(v, &mut redirect_uris);
    Ok(ImportedApp {
        app_name: s(&v["app"]["appName"]),
        app_id: s(&v["app"]["appId"]),
        client_id,
        client_secret,
        redirect_uris,
    })
}

fn collect_redirects(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::Object(o) => {
            for (k, val) in o {
                if k.eq_ignore_ascii_case("redirectUris") {
                    for u in val.as_array().into_iter().flatten().filter_map(Value::as_str) {
                        if !out.iter().any(|x| x == u) {
                            out.push(u.to_string());
                        }
                    }
                } else {
                    collect_redirects(val, out);
                }
            }
        }
        Value::Array(a) => a.iter().for_each(|x| collect_redirects(x, out)),
        _ => {}
    }
}

/// The authorization code from what the user pasted: the full redirected URL
/// (`https://httpbin.org/get?code=…&state=…`), the JSON httpbin shows (`"args":
/// {"code": …}`), a bare query string, or just the code. If both `expected_state` and a
/// state in the input are present they must match. Pure.
pub fn code_from_input(input: &str, expected_state: Option<&str>) -> Result<String> {
    let t = input.trim();
    if t.is_empty() {
        return Err(Error::parse("nothing pasted"));
    }
    let (code, state, error) = if t.starts_with('{') {
        let v: Value = serde_json::from_str(t).map_err(|e| Error::parse(format!("pasted JSON: {e}")))?;
        let args = if v["args"].is_object() { &v["args"] } else { &v };
        let g = |k: &str| args[k].as_str().map(str::to_string);
        (g("code"), g("state"), g("error"))
    } else if t.contains("code=") || t.contains("error=") {
        let q = t.split_once('?').map(|(_, q)| q).unwrap_or(t);
        let q = q.split('#').next().unwrap_or(q);
        let pairs = parse_query(q);
        let g = |k: &str| query_get(&pairs, k).map(str::to_string);
        (g("code"), g("state"), g("error"))
    } else if t.chars().all(|c| c.is_ascii_alphanumeric() || "-._~".contains(c)) {
        (Some(t.to_string()), None, None)
    } else {
        return Err(Error::parse("paste the whole redirected URL (with ?code=…) or just the code"));
    };
    if let Some(e) = error {
        return Err(Error::SmartThings(format!("the login was not approved ({e})")));
    }
    if let (Some(exp), Some(got)) = (expected_state, state.as_deref()) {
        if exp != got {
            return Err(Error::SmartThings(
                "the pasted URL belongs to another login attempt (state mismatch): start again".into(),
            ));
        }
    }
    code.filter(|c| !c.trim().is_empty())
        .map(|c| c.trim().to_string())
        .ok_or_else(|| Error::parse("no code in the pasted text"))
}

/// What a request to the callback listener was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Callback {
    /// `GET /callback?code=…&state=<expected>`.
    Code(String),
    /// `GET /callback?error=…&state=<expected>` (the user refused).
    Denied(String),
    /// `GET /callback` with a missing or wrong `state`.
    BadState,
    /// Anything else (other path or method, e.g. `/favicon.ico`).
    Other,
}

/// Classify the request head received by the callback listener. Pure.
pub fn parse_callback_request(head: &str, expected_state: &str) -> Callback {
    let line = head.lines().next().unwrap_or("");
    let mut parts = line.split_whitespace();
    let (Some(method), Some(target)) = (parts.next(), parts.next()) else { return Callback::Other };
    if method != "GET" {
        return Callback::Other;
    }
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    if path != "/callback" {
        return Callback::Other;
    }
    let pairs = parse_query(query);
    if query_get(&pairs, "state") != Some(expected_state) {
        return Callback::BadState;
    }
    if let Some(code) = query_get(&pairs, "code").filter(|c| !c.is_empty()) {
        return Callback::Code(code.to_string());
    }
    Callback::Denied(query_get(&pairs, "error").unwrap_or("no code").to_string())
}

/// A random `state` value (128 bits from `/dev/urandom`, hex).
pub fn random_state() -> String {
    let mut buf = [0u8; 16];
    let ok = std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut buf)).is_ok();
    if !ok {
        // Not secret-grade, but the state only guards against stale/forged redirects.
        let seed = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)
            ^ ((std::process::id() as u128) << 64);
        buf = seed.to_le_bytes();
    }
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// Open `url` in the default browser with `xdg-open` (detached). False if it could not
/// be started: print or show the URL instead.
pub fn open_in_browser(url: &str) -> bool {
    std::process::Command::new("xdg-open")
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|mut child| {
            // Reap it in the background so it does not linger as a zombie.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        })
        .is_ok()
}

// ---------------------------------------------------------------------------------------
// One-shot local callback listener

const PAGE_OK: &str = "<!doctype html><meta charset=utf-8><title>TV Remote</title>\
<body style=\"font-family:sans-serif;margin:3em\"><h2>SmartThings is connected.</h2>\
<p>You can close this tab and go back to TV Remote.</p>";
const PAGE_DENIED: &str = "<!doctype html><meta charset=utf-8><title>TV Remote</title>\
<body style=\"font-family:sans-serif;margin:3em\"><h2>The login was not approved.</h2>\
<p>You can close this tab.</p>";
const PAGE_BAD_STATE: &str = "<!doctype html><meta charset=utf-8><title>TV Remote</title>\
<body style=\"font-family:sans-serif;margin:3em\"><h2>This link belongs to another login attempt.</h2>\
<p>Start the login again from TV Remote.</p>";

/// A one-shot HTTP listener on `127.0.0.1:47811` that accepts only `GET /callback`.
#[derive(Debug)]
pub struct CallbackListener {
    listener: TcpListener,
    port: u16,
}

impl CallbackListener {
    /// Bind `127.0.0.1:47811`. Fails (`Error::Io`, `AddrInUse`) if the port is busy:
    /// fall back to the paste flow ([`PASTE_REDIRECT`]).
    pub fn bind() -> Result<CallbackListener> {
        Self::bind_port(CALLBACK_PORT)
    }

    /// Bind `127.0.0.1:port` (0 = any free port, for tests).
    pub fn bind_port(port: u16) -> Result<CallbackListener> {
        let listener = TcpListener::bind(("127.0.0.1", port))?;
        let port = listener.local_addr()?.port();
        listener.set_nonblocking(true)?;
        Ok(CallbackListener { listener, port })
    }

    /// The port it listens on.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// The redirect URI that reaches this listener ([`LOCAL_REDIRECT`] on the default port).
    pub fn redirect_uri(&self) -> String {
        format!("http://localhost:{}/callback", self.port)
    }

    /// Wait for `GET /callback?code=…&state=<state>` and return the code; answers it with
    /// a "You can close this tab" page. Other requests get a 404, a wrong `state` a 400
    /// (and the wait goes on). Gives up after `timeout` ([`Error::Timeout`]) or when
    /// `cancel` is set ([`Error::Cancelled`]).
    pub fn wait_for_code(&self, state: &str, timeout: Duration, cancel: &AtomicBool) -> Result<String> {
        let deadline = Instant::now() + timeout;
        loop {
            if cancel.load(Ordering::SeqCst) {
                return Err(Error::Cancelled);
            }
            if Instant::now() >= deadline {
                return Err(Error::Timeout("no answer from the browser within 5 minutes".into()));
            }
            match self.listener.accept() {
                Ok((stream, _)) => {
                    if let Some(res) = Self::serve(stream, state) {
                        return res;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(100)),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// Answer one connection; `Some` ends the wait.
    fn serve(mut stream: TcpStream, state: &str) -> Option<Result<String>> {
        let _ = stream.set_nonblocking(false);
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        let mut head = Vec::new();
        let mut buf = [0u8; 1024];
        while head.len() < 16 * 1024 && !head.windows(4).any(|w| w == b"\r\n\r\n") {
            match stream.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => head.extend_from_slice(&buf[..n]),
            }
        }
        let head = String::from_utf8_lossy(&head);
        let (status, page, result) = match parse_callback_request(&head, state) {
            Callback::Code(c) => ("200 OK", PAGE_OK, Some(Ok(c))),
            Callback::Denied(e) => (
                "200 OK",
                PAGE_DENIED,
                Some(Err(Error::SmartThings(format!("the login was not approved ({e})")))),
            ),
            Callback::BadState => ("400 Bad Request", PAGE_BAD_STATE, None),
            Callback::Other => ("404 Not Found", "not found", None),
        };
        let resp = format!(
            "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\n\
             Cache-Control: no-store\r\nConnection: close\r\n\r\n{page}",
            page.len()
        );
        let _ = stream.write_all(resp.as_bytes());
        let _ = stream.flush();
        result
    }
}

// ---------------------------------------------------------------------------------------
// Status

/// Which credential SmartThings calls use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuthMethod {
    /// OAuth access token (refreshed automatically).
    OAuth,
    /// Legacy personal access token.
    Pat,
    /// Nothing usable.
    None,
}

/// Snapshot of the SmartThings authorization state (no secrets).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthStatus {
    /// What SmartThings calls use now.
    pub method: AuthMethod,
    /// An OAuth app was imported (client id + secret present).
    pub app_imported: bool,
    /// SmartThings app id of the imported app.
    pub app_id: Option<String>,
    /// OAuth connected (a refresh token is stored).
    pub connected: bool,
    /// The user must log in again: the last refresh was rejected, or the refresh token
    /// is older than 30 days.
    pub needs_login: bool,
    /// Seconds until the access token expires (negative: expired).
    pub access_expires_in: Option<i64>,
    /// Age of the refresh token in seconds (since login or the last refresh).
    pub refresh_age: Option<u64>,
    /// A personal access token is configured (legacy fallback).
    pub pat_present: bool,
    /// Granted scopes.
    pub scope: Option<String>,
}

impl AuthStatus {
    /// From a config snapshot. Pure.
    pub fn from_config(app: &AppConfig, now: u64) -> AuthStatus {
        let o = app.smartthings_oauth.as_ref();
        let connected = o.is_some_and(StOAuth::is_connected);
        let pat_present = app.smartthings_token.as_deref().is_some_and(|t| !t.trim().is_empty());
        let refresh_age = o.and_then(|o| o.refreshed_at).map(|t| now.saturating_sub(t));
        let expired_login = o.is_some_and(|o| o.has_app() && !o.is_connected() && o.refreshed_at.is_some());
        let too_old = connected && refresh_age.is_some_and(|a| a >= REFRESH_TOKEN_LIFETIME_SECS);
        AuthStatus {
            method: if connected {
                AuthMethod::OAuth
            } else if pat_present {
                AuthMethod::Pat
            } else {
                AuthMethod::None
            },
            app_imported: o.is_some_and(StOAuth::has_app),
            app_id: o.and_then(|o| o.app_id.clone()),
            connected,
            needs_login: expired_login || too_old,
            access_expires_in: o
                .filter(|o| o.access_token.is_some())
                .and_then(|o| o.access_expires_at)
                .map(|e| e as i64 - now as i64),
            refresh_age: if connected { refresh_age } else { None },
            pat_present,
            scope: o.and_then(|o| o.scope.clone()),
        }
    }

    /// The access token expires within [`REFRESH_MARGIN_SECS`] (or is missing).
    pub fn expiring_soon(&self) -> bool {
        self.connected && self.access_expires_in.is_none_or(|s| s <= REFRESH_MARGIN_SECS as i64)
    }

    /// One line for a status row: "Connected via OAuth", "Needs login", …
    pub fn summary(&self) -> String {
        let hours = |s: i64| -> String {
            if s >= 3600 {
                format!("{} h", s / 3600)
            } else {
                format!("{} min", (s / 60).max(0))
            }
        };
        if self.needs_login {
            return if self.pat_present {
                "Needs login (OAuth expired; using the personal access token meanwhile)".into()
            } else {
                "Needs login (the SmartThings login expired)".into()
            };
        }
        match self.method {
            AuthMethod::OAuth => match self.access_expires_in {
                Some(s) if s > REFRESH_MARGIN_SECS as i64 => format!("Connected via OAuth (token valid {})", hours(s)),
                _ => "Connected via OAuth (token expiring soon: refreshed on next use)".into(),
            },
            AuthMethod::Pat if self.app_imported => "Using a personal access token (app imported: connect to switch to OAuth)".into(),
            AuthMethod::Pat => "Using a personal access token (expires 24 h after creation)".into(),
            AuthMethod::None if self.app_imported => "App imported: not connected yet".into(),
            AuthMethod::None => "Not set up".into(),
        }
    }
}

// ---------------------------------------------------------------------------------------
// The client

/// Sends a token request and returns (HTTP status, body). Replaceable for tests.
pub type Transport = Arc<dyn Fn(&TokenRequest) -> Result<(u16, String)> + Send + Sync>;

fn http_transport() -> Transport {
    Arc::new(|req: &TokenRequest| {
        let agent = tls_agent(Duration::from_secs(15))?;
        status_and_body(
            agent
                .post(&req.url)
                .set("Authorization", &req.authorization)
                .set("Content-Type", "application/x-www-form-urlencoded")
                .set("Accept", "application/json")
                .send_string(&req.body),
        )
    })
}

/// The account's SmartThings OAuth state, bound to the shared config (and its file).
/// Cheap to clone; `Send + Sync`. Every method that touches tokens is serialized across
/// threads and processes (see the module docs).
#[derive(Clone)]
pub struct StAuth {
    config: SharedConfig,
    transport: Transport,
}

impl std::fmt::Debug for StAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StAuth").field("path", &self.config.path()).finish()
    }
}

impl StAuth {
    /// Bound to `config` (its scope is ignored: OAuth is account-level). No I/O.
    pub fn new(config: &SharedConfig) -> StAuth {
        Self::with_transport(config, http_transport())
    }

    /// Same with a custom token transport (tests).
    pub fn with_transport(config: &SharedConfig, transport: Transport) -> StAuth {
        StAuth { config: config.unscoped(), transport }
    }

    /// The OAuth section as the config file has it now (secrets included: never print).
    pub fn oauth(&self) -> Option<StOAuth> {
        self.config.oauth_from_disk()
    }

    /// Current status (reads the config file).
    pub fn status(&self) -> AuthStatus {
        let _ = self.config.oauth_from_disk();
        AuthStatus::from_config(&self.config.app(), now_unix())
    }

    /// True if the OAuth login is connected.
    pub fn is_connected(&self) -> bool {
        self.oauth().is_some_and(|o| o.is_connected())
    }

    /// Read the `apps:create` output file and store the client id / secret / app id.
    /// Importing another app (other client id) forgets the tokens of the previous one.
    pub fn import_app(&self, created_json: &Path) -> Result<ImportedApp> {
        let text = std::fs::read_to_string(created_json)
            .map_err(|e| Error::Config(format!("cannot read {}: {e}", created_json.display())))?;
        let v: Value = serde_json::from_str(&text)
            .map_err(|e| Error::Config(format!("{} is not JSON: {e}", created_json.display())))?;
        let app = parse_app_file(&v)?;
        let imported = app.clone();
        self.config.with_oauth_locked(move |cur| {
            let mut o = match cur.take() {
                Some(o) if o.client_id == app.client_id => o,
                _ => StOAuth::default(),
            };
            o.client_id = app.client_id;
            o.client_secret = app.client_secret;
            if app.app_id.is_some() {
                o.app_id = app.app_id;
            }
            if o.redirect_uri.is_none() {
                o.redirect_uri = Some(LOCAL_REDIRECT.into());
            }
            *cur = Some(o);
            Ok(())
        })?;
        Ok(imported)
    }

    fn app_or_err(o: &Option<StOAuth>) -> Result<&StOAuth> {
        o.as_ref().filter(|o| o.has_app()).ok_or_else(|| {
            Error::SmartThings("no SmartThings OAuth app imported (run `tv-remote-tui smartthings app-import`)".into())
        })
    }

    /// The URL to open in the browser for `state` and `redirect` ([`LOCAL_REDIRECT`] or
    /// [`PASTE_REDIRECT`]).
    pub fn authorize_url(&self, state: &str, redirect: &str) -> Result<String> {
        let o = self.oauth();
        Ok(build_authorize_url(&Self::app_or_err(&o)?.client_id, redirect, state))
    }

    /// Exchange an authorization code (from the redirect to `redirect`, which must be the
    /// one in the authorize URL) for a token pair, saved at once.
    pub fn exchange_code(&self, code: &str, redirect: &str) -> Result<()> {
        let code = code.trim();
        if code.is_empty() {
            return Err(Error::parse("empty authorization code"));
        }
        self.config.with_oauth_locked(|cur| {
            let req = exchange_request(Self::app_or_err(cur)?, code, redirect);
            let (status, body) = (self.transport)(&req)?;
            let resp = parse_token_response(status, &body).map_err(|e| match e {
                Error::SmartThingsLoginExpired => Error::SmartThings(
                    "the authorization code was rejected (it expires after ~5 minutes and works once): log in again"
                        .into(),
                ),
                e => e,
            })?;
            if let Some(o) = cur.as_mut() {
                apply_token_response(o, &resp, now_unix());
                o.redirect_uri = Some(redirect.to_string());
                if resp.refresh_token.is_none() {
                    o.refreshed_at = Some(now_unix());
                }
            }
            Ok(())
        })
    }

    /// Spend the refresh token now (whatever the access token's age) and save the new
    /// pair. A rejected refresh token (`invalid_grant`) clears the tokens and returns
    /// [`Error::SmartThingsLoginExpired`].
    pub fn refresh(&self) -> Result<()> {
        self.config.with_oauth_locked(|cur| self.refresh_locked(cur).map(|_| ()))
    }

    /// Refresh inside the lock; returns the new access token.
    fn refresh_locked(&self, cur: &mut Option<StOAuth>) -> Result<String> {
        let o = match cur.as_mut() {
            Some(o) if o.is_connected() => o,
            Some(o) if o.has_app() && o.refreshed_at.is_some() => return Err(Error::SmartThingsLoginExpired),
            _ => return Err(Error::SmartThingsNotConnected),
        };
        let rt = o.refresh_token.clone().unwrap_or_default();
        let req = refresh_request(o, &rt);
        let (status, body) = (self.transport)(&req)?;
        match parse_token_response(status, &body) {
            Ok(resp) => {
                apply_token_response(o, &resp, now_unix());
                Ok(resp.access_token)
            }
            Err(Error::SmartThingsLoginExpired) => {
                o.clear_tokens(true);
                Err(Error::SmartThingsLoginExpired)
            }
            Err(e) => Err(e),
        }
    }

    /// A valid access token, refreshed first (serialized, saved before it is returned)
    /// if it expires within [`REFRESH_MARGIN_SECS`].
    pub fn bearer(&self) -> Result<String> {
        let now = now_unix();
        if let Some(o) = self.config.app().smartthings_oauth.filter(|o| o.is_connected() && !needs_refresh(o, now)) {
            if let Some(t) = o.access_token {
                return Ok(t);
            }
        }
        self.config.with_oauth_locked(|cur| {
            // Re-read inside the lock: another thread or process may have refreshed.
            match cur.as_ref() {
                Some(o) if o.is_connected() && !needs_refresh(o, now_unix()) => {
                    Ok(o.access_token.clone().unwrap_or_default())
                }
                _ => self.refresh_locked(cur),
            }
        })
    }

    /// After HTTP 401 with `used` as the bearer: a token another process stored since
    /// (the refresh it made invalidated `used`), else a fresh one from a refresh.
    pub fn after_unauthorized(&self, used: &str) -> Result<String> {
        self.config.with_oauth_locked(|cur| match cur.as_ref() {
            Some(o)
                if o.is_connected()
                    && o.access_token.as_deref().is_some_and(|t| t != used)
                    && !needs_refresh(o, now_unix()) =>
            {
                Ok(o.access_token.clone().unwrap_or_default())
            }
            _ => self.refresh_locked(cur),
        })
    }

    /// Forget the tokens (keep the app credentials, so `login` works again).
    pub fn logout(&self) -> Result<()> {
        self.config.with_oauth_locked(|cur| {
            if let Some(o) = cur.as_mut() {
                o.clear_tokens(false);
            }
            Ok(())
        })
    }

    /// Login, local flow: `listener` gets the redirect (state checked), the code is
    /// exchanged. Open [`StAuth::authorize_url`]`(state, &listener.redirect_uri())` first.
    pub fn login_with_listener(
        &self,
        listener: &CallbackListener,
        state: &str,
        timeout: Duration,
        cancel: &AtomicBool,
    ) -> Result<()> {
        let code = listener.wait_for_code(state, timeout, cancel)?;
        self.exchange_code(&code, &listener.redirect_uri())
    }

    /// Login, paste flow: `input` is the redirected URL (or the code) for an authorize
    /// URL built with `state` and `redirect`.
    pub fn login_with_input(&self, input: &str, state: Option<&str>, redirect: &str) -> Result<()> {
        let code = code_from_input(input, state)?;
        self.exchange_code(&code, redirect)
    }
}

/// The SmartThings client for this config: OAuth if connected (it wins), else the
/// personal access token, else [`Error::SmartThingsLoginExpired`] (an OAuth login that
/// expired) or [`Error::SmartThingsNotConnected`].
pub fn client_for(config: &SharedConfig) -> Result<SmartThings> {
    let oauth = config.oauth_from_disk();
    if oauth.as_ref().is_some_and(StOAuth::is_connected) {
        return SmartThings::with_oauth(StAuth::new(config));
    }
    if let Some(pat) = config.app().smartthings_token.filter(|t| !t.trim().is_empty()) {
        return SmartThings::new(&pat);
    }
    match oauth {
        Some(o) if o.has_app() && o.refreshed_at.is_some() => Err(Error::SmartThingsLoginExpired),
        _ => Err(Error::SmartThingsNotConnected),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn app() -> StOAuth {
        StOAuth {
            client_id: "fake-client".into(),
            client_secret: "fake-secret".into(),
            ..Default::default()
        }
    }

    fn temp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tvremote-auth-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn encoding_round_trips() {
        assert_eq!(percent_encode("r:devices:* x"), "r%3Adevices%3A%2A%20x");
        assert_eq!(percent_decode("a%3Ab+c%2"), "a:b c%2");
        assert_eq!(percent_decode("%zz%41"), "%zzA");
        assert_eq!(parse_query("code=a%2Fb&state=s&x"), vec![
            ("code".into(), "a/b".into()),
            ("state".into(), "s".into()),
            ("x".into(), String::new())
        ]);
    }

    #[test]
    fn authorize_url_is_well_formed() {
        let u = build_authorize_url("fake-client", LOCAL_REDIRECT, "st8");
        assert!(u.starts_with("https://api.smartthings.com/oauth/authorize?client_id=fake-client&response_type=code&"));
        assert!(u.contains("redirect_uri=http%3A%2F%2Flocalhost%3A47811%2Fcallback"));
        assert!(u.contains("scope=r%3Adevices%3A%2A%20w%3Adevices%3A%2A%20x%3Adevices%3A%2A%20r%3Alocations%3A%2A"));
        assert!(u.ends_with("&state=st8"));
    }

    #[test]
    fn token_requests_use_basic_auth_and_form_bodies() {
        let r = exchange_request(&app(), " the-code ", LOCAL_REDIRECT);
        assert_eq!(r.url, TOKEN_URL);
        // base64("fake-client:fake-secret")
        assert_eq!(r.authorization, "Basic ZmFrZS1jbGllbnQ6ZmFrZS1zZWNyZXQ=");
        assert_eq!(
            r.body,
            "grant_type=authorization_code&client_id=fake-client&code=the-code&redirect_uri=http%3A%2F%2Flocalhost%3A47811%2Fcallback"
        );
        assert!(!r.body.contains("secret"), "the secret only goes in the Basic header");
        let r = refresh_request(&app(), "rt/1");
        assert_eq!(r.body, "grant_type=refresh_token&client_id=fake-client&refresh_token=rt%2F1");
        let dbg = format!("{r:?}");
        assert!(dbg.contains("refresh_token") && !dbg.contains("rt%2F1") && !dbg.contains("ZmFr"));
    }

    #[test]
    fn token_responses() {
        let ok = parse_token_response(
            200,
            r#"{"access_token":"a1","refresh_token":"r1","expires_in":86399,"scope":"r:devices:*","token_type":"bearer"}"#,
        )
        .unwrap();
        assert_eq!((ok.access_token.as_str(), ok.refresh_token.as_deref(), ok.expires_in), ("a1", Some("r1"), Some(86399)));
        assert!(!format!("{ok:?}").contains("a1"));
        let mut o = app();
        apply_token_response(&mut o, &ok, 1000);
        assert_eq!(o.access_expires_at, Some(87399));
        assert_eq!(o.refreshed_at, Some(1000));
        assert!(o.is_connected());
        assert!(!needs_refresh(&o, 1000));
        assert!(!needs_refresh(&o, 87399 - 601));
        assert!(needs_refresh(&o, 87399 - 600), "10 minutes before expiry");
        assert!(needs_refresh(&app(), 0), "no token");

        assert!(matches!(
            parse_token_response(400, r#"{"error":"invalid_grant","error_description":"Invalid refresh token"}"#),
            Err(Error::SmartThingsLoginExpired)
        ));
        assert!(matches!(parse_token_response(401, r#"{"error":"invalid_client"}"#), Err(Error::SmartThings(m)) if m.contains("import")));
        assert!(matches!(parse_token_response(503, "oops"), Err(Error::Http(_))));
        assert!(matches!(parse_token_response(400, r#"{"error":"invalid_scope"}"#), Err(Error::SmartThings(m)) if m.contains("invalid_scope")));
        assert!(parse_token_response(200, "{}").is_err());
    }

    #[test]
    fn app_file_parsing() {
        let v = serde_json::json!({
            "app": {"appName": "tv-remote-someone", "appId": "app-123", "appType": "API_ONLY",
                    "oauth": {"redirectUris": ["http://localhost:47811/callback", "https://httpbin.org/get"]}},
            "oauthClientId": "fake-client", "oauthClientSecret": "fake-secret"
        });
        let a = parse_app_file(&v).unwrap();
        assert_eq!(a.app_id.as_deref(), Some("app-123"));
        assert_eq!(a.client_id, "fake-client");
        assert_eq!(a.redirect_uris.len(), 2);
        assert!(!format!("{a:?}").contains("fake-secret"));
        assert!(parse_app_file(&serde_json::json!({"app": {}})).is_err());
        assert!(parse_app_file(&serde_json::json!({"oauthClientId": "x"})).is_err());
    }

    #[test]
    fn pasted_input() {
        let s = Some("st8");
        assert_eq!(code_from_input("https://httpbin.org/get?code=AbC&state=st8", s).unwrap(), "AbC");
        assert_eq!(code_from_input("  http://localhost:47811/callback?state=st8&code=x%2Dy  ", s).unwrap(), "x-y");
        assert_eq!(code_from_input("AbC123", s).unwrap(), "AbC123");
        assert_eq!(code_from_input(r#"{"args": {"code": "J", "state": "st8"}, "url": "…"}"#, s).unwrap(), "J");
        assert!(code_from_input("https://httpbin.org/get?code=AbC&state=other", s).is_err());
        assert!(code_from_input("https://httpbin.org/get?error=access_denied&state=st8", s).is_err());
        assert!(code_from_input("", s).is_err());
        assert!(code_from_input("not a code at all", s).is_err());
        assert_eq!(code_from_input("code=Q", None).unwrap(), "Q");
    }

    #[test]
    fn callback_requests() {
        let get = |t: &str| format!("GET {t} HTTP/1.1\r\nHost: localhost:47811\r\n\r\n");
        assert_eq!(parse_callback_request(&get("/callback?code=C1&state=s"), "s"), Callback::Code("C1".into()));
        assert_eq!(parse_callback_request(&get("/callback?code=C1&state=t"), "s"), Callback::BadState);
        assert_eq!(parse_callback_request(&get("/callback?code=C1"), "s"), Callback::BadState);
        assert_eq!(parse_callback_request(&get("/callback?error=access_denied&state=s"), "s"), Callback::Denied("access_denied".into()));
        assert_eq!(parse_callback_request(&get("/favicon.ico"), "s"), Callback::Other);
        assert_eq!(parse_callback_request("POST /callback?code=C&state=s HTTP/1.1\r\n\r\n", "s"), Callback::Other);
        assert_eq!(parse_callback_request("", "s"), Callback::Other);
        let a = random_state();
        assert_eq!(a.len(), 32);
        assert_ne!(a, random_state());
    }

    #[test]
    fn listener_answers_once_and_checks_state() {
        let l = CallbackListener::bind_port(0).unwrap();
        let port = l.port();
        assert_eq!(l.redirect_uri(), format!("http://localhost:{port}/callback"));
        let client = std::thread::spawn(move || {
            let get = |target: &str| {
                let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
                write!(s, "GET {target} HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();
                let mut out = String::new();
                let _ = s.read_to_string(&mut out);
                out
            };
            let a = get("/favicon.ico");
            let b = get("/callback?code=nope&state=wrong");
            let c = get("/callback?code=good&state=st8");
            (a, b, c)
        });
        let cancel = AtomicBool::new(false);
        let code = l.wait_for_code("st8", Duration::from_secs(10), &cancel).unwrap();
        let (a, b, c) = client.join().unwrap();
        assert_eq!(code, "good");
        assert!(a.starts_with("HTTP/1.1 404"));
        assert!(b.starts_with("HTTP/1.1 400"));
        assert!(c.starts_with("HTTP/1.1 200") && c.contains("You can close this tab"));
        cancel.store(true, Ordering::SeqCst);
        assert!(matches!(l.wait_for_code("st8", Duration::from_secs(10), &cancel), Err(Error::Cancelled)));
        let idle = AtomicBool::new(false);
        assert!(matches!(l.wait_for_code("st8", Duration::from_millis(150), &idle), Err(Error::Timeout(_))));
    }

    /// A fake token endpoint: records the requests, answers from a queue.
    fn fake(answers: Vec<(u16, &'static str)>) -> (Transport, Arc<Mutex<Vec<TokenRequest>>>) {
        let log = Arc::new(Mutex::new(Vec::new()));
        let queue = Arc::new(Mutex::new(answers));
        let l2 = log.clone();
        let t: Transport = Arc::new(move |r: &TokenRequest| {
            l2.lock().unwrap().push(r.clone());
            let mut q = queue.lock().unwrap();
            if q.is_empty() {
                return Err(Error::Http("unexpected token request".into()));
            }
            let (c, b) = q.remove(0);
            Ok((c, b.to_string()))
        });
        (t, log)
    }

    fn connected_config(dir: &Path, expires_at: u64) -> (PathBuf, SharedConfig) {
        let path = dir.join("config.json");
        let mut a = AppConfig::default();
        let mut o = app();
        o.access_token = Some("old-access".into());
        o.access_expires_at = Some(expires_at);
        o.refresh_token = Some("old-refresh".into());
        o.refreshed_at = Some(now_unix() - 3600);
        a.smartthings_oauth = Some(o);
        a.smartthings_token = Some("legacy-pat".into());
        a.save_to(&path).unwrap();
        let shared = SharedConfig::from_app(AppConfig::load_from(&path).unwrap(), Some(path.clone()));
        (path, shared)
    }

    #[test]
    fn import_login_bearer_refresh_logout() {
        let dir = temp("flow");
        let path = dir.join("config.json");
        let shared = SharedConfig::from_app(AppConfig::default(), Some(path.clone()));
        let (t, log) = fake(vec![
            (200, r#"{"access_token":"A1","refresh_token":"R1","expires_in":86399,"scope":"r:devices:*"}"#),
            (200, r#"{"access_token":"A2","refresh_token":"R2","expires_in":86399}"#),
            (400, r#"{"error":"invalid_grant"}"#),
        ]);
        let auth = StAuth::with_transport(&shared, t);
        assert!(matches!(auth.bearer(), Err(Error::SmartThingsNotConnected)));
        assert!(auth.authorize_url("s", LOCAL_REDIRECT).is_err(), "no app yet");
        assert!(matches!(client_for(&shared), Err(Error::SmartThingsNotConnected)));

        let file = dir.join("st-app-created.json");
        std::fs::write(&file, r#"{"app": {"appName": "tv-remote-x", "appId": "app-1", "appType": "API_ONLY"},
            "oauthClientId": "fake-client", "oauthClientSecret": "fake-secret"}"#).unwrap();
        let imported = auth.import_app(&file).unwrap();
        assert_eq!(imported.app_id.as_deref(), Some("app-1"));
        let st = auth.status();
        assert!(st.app_imported && !st.connected && !st.needs_login);
        assert_eq!(st.summary(), "App imported: not connected yet");
        assert!(auth.authorize_url("s", PASTE_REDIRECT).unwrap().contains("redirect_uri=https%3A%2F%2Fhttpbin.org%2Fget"));

        auth.login_with_input("https://httpbin.org/get?code=C0&state=s", Some("s"), PASTE_REDIRECT).unwrap();
        assert!(log.lock().unwrap()[0].body.contains("code=C0"));
        let disk = AppConfig::load_from(&path).unwrap().smartthings_oauth.unwrap();
        assert_eq!(disk.refresh_token.as_deref(), Some("R1"), "saved at once");
        assert_eq!(disk.redirect_uri.as_deref(), Some(PASTE_REDIRECT));
        assert_eq!(auth.bearer().unwrap(), "A1", "fresh: no refresh");
        assert_eq!(log.lock().unwrap().len(), 1);
        let st = auth.status();
        assert!(st.connected && st.method == AuthMethod::OAuth && st.access_expires_in.unwrap() > 86000);
        assert!(st.summary().starts_with("Connected via OAuth"));
        assert!(client_for(&shared).is_ok());

        auth.refresh().unwrap();
        assert!(log.lock().unwrap()[1].body.contains("refresh_token=R1"));
        assert_eq!(AppConfig::load_from(&path).unwrap().smartthings_oauth.unwrap().refresh_token.as_deref(), Some("R2"));

        // invalid_grant: tokens cleared (saved), login expired.
        assert!(matches!(auth.refresh(), Err(Error::SmartThingsLoginExpired)));
        let st = auth.status();
        assert!(st.needs_login && !st.connected);
        assert!(matches!(auth.bearer(), Err(Error::SmartThingsLoginExpired)));
        assert!(matches!(client_for(&shared), Err(Error::SmartThingsLoginExpired)));
        assert_eq!(log.lock().unwrap().len(), 3, "no request without a refresh token");

        auth.logout().unwrap();
        let st = auth.status();
        assert!(st.app_imported && !st.needs_login, "logout keeps the app");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bearer_refreshes_near_expiry_and_adopts_rotations_from_other_processes() {
        let dir = temp("rotate");
        // Expires in 5 minutes: bearer must refresh first.
        let (path, shared) = connected_config(&dir, now_unix() + 300);
        let (t, log) = fake(vec![(200, r#"{"access_token":"A2","refresh_token":"R2","expires_in":86399}"#)]);
        let auth = StAuth::with_transport(&shared, t.clone());
        assert_eq!(auth.bearer().unwrap(), "A2");
        assert!(log.lock().unwrap()[0].body.contains("refresh_token=old-refresh"));
        assert_eq!(AppConfig::load_from(&path).unwrap().smartthings_oauth.unwrap().access_token.as_deref(), Some("A2"));

        // OAuth wins over the PAT.
        assert!(format!("{:?}", client_for(&shared).unwrap()).contains("OAuth"));

        // Another process (the timer) rotates the pair on disk: our A2 is now invalid.
        let mut other = AppConfig::load_from(&path).unwrap();
        let o = other.smartthings_oauth.as_mut().unwrap();
        o.access_token = Some("A3".into());
        o.refresh_token = Some("R3".into());
        other.save_to(&path).unwrap();
        // A 401 with A2 adopts A3 from the file without spending R3.
        assert_eq!(auth.after_unauthorized("A2").unwrap(), "A3");
        assert_eq!(log.lock().unwrap().len(), 1);
        // A 401 with the current token: refresh (here the fake endpoint has no answer
        // left, so the network error surfaces and nothing is cleared).
        assert!(matches!(auth.after_unauthorized("A3"), Err(Error::Http(_))));
        assert!(auth.is_connected());

        // A second handle (another "process" view) re-reads inside the lock.
        let shared2 = SharedConfig::from_app(AppConfig::load_from(&path).unwrap(), Some(path.clone()));
        let (t2, log2) = fake(vec![]);
        assert_eq!(StAuth::with_transport(&shared2, t2).bearer().unwrap(), "A3");
        assert!(log2.lock().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn concurrent_bearers_spend_the_refresh_token_once() {
        let dir = temp("race");
        let (_path, shared) = connected_config(&dir, now_unix() + 60);
        let (t, log) = fake(vec![(200, r#"{"access_token":"A2","refresh_token":"R2","expires_in":86399}"#)]);
        let handles: Vec<_> = (0..8)
            .map(|_| {
                // Separate SharedConfig instances, like separate processes on one file.
                let s = SharedConfig::from_app(AppConfig::load_from(shared.path().unwrap()).unwrap(), shared.path().map(Path::to_path_buf));
                let auth = StAuth::with_transport(&s, t.clone());
                std::thread::spawn(move || auth.bearer())
            })
            .collect();
        for h in handles {
            assert_eq!(h.join().unwrap().unwrap(), "A2");
        }
        assert_eq!(log.lock().unwrap().len(), 1, "exactly one refresh");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn status_summaries() {
        let now = 10_000_000;
        let mut a = AppConfig::default();
        assert_eq!(AuthStatus::from_config(&a, now).summary(), "Not set up");
        a.smartthings_token = Some("p".into());
        let s = AuthStatus::from_config(&a, now);
        assert_eq!(s.method, AuthMethod::Pat);
        let mut o = app();
        o.access_token = Some("a".into());
        o.access_expires_at = Some(now + 300);
        o.refresh_token = Some("r".into());
        o.refreshed_at = Some(now - 31 * 24 * 3600);
        a.smartthings_oauth = Some(o);
        let s = AuthStatus::from_config(&a, now);
        assert!(s.connected && s.expiring_soon() && s.needs_login, "refresh token older than 30 days");
        assert_eq!(s.access_expires_in, Some(300));
        let json = serde_json::to_string(&s).unwrap();
        assert!(!json.contains("\"r\"") && !json.contains("fake-secret"));
    }
}
