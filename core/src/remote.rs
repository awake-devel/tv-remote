//! Remote-control WebSocket channel (`samsung.remote.control`).
//!
//! A [`Remote`] owns one background thread that holds the socket. Callers send
//! commands through a channel and get a `Result` back; connection state changes,
//! new tokens, and unsolicited TV events go out on the [`EventBus`].
//!
//! The thread connects lazily: on [`Remote::spawn`] it starts connecting right away,
//! reconnects with exponential back-off (1 s → 30 s) when the link drops, and makes an
//! immediate attempt whenever a command arrives while disconnected.

use std::net::{TcpStream, ToSocketAddrs};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

use crate::config::{Config, SharedConfig};
use crate::error::{Error, Result};
use crate::events::{ConnectionState, Event, EventBus};
use crate::keys::{Key, KeyAction};

type Ws = WebSocket<MaybeTlsStream<TcpStream>>;

/// How long a command waits for the thread (including a reconnect attempt).
/// Must outlast [`PAIR_TIMEOUT`]: the first command from a new client waits while the TV
/// shows its approval prompt.
const CMD_TIMEOUT: Duration = Duration::from_secs(40);
/// How long `installed_apps` waits for the TV's answer.
const APPS_TIMEOUT: Duration = Duration::from_secs(10);
/// How long we wait for `ms.channel.connect` (covers the on-screen approval prompt).
const PAIR_TIMEOUT: Duration = Duration::from_secs(30);

/// An installed app as reported by `ed.installedApp.get`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct App {
    /// App id, e.g. `111299001912` (YouTube) or `3201907018807` (Netflix).
    pub app_id: String,
    /// Display name.
    pub name: String,
    /// Raw `app_type` (2 = web/deep-link app, 4 = native app).
    pub app_type: i64,
    /// Icon path on the TV (not directly fetchable without extra API calls).
    pub icon: Option<String>,
}

impl App {
    /// The launch type the TV expects for this app.
    pub fn launch_type(&self) -> LaunchType {
        if self.app_type == 4 {
            LaunchType::NativeLaunch
        } else {
            LaunchType::DeepLink
        }
    }
}

/// `action_type` for `ed.apps.launch`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchType {
    /// `DEEP_LINK` (most store apps; `meta_tag` is passed to the app).
    DeepLink,
    /// `NATIVE_LAUNCH`.
    NativeLaunch,
}

impl LaunchType {
    /// Wire value.
    pub fn as_str(self) -> &'static str {
        match self {
            LaunchType::DeepLink => "DEEP_LINK",
            LaunchType::NativeLaunch => "NATIVE_LAUNCH",
        }
    }
}

// ---------------------------------------------------------------------------
// Pure message builders (unit-tested)

/// WebSocket URL for the remote channel. Port 8002 → `wss://`, anything else → `ws://`.
pub fn ws_url(host: &str, port: u16, name: &str, token: Option<&str>) -> String {
    let scheme = if port == 8002 { "wss" } else { "ws" };
    let name_b64 = base64::engine::general_purpose::STANDARD.encode(name.as_bytes());
    let mut url = format!(
        "{scheme}://{host}:{port}/api/v2/channels/samsung.remote.control?name={name_b64}"
    );
    if let Some(t) = token.filter(|t| !t.is_empty()) {
        url.push_str("&token=");
        url.push_str(t);
    }
    url
}

/// `ms.remote.control` / `SendRemoteKey` message.
pub fn key_message(key: &Key, action: KeyAction) -> String {
    json!({"method": "ms.remote.control", "params": {
        "Cmd": action.as_str(), "DataOfCmd": key.code(), "Option": "false",
        "TypeOfRemote": "SendRemoteKey"}})
    .to_string()
}

/// `SendInputString` (base64 text) followed by `SendInputEnd`.
pub fn text_messages(text: &str) -> [String; 2] {
    let b64 = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    [
        json!({"method": "ms.remote.control", "params": {
            "Cmd": b64, "DataOfCmd": "base64", "TypeOfRemote": "SendInputString"}})
        .to_string(),
        json!({"method": "ms.remote.control", "params": {"TypeOfRemote": "SendInputEnd"}})
            .to_string(),
    ]
}

/// `ProcessMouseDevice` relative move.
pub fn mouse_move_message(dx: i32, dy: i32) -> String {
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    json!({"method": "ms.remote.control", "params": {
        "Cmd": "Move", "Position": {"x": dx, "y": dy, "Time": format!("{t:.3}")},
        "TypeOfRemote": "ProcessMouseDevice"}})
    .to_string()
}

/// `ProcessMouseDevice` left click.
pub fn mouse_click_message() -> String {
    json!({"method": "ms.remote.control", "params": {
        "Cmd": "LeftClick", "TypeOfRemote": "ProcessMouseDevice"}})
    .to_string()
}

/// Request for the installed-app list.
pub fn installed_apps_message() -> String {
    json!({"method": "ms.channel.emit", "params": {"event": "ed.installedApp.get", "to": "host"}})
        .to_string()
}

/// `ed.apps.launch` message.
pub fn launch_app_message(app_id: &str, kind: LaunchType, meta_tag: Option<&str>) -> String {
    let mut data = json!({"appId": app_id, "action_type": kind.as_str()});
    if let Some(m) = meta_tag {
        data["metaTag"] = json!(m);
    }
    json!({"method": "ms.channel.emit", "params": {"event": "ed.apps.launch", "to": "host", "data": data}})
        .to_string()
}

/// Parse the `data` of an `ed.installedApp.get` event.
pub fn parse_installed_apps(event: &serde_json::Value) -> Vec<App> {
    let list = event["data"]["data"]
        .as_array()
        .or_else(|| event["data"].as_array())
        .cloned()
        .unwrap_or_default();
    list.iter()
        .filter_map(|a| {
            let id = a["appId"].as_str()?.to_string();
            Some(App {
                name: a["name"].as_str().unwrap_or(&id).to_string(),
                app_type: a["app_type"].as_i64().unwrap_or(2),
                icon: a["icon"].as_str().map(str::to_string),
                app_id: id,
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Thread + handle

enum Cmd {
    Send(Vec<String>, Sender<Result<()>>),
    Apps(Sender<Result<Vec<App>>>),
    Reconnect,
    Shutdown,
}

/// Handle to the remote-control channel thread. Cheap to clone; the thread stops
/// when the last clone is dropped or [`Remote::shutdown`] is called.
#[derive(Clone)]
pub struct Remote {
    tx: Sender<Cmd>,
    state: Arc<Mutex<ConnectionState>>,
    _guard: Arc<ShutdownGuard>,
}

struct ShutdownGuard {
    tx: Sender<Cmd>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl Drop for ShutdownGuard {
    fn drop(&mut self) {
        let _ = self.tx.send(Cmd::Shutdown);
        // Don't join: the thread may be blocked in a connect for a few seconds.
        if let Ok(mut t) = self.thread.lock() {
            t.take();
        }
    }
}

impl std::fmt::Debug for Remote {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Remote").field("state", &self.state()).finish()
    }
}

impl Remote {
    /// Start the channel thread. It begins connecting immediately (non-blocking).
    pub fn spawn(config: SharedConfig, bus: EventBus) -> Remote {
        let (tx, rx) = channel();
        let state = Arc::new(Mutex::new(ConnectionState::Disconnected));
        let st = state.clone();
        let thread = std::thread::Builder::new()
            .name("tv-remote-ws".into())
            .spawn(move || Worker::new(config, bus, st, rx).run())
            .ok();
        Remote {
            tx: tx.clone(),
            state,
            _guard: Arc::new(ShutdownGuard {
                tx,
                thread: Mutex::new(thread),
            }),
        }
    }

    /// Current connection state.
    pub fn state(&self) -> ConnectionState {
        self.state
            .lock()
            .map(|s| s.clone())
            .unwrap_or(ConnectionState::Disconnected)
    }

    /// True when connected.
    pub fn is_connected(&self) -> bool {
        self.state() == ConnectionState::Connected
    }

    /// Drop the current socket (if any) and reconnect now (e.g. after a host change).
    pub fn reconnect(&self) {
        let _ = self.tx.send(Cmd::Reconnect);
    }

    /// Stop the thread. Further calls return [`Error::NotConnected`].
    pub fn shutdown(&self) {
        let _ = self.tx.send(Cmd::Shutdown);
    }

    fn send_raw(&self, msgs: Vec<String>) -> Result<()> {
        let (rtx, rrx) = channel();
        self.tx
            .send(Cmd::Send(msgs, rtx))
            .map_err(|_| Error::NotConnected)?;
        rrx.recv_timeout(CMD_TIMEOUT)
            .map_err(|_| Error::Timeout("remote channel did not answer".into()))?
    }

    /// Send a key (Click / Press / Release).
    pub fn send_key(&self, key: &Key, action: KeyAction) -> Result<()> {
        self.send_raw(vec![key_message(key, action)])
    }

    /// Type text into the focused TV text field (`SendInputString` + `SendInputEnd`).
    pub fn send_text(&self, text: &str) -> Result<()> {
        self.send_raw(text_messages(text).to_vec())
    }

    /// Move the TV pointer by (dx, dy) pixels (apps with pointer support, e.g. the browser).
    pub fn mouse_move(&self, dx: i32, dy: i32) -> Result<()> {
        self.send_raw(vec![mouse_move_message(dx, dy)])
    }

    /// Left click at the pointer position.
    pub fn mouse_click(&self) -> Result<()> {
        self.send_raw(vec![mouse_click_message()])
    }

    /// Ask the TV for its installed apps (waits up to ~10 s for the answer).
    pub fn installed_apps(&self) -> Result<Vec<App>> {
        let (rtx, rrx) = channel();
        self.tx.send(Cmd::Apps(rtx)).map_err(|_| Error::NotConnected)?;
        rrx.recv_timeout(CMD_TIMEOUT + APPS_TIMEOUT)
            .map_err(|_| Error::Timeout("installed app list".into()))?
    }

    /// Launch an app (`ed.apps.launch`). `meta_tag` is the deep-link payload, if any.
    pub fn launch_app(&self, app_id: &str, kind: LaunchType, meta_tag: Option<&str>) -> Result<()> {
        self.send_raw(vec![launch_app_message(app_id, kind, meta_tag)])
    }
}

struct Worker {
    config: SharedConfig,
    bus: EventBus,
    state: Arc<Mutex<ConnectionState>>,
    rx: Receiver<Cmd>,
    ws: Option<Ws>,
    backoff: Duration,
    next_attempt: Instant,
    last_attempt: Option<Instant>,
    pending_apps: Vec<(Instant, Sender<Result<Vec<App>>>)>,
}

impl Worker {
    fn new(
        config: SharedConfig,
        bus: EventBus,
        state: Arc<Mutex<ConnectionState>>,
        rx: Receiver<Cmd>,
    ) -> Self {
        Worker {
            config,
            bus,
            state,
            rx,
            ws: None,
            backoff: Duration::from_secs(1),
            next_attempt: Instant::now(),
            last_attempt: None,
            pending_apps: Vec::new(),
        }
    }

    fn set_state(&self, s: ConnectionState) {
        let changed = match self.state.lock() {
            Ok(mut cur) => {
                let changed = *cur != s;
                *cur = s.clone();
                changed
            }
            Err(_) => false,
        };
        if changed {
            self.bus.emit(Event::Connection(s));
        }
    }

    fn run(mut self) {
        loop {
            if self.ws.is_none() && Instant::now() >= self.next_attempt {
                self.try_connect();
            }
            // Drain commands.
            let wait = if self.ws.is_some() {
                Duration::from_millis(0)
            } else {
                self.next_attempt
                    .saturating_duration_since(Instant::now())
                    .clamp(Duration::from_millis(50), Duration::from_millis(500))
            };
            let mut first = true;
            loop {
                let cmd = if first && !wait.is_zero() {
                    match self.rx.recv_timeout(wait) {
                        Ok(c) => Some(c),
                        Err(RecvTimeoutError::Timeout) => None,
                        Err(RecvTimeoutError::Disconnected) => return,
                    }
                } else {
                    match self.rx.try_recv() {
                        Ok(c) => Some(c),
                        Err(std::sync::mpsc::TryRecvError::Empty) => None,
                        Err(std::sync::mpsc::TryRecvError::Disconnected) => return,
                    }
                };
                first = false;
                let Some(cmd) = cmd else { break };
                match cmd {
                    Cmd::Shutdown => {
                        if let Some(mut ws) = self.ws.take() {
                            let _ = ws.close(None);
                            let _ = ws.flush();
                        }
                        self.set_state(ConnectionState::Disconnected);
                        return;
                    }
                    Cmd::Reconnect => {
                        self.drop_socket(None);
                        self.backoff = Duration::from_secs(1);
                        self.next_attempt = Instant::now();
                    }
                    Cmd::Send(msgs, reply) => {
                        let r = self.ensure_connected().and_then(|_| self.write_all(&msgs));
                        let _ = reply.send(r);
                    }
                    Cmd::Apps(reply) => {
                        let r = self
                            .ensure_connected()
                            .and_then(|_| self.write_all(&[installed_apps_message()]));
                        match r {
                            Ok(()) => self.pending_apps.push((Instant::now(), reply)),
                            Err(e) => {
                                let _ = reply.send(Err(e));
                            }
                        }
                    }
                }
            }
            // Read from the socket (short read timeout keeps the loop responsive).
            if self.ws.is_some() {
                self.read_once();
            }
            // Expire app-list requests that never got an answer.
            let now = Instant::now();
            self.pending_apps.retain(|(t, tx)| {
                if now.duration_since(*t) > APPS_TIMEOUT {
                    let _ = tx.send(Err(Error::Timeout("the TV did not send its app list".into())));
                    false
                } else {
                    true
                }
            });
        }
    }

    fn ensure_connected(&mut self) -> Result<()> {
        if self.ws.is_some() {
            return Ok(());
        }
        // Avoid hammering: at most one on-demand attempt per second.
        if self
            .last_attempt
            .is_none_or(|t| t.elapsed() >= Duration::from_secs(1))
        {
            self.try_connect();
        }
        if self.ws.is_some() {
            Ok(())
        } else {
            Err(Error::NotConnected)
        }
    }

    fn write_all(&mut self, msgs: &[String]) -> Result<()> {
        let ws = self.ws.as_mut().ok_or(Error::NotConnected)?;
        let mut res = Ok(());
        for m in msgs {
            if let Err(e) = ws.send(Message::Text(m.clone())) {
                res = Err(Error::from(e));
                break;
            }
        }
        if let Err(e) = &res {
            let reason = e.to_string();
            self.drop_socket(Some(reason));
        }
        res
    }

    fn drop_socket(&mut self, reason: Option<String>) {
        if let Some(mut ws) = self.ws.take() {
            let _ = ws.close(None);
            let _ = ws.flush();
        }
        for (_, tx) in self.pending_apps.drain(..) {
            let _ = tx.send(Err(Error::NotConnected));
        }
        match reason {
            Some(r) => {
                self.set_state(ConnectionState::Retrying(r));
                self.next_attempt = Instant::now() + Duration::from_secs(1);
            }
            None => self.set_state(ConnectionState::Disconnected),
        }
    }

    fn try_connect(&mut self) {
        self.last_attempt = Some(Instant::now());
        self.set_state(ConnectionState::Connecting);
        let cfg = self.config.get();
        match open_and_pair(&cfg) {
            Ok((ws, new_token)) => {
                if let Some(tok) = new_token {
                    if cfg.token.as_deref() != Some(tok.as_str())
                        && self.config.update(|c| c.token = Some(tok)).is_ok()
                    {
                        self.bus.emit(Event::TokenUpdated);
                    }
                }
                self.ws = Some(ws);
                self.backoff = Duration::from_secs(1);
                self.set_state(ConnectionState::Connected);
            }
            Err(Error::Unauthorized) => {
                self.set_state(ConnectionState::Unauthorized);
                self.next_attempt = Instant::now() + Duration::from_secs(60);
            }
            Err(e) => {
                self.set_state(ConnectionState::Retrying(e.to_string()));
                self.next_attempt = Instant::now() + self.backoff;
                self.backoff = (self.backoff * 2).min(Duration::from_secs(30));
            }
        }
    }

    fn read_once(&mut self) {
        let Some(ws) = self.ws.as_mut() else { return };
        match ws.read() {
            Ok(Message::Text(t)) => self.handle_text(&t),
            Ok(Message::Close(_)) => self.drop_socket(Some("the TV closed the connection".into())),
            Ok(_) => {}
            Err(tungstenite::Error::Io(e))
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                // No data; flush queued pongs.
                let _ = ws.flush();
            }
            Err(e) => self.drop_socket(Some(e.to_string())),
        }
    }

    fn handle_text(&mut self, text: &str) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else {
            return;
        };
        let name = v["event"].as_str().unwrap_or_default().to_string();
        match name.as_str() {
            "ed.installedApp.get" => {
                let apps = parse_installed_apps(&v);
                for (_, tx) in self.pending_apps.drain(..) {
                    let _ = tx.send(Ok(apps.clone()));
                }
            }
            "ms.channel.connect" | "ms.channel.ready" | "ms.channel.clientConnect"
            | "ms.channel.clientDisconnect" => {}
            "ms.channel.unauthorized" => {
                self.drop_socket(None);
                self.set_state(ConnectionState::Unauthorized);
                self.next_attempt = Instant::now() + Duration::from_secs(60);
            }
            _ => self.bus.emit(Event::RemoteEvent {
                name,
                data: v["data"].clone(),
            }),
        }
    }
}

fn set_read_timeout(ws: &mut Ws, d: Duration) {
    match ws.get_mut() {
        MaybeTlsStream::Plain(s) => {
            let _ = s.set_read_timeout(Some(d));
        }
        MaybeTlsStream::NativeTls(s) => {
            let _ = s.get_mut().set_read_timeout(Some(d));
        }
        _ => {}
    }
}

/// Connect, then wait for `ms.channel.connect` (returns its `data.token`, if any).
fn open_and_pair(cfg: &Config) -> Result<(Ws, Option<String>)> {
    if cfg.host.is_empty() {
        return Err(Error::Config("no TV host configured".into()));
    }
    let addr = (cfg.host.as_str(), cfg.port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| Error::WebSocket(format!("cannot resolve {}", cfg.host)))?;
    let tcp = TcpStream::connect_timeout(&addr, Duration::from_secs(4))?;
    tcp.set_nodelay(true)?;
    tcp.set_read_timeout(Some(Duration::from_secs(8)))?;
    tcp.set_write_timeout(Some(Duration::from_secs(8)))?;
    let tls = native_tls::TlsConnector::builder()
        .danger_accept_invalid_certs(true)
        .danger_accept_invalid_hostnames(true)
        .build()
        .map_err(|e| Error::WebSocket(format!("TLS init: {e}")))?;
    let url = ws_url(&cfg.host, cfg.port, &cfg.name, cfg.token.as_deref());
    let (mut ws, _resp) = tungstenite::client_tls_with_config(
        url.as_str(),
        tcp,
        None,
        Some(tungstenite::Connector::NativeTls(tls)),
    )
    .map_err(|e| Error::WebSocket(format!("handshake failed: {e}")))?;

    set_read_timeout(&mut ws, Duration::from_millis(500));
    let deadline = Instant::now() + PAIR_TIMEOUT;
    loop {
        if Instant::now() > deadline {
            return Err(Error::Timeout("no ms.channel.connect from the TV".into()));
        }
        match ws.read() {
            Ok(Message::Text(t)) => {
                let v: serde_json::Value = serde_json::from_str(&t).unwrap_or_default();
                match v["event"].as_str() {
                    Some("ms.channel.connect") => {
                        let tok = v["data"]["token"]
                            .as_str()
                            .map(str::to_string)
                            .or_else(|| v["data"]["token"].as_i64().map(|n| n.to_string()));
                        set_read_timeout(&mut ws, Duration::from_millis(50));
                        return Ok((ws, tok));
                    }
                    Some("ms.channel.unauthorized") | Some("ms.channel.timeOut") => {
                        return Err(Error::Unauthorized)
                    }
                    _ => {}
                }
            }
            Ok(Message::Close(_)) => return Err(Error::WebSocket("closed during pairing".into())),
            Ok(_) => {}
            Err(tungstenite::Error::Io(e))
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(e) => return Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_and_messages() {
        let u = ws_url("10.0.0.2", 8002, "Ryoku", Some("123"));
        assert_eq!(
            u,
            "wss://10.0.0.2:8002/api/v2/channels/samsung.remote.control?name=Unlva3U=&token=123"
        );
        assert!(ws_url("h", 8001, "Ryoku", None).starts_with("ws://h:8001/"));

        let k: serde_json::Value =
            serde_json::from_str(&key_message(&Key::VolumeUp, KeyAction::Click)).unwrap();
        assert_eq!(k["params"]["DataOfCmd"], "KEY_VOLUP");
        assert_eq!(k["params"]["TypeOfRemote"], "SendRemoteKey");

        let [a, b] = text_messages("hé");
        let a: serde_json::Value = serde_json::from_str(&a).unwrap();
        assert_eq!(a["params"]["Cmd"], "aMOp");
        assert!(b.contains("SendInputEnd"));

        let l: serde_json::Value =
            serde_json::from_str(&launch_app_message("111299001912", LaunchType::DeepLink, None))
                .unwrap();
        assert_eq!(l["params"]["data"]["action_type"], "DEEP_LINK");
        assert!(mouse_move_message(3, -4).contains("\"x\":3"));
    }

    #[test]
    fn parse_apps() {
        let ev = serde_json::json!({"event":"ed.installedApp.get","data":{"data":[
            {"appId":"111299001912","app_type":2,"icon":"/x.png","is_lock":0,"name":"YouTube"},
            {"appId":"org.tizen.browser","app_type":4,"name":"Internet"}]}});
        let apps = parse_installed_apps(&ev);
        assert_eq!(apps.len(), 2);
        assert_eq!(apps[1].launch_type(), LaunchType::NativeLaunch);
    }
}
