//! UniFFI surface of `tvremote_core` for the Android app.
//!
//! Every method blocks (network I/O): Kotlin calls them on `Dispatchers.IO`. Events from
//! the core (connection, power, TV keyboard, list changes) reach Kotlin through an
//! [`EventListener`] callback, called on a Rust background thread.
//!
//! Scope of this first milestone: discovery, pairing, keys, text, touchpad, power,
//! volume and apps. No casting, no SmartThings.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tvremote_core::fleet::{self, LanTv};
use tvremote_core::remote::{App, LaunchType};
use tvremote_core::{
    config, wol, Candidate, ConnectionState, Error, Event, Fleet, Key, KeyAction, PowerState, Tv, TvEntry,
    YouTubeOpened,
};

uniffi::setup_scaffolding!();

// ---------------------------------------------------------------------------------------
// Errors

/// A failed call. `message` is short enough for a snackbar.
#[derive(Debug, thiserror::Error, uniffi::Error)]
// Flat: Kotlin gets one exception class per variant whose `message` is the text.
#[uniffi(flat_error)]
pub enum RemoteError {
    /// The TV refused the pairing (denied on the TV, or the prompt timed out).
    #[error("{0}")]
    NotAuthorized(String),
    /// The TV did not answer in time.
    #[error("{0}")]
    Timeout(String),
    /// Network failure: the TV is off, unreachable, or dropped the connection.
    #[error("{0}")]
    Unreachable(String),
    /// Bad configuration or input (unknown TV id, empty host, no MAC for Wake-on-LAN, …).
    #[error("{0}")]
    Invalid(String),
    /// The TV or this build cannot do it.
    #[error("{0}")]
    Unsupported(String),
    /// Anything else.
    #[error("{0}")]
    Other(String),
}

impl From<Error> for RemoteError {
    fn from(e: Error) -> Self {
        let message = e.to_string();
        match e {
            Error::Unauthorized => RemoteError::NotAuthorized(message),
            Error::Timeout(_) => RemoteError::Timeout(message),
            Error::Io(_) | Error::Http(_) | Error::WebSocket(_) | Error::NotConnected | Error::Soap { .. } => {
                RemoteError::Unreachable(message)
            }
            Error::Config(_) | Error::Parse(_) => RemoteError::Invalid(message),
            Error::Unsupported(_) => RemoteError::Unsupported(message),
            _ => RemoteError::Other(message),
        }
    }
}

type Res<T> = std::result::Result<T, RemoteError>;

// ---------------------------------------------------------------------------------------
// Keys

macro_rules! remote_keys {
    ($($v:ident),* $(,)?) => {
        /// Every named key of the core ([`tvremote_core::Key`] without `Raw`; use
        /// [`TvRemote::send_key_code`] for other codes).
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, uniffi::Enum)]
        pub enum RemoteKey { $($v,)* }

        impl RemoteKey {
            /// All keys, in the core's display order.
            pub const ALL: &'static [RemoteKey] = &[$(RemoteKey::$v,)*];

            fn core(self) -> Key {
                match self { $(RemoteKey::$v => Key::$v,)* }
            }
        }
    };
}

remote_keys! {
    Power, PowerOff, Up, Down, Left, Right, Enter, Return, Exit, Home, Menu, Source, Info, Guide, Tools,
    VolumeUp, VolumeDown, Mute, ChannelUp, ChannelDown, ChannelList, PreviousChannel,
    Num0, Num1, Num2, Num3, Num4, Num5, Num6, Num7, Num8, Num9,
    Red, Green, Yellow, Blue, Play, Pause, Stop, FastForward, Rewind,
}

/// The `KEY_*` code sent for a key.
#[uniffi::export]
pub fn key_code(key: RemoteKey) -> String {
    key.core().code().to_string()
}

/// The short button label of a key (`"Vol +"`).
#[uniffi::export]
pub fn key_label(key: RemoteKey) -> String {
    key.core().label().to_string()
}

/// How a key is sent: a click, or press/release for long presses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum KeyPress {
    Click,
    Press,
    Release,
}

impl From<KeyPress> for KeyAction {
    fn from(p: KeyPress) -> Self {
        match p {
            KeyPress::Click => KeyAction::Click,
            KeyPress::Press => KeyAction::Press,
            KeyPress::Release => KeyAction::Release,
        }
    }
}

// ---------------------------------------------------------------------------------------
// Records

/// A TV in the config.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct TvSummary {
    /// Stable local id (`tv1`, …).
    pub id: String,
    pub name: String,
    /// IP address (empty for a TV without a LAN address).
    pub host: String,
    pub model: Option<String>,
    /// For Wake-on-LAN; `None` = power on only works from network standby.
    pub mac: Option<String>,
    /// A pairing token is stored (the TV approved this phone).
    pub paired: bool,
    pub active: bool,
}

fn summary(e: &TvEntry, active: Option<&str>) -> TvSummary {
    TvSummary {
        id: e.id.clone(),
        name: e.name.clone(),
        host: e.host.clone(),
        model: e.model.clone(),
        mac: e.mac.clone(),
        paired: e.token.is_some(),
        active: active == Some(e.id.as_str()),
    }
}

/// Power as seen over REST.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum TvPower {
    On,
    /// Network standby: screen off, still answering.
    Standby,
    /// Not answering (fully off or unreachable).
    Off,
    Unknown,
}

impl From<&PowerState> for TvPower {
    fn from(p: &PowerState) -> Self {
        match p {
            PowerState::On => TvPower::On,
            PowerState::Standby => TvPower::Standby,
            PowerState::Off => TvPower::Off,
            PowerState::Unknown(_) => TvPower::Unknown,
        }
    }
}

/// A TV found by a scan.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FoundTv {
    /// Pass to [`RemoteCore::add_found`] (`lan:<ip>`).
    pub key: String,
    pub name: String,
    pub model: Option<String>,
    pub ip: Option<String>,
    pub mac: Option<String>,
    /// `None` when the TV did not answer REST.
    pub power: Option<TvPower>,
    /// Id of the TV in the config, if already added.
    pub known_id: Option<String>,
}

fn found(c: &Candidate) -> FoundTv {
    FoundTv {
        key: c.key.clone(),
        name: c.name.clone(),
        model: c.model.clone(),
        ip: c.ip().map(str::to_string),
        mac: c.mac().map(str::to_string),
        power: c.lan.as_ref().and_then(LanTv::power).as_ref().map(TvPower::from),
        known_id: c.known.as_ref().map(|(id, _)| id.clone()),
    }
}

/// What a scan found.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ScanResult {
    pub tvs: Vec<FoundTv>,
    /// Why the LAN search failed, if it did (no Wi-Fi, multicast blocked, …).
    pub error: Option<String>,
}

/// An installed app.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct AppInfo {
    pub app_id: String,
    pub name: String,
    /// 2 = web/deep-link app, 4 = native app (decides how it is launched).
    pub app_type: i64,
    /// Icon path on the TV (not fetchable without extra API calls).
    pub icon: Option<String>,
}

impl From<App> for AppInfo {
    fn from(a: App) -> Self {
        AppInfo { app_id: a.app_id, name: a.name, app_type: a.app_type, icon: a.icon }
    }
}

impl From<AppInfo> for App {
    fn from(a: AppInfo) -> Self {
        App { app_id: a.app_id, name: a.name, app_type: a.app_type, icon: a.icon }
    }
}

/// `GET /api/v2/` of the TV, the useful part.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct DeviceSummary {
    pub name: Option<String>,
    pub model_name: Option<String>,
    pub resolution: Option<String>,
    pub network_type: Option<String>,
    pub wifi_mac: Option<String>,
    pub ip: Option<String>,
    pub os: Option<String>,
    pub power: TvPower,
}

/// What [`TvRemote::youtube`] opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum YouTubeResult {
    /// The video itself.
    Video,
    /// Only the YouTube app (the TV refused the link).
    AppOnly,
}

// ---------------------------------------------------------------------------------------
// Events

/// State of the remote channel.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum Connection {
    Disconnected,
    Connecting,
    Connected,
    /// Lost or failed; retrying after a back-off.
    Retrying { reason: String },
    /// The TV refused the pairing; retried after a long back-off.
    Unauthorized,
}

impl From<&ConnectionState> for Connection {
    fn from(s: &ConnectionState) -> Self {
        match s {
            ConnectionState::Disconnected => Connection::Disconnected,
            ConnectionState::Connecting => Connection::Connecting,
            ConnectionState::Connected => Connection::Connected,
            ConnectionState::Retrying(r) => Connection::Retrying { reason: r.clone() },
            ConnectionState::Unauthorized => Connection::Unauthorized,
        }
    }
}

/// Something that happened in the background.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum CoreEvent {
    Connection { state: Connection },
    Power { power: TvPower },
    /// The TV approved this phone; its token is saved.
    Paired,
    /// The TV's on-screen keyboard opened (`ms.remote.imeStart`): offer text entry.
    KeyboardShown,
    /// The TV's keyboard closed (`ms.remote.imeEnd`).
    KeyboardHidden,
    /// Another unsolicited TV event; `data` is JSON.
    Remote { name: String, data: String },
    /// This TV's settings changed.
    ConfigChanged,
    /// The TV list changed (added, removed, renamed, active TV).
    TvsChanged,
}

/// Map a core event; `None` for events this app ignores (casting, SmartThings).
fn event(e: &Event) -> Option<CoreEvent> {
    Some(match e {
        Event::Connection(s) => CoreEvent::Connection { state: s.into() },
        Event::Power(p) => CoreEvent::Power { power: p.into() },
        Event::TokenUpdated => CoreEvent::Paired,
        Event::RemoteEvent { name, data } => match name.as_str() {
            "ms.remote.imeStart" => CoreEvent::KeyboardShown,
            "ms.remote.imeEnd" => CoreEvent::KeyboardHidden,
            _ => CoreEvent::Remote { name: name.clone(), data: data.to_string() },
        },
        Event::ConfigChanged => CoreEvent::ConfigChanged,
        Event::FleetChanged => CoreEvent::TvsChanged,
        Event::Cast(_) | Event::Cloud(_) => return None,
    })
}

/// Receives core events, on a background thread. `tv_id` is empty for list-wide events.
#[uniffi::export(callback_interface)]
pub trait EventListener: Send + Sync {
    fn on_event(&self, tv_id: String, event: CoreEvent);
}

// ---------------------------------------------------------------------------------------
// Process-wide settings

/// Subnet broadcast addresses for Wake-on-LAN (the phone's Wi-Fi broadcast), added to
/// 255.255.255.255. Unparseable entries are skipped.
#[uniffi::export]
pub fn set_wol_broadcasts(addrs: Vec<String>) {
    wol::set_broadcast_hints(addrs.iter().filter_map(|a| a.trim().parse().ok()).collect());
}

/// The TLS backend compiled into the core (`"rustls"` here).
#[uniffi::export]
pub fn tls_backend() -> String {
    tvremote_core::tls::backend().to_string()
}

// ---------------------------------------------------------------------------------------
// The TV list

/// All TVs (one config file). Create one per process.
#[derive(uniffi::Object)]
pub struct RemoteCore {
    fleet: Fleet,
    /// Candidates of the last scan, for [`RemoteCore::add_found`].
    last_scan: Mutex<Vec<Candidate>>,
    /// Bumped to stop the current listener thread.
    listener_gen: Arc<AtomicU64>,
}

impl Drop for RemoteCore {
    fn drop(&mut self) {
        self.listener_gen.fetch_add(1, Ordering::SeqCst);
        self.fleet.stop_status_polling();
    }
}

#[uniffi::export]
impl RemoteCore {
    /// Open (or start) the config at `config_path`; `cache_dir` holds the last scan.
    /// `client_name` is shown in the TV's pairing prompt; it is stored only in a new
    /// config (an existing one keeps its name, so existing pairings stay valid).
    #[uniffi::constructor]
    pub fn new(config_path: String, cache_dir: String, client_name: String) -> Res<Arc<Self>> {
        config::set_cache_dir(PathBuf::from(cache_dir));
        let path = PathBuf::from(config_path);
        let fresh = !path.exists();
        let fleet = Fleet::open_or_default(&path)?;
        let name = client_name.trim().to_string();
        if fresh && !name.is_empty() {
            fleet.shared_config().update_app(|a| a.name = name)?;
        }
        Ok(Arc::new(RemoteCore {
            fleet,
            last_scan: Mutex::new(Vec::new()),
            listener_gen: Arc::new(AtomicU64::new(0)),
        }))
    }

    /// Known TVs in display order.
    pub fn tvs(&self) -> Vec<TvSummary> {
        let cfg = self.fleet.config();
        cfg.tvs.iter().map(|e| summary(e, cfg.active.as_deref())).collect()
    }

    pub fn active_id(&self) -> Option<String> {
        self.fleet.active_id()
    }

    pub fn set_active(&self, id: String) -> Res<()> {
        Ok(self.fleet.set_active(&id)?)
    }

    pub fn rename(&self, id: String, name: String) -> Res<()> {
        Ok(self.fleet.rename(&id, &name)?)
    }

    /// Forget a TV (its pairing token too).
    pub fn remove(&self, id: String) -> Res<()> {
        Ok(self.fleet.remove(&id)?)
    }

    /// SSDP search plus REST info, for about `timeout_ms`. Known TVs found at a new IP
    /// (same MAC) get their address updated. Needs the Wi-Fi multicast lock held.
    pub fn scan(&self, timeout_ms: u32) -> ScanResult {
        let report = self.fleet.scan(Duration::from_millis(timeout_ms.into()));
        let tvs = report.candidates.iter().filter(|c| c.lan.is_some()).map(found).collect();
        if let Ok(mut g) = self.last_scan.lock() {
            *g = report.candidates.clone();
        }
        ScanResult { tvs, error: report.lan_error }
    }

    /// Add a TV from the last scan by its `key`; returns its id (the existing one if it
    /// is already known). The first TV becomes active.
    pub fn add_found(&self, key: String) -> Res<String> {
        let c = self
            .last_scan
            .lock()
            .ok()
            .and_then(|g| g.iter().find(|c| c.key == key).cloned())
            .ok_or_else(|| RemoteError::Invalid("that TV is no longer in the scan results".into()))?;
        Ok(self.fleet.add_candidate(&c)?)
    }

    /// Add a TV by IP address. If it answers, its name, model and MAC are filled in
    /// (the MAC enables Wake-on-LAN); else it is added with `name` (or a default).
    pub fn add_host(&self, host: String, name: Option<String>) -> Res<String> {
        let host = host.trim().to_string();
        if host.is_empty() {
            return Err(RemoteError::Invalid("enter the TV's IP address".into()));
        }
        if let Some(lan) = Fleet::probe_host(&host) {
            let cands = fleet::merge_candidates(&self.fleet.entries(), &[lan], &[]);
            if let Some(c) = cands.first() {
                let id = self.fleet.add_candidate(c)?;
                if let Some(n) = name.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
                    self.fleet.rename(&id, n)?;
                }
                return Ok(id);
            }
        }
        Ok(self.fleet.add_host(&host, name.as_deref().map(str::trim).filter(|n| !n.is_empty()))?)
    }

    /// The handle of TV `id`. Handles of one TV share its connection.
    pub fn tv(&self, id: String) -> Res<Arc<TvRemote>> {
        Ok(Arc::new(TvRemote { tv: self.fleet.tv(&id)? }))
    }

    /// Deliver events of every TV to `listener` (replaces the previous listener).
    pub fn set_listener(&self, listener: Box<dyn EventListener>) {
        let gen = self.listener_gen.fetch_add(1, Ordering::SeqCst) + 1;
        let current = self.listener_gen.clone();
        let rx = self.fleet.subscribe();
        let _ = std::thread::Builder::new().name("ffi-events".into()).spawn(move || {
            while current.load(Ordering::SeqCst) == gen {
                match rx.recv_timeout(Duration::from_millis(250)) {
                    Ok(ev) => {
                        if let Some(e) = event(&ev.event) {
                            listener.on_event(ev.tv, e);
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            }
        });
    }

    /// Stop delivering events.
    pub fn clear_listener(&self) {
        self.listener_gen.fetch_add(1, Ordering::SeqCst);
    }

    /// Poll every TV's power over REST every `interval_ms` (emits `Power` on changes).
    /// Call while the app is visible; no WebSocket is opened.
    pub fn start_status_polling(&self, interval_ms: u64) {
        let i = Duration::from_millis(interval_ms.max(1000));
        // The cloud interval is irrelevant: no SmartThings account in this app.
        self.fleet.start_status_polling(i, Duration::from_secs(3600));
    }

    pub fn stop_status_polling(&self) {
        self.fleet.stop_status_polling();
    }
}

// ---------------------------------------------------------------------------------------
// One TV

/// One TV. Every method blocks; call from a background thread.
#[derive(uniffi::Object)]
pub struct TvRemote {
    tv: Tv,
}

#[uniffi::export]
impl TvRemote {
    pub fn id(&self) -> String {
        self.tv.id()
    }

    pub fn name(&self) -> String {
        self.tv.display_name()
    }

    pub fn host(&self) -> String {
        self.tv.host()
    }

    /// Start the remote channel now (otherwise the first command does). On first use
    /// the TV shows its approval prompt.
    pub fn connect(&self) {
        self.tv.connect();
    }

    pub fn disconnect(&self) {
        self.tv.disconnect();
    }

    pub fn connection(&self) -> Connection {
        (&self.tv.connection_state()).into()
    }

    pub fn send_key(&self, key: RemoteKey) -> Res<()> {
        Ok(self.tv.send_key(&key.core())?)
    }

    /// Press or release (for long presses: `Press` on touch down, `Release` on up).
    pub fn send_key_action(&self, key: RemoteKey, action: KeyPress) -> Res<()> {
        Ok(self.tv.send_key_action(&key.core(), action.into())?)
    }

    /// Any key by name or code (`"KEY_PICTURE_SIZE"`, `"VolumeUp"`, `"vol +"`).
    pub fn send_key_code(&self, code: String) -> Res<()> {
        let key: Key = code.parse()?;
        Ok(self.tv.send_key(&key)?)
    }

    /// Press, wait `hold_ms`, release.
    pub fn hold_key(&self, key: RemoteKey, hold_ms: u32) -> Res<()> {
        Ok(self.tv.hold_key(&key.core(), Duration::from_millis(hold_ms.into()))?)
    }

    /// Type into the TV's focused text field.
    pub fn send_text(&self, text: String) -> Res<()> {
        Ok(self.tv.send_text(&text)?)
    }

    /// Move the TV's pointer by (dx, dy) pixels.
    pub fn mouse_move(&self, dx: i32, dy: i32) -> Res<()> {
        Ok(self.tv.mouse_move(dx, dy)?)
    }

    pub fn mouse_click(&self) -> Res<()> {
        Ok(self.tv.mouse_click()?)
    }

    /// The TV's apps found by probing the core's catalog of known app ids (Tizen 2025
    /// has no full list for third parties).
    pub fn installed_apps(&self) -> Res<Vec<AppInfo>> {
        Ok(self.tv.installed_apps()?.into_iter().map(AppInfo::from).collect())
    }

    /// [`TvRemote::installed_apps`], also probing `extra_ids` (apps the user added by id).
    pub fn installed_apps_with(&self, extra_ids: Vec<String>) -> Res<Vec<AppInfo>> {
        let extra: Vec<&str> = extra_ids.iter().map(String::as_str).collect();
        Ok(self.tv.installed_apps_with(&extra)?.into_iter().map(AppInfo::from).collect())
    }

    pub fn launch_app(&self, app: AppInfo) -> Res<()> {
        Ok(self.tv.launch_app(&app.into())?)
    }

    /// Launch an app by id (deep-link launch, as for web apps).
    pub fn launch_app_id(&self, app_id: String) -> Res<()> {
        Ok(self.tv.launch_app_id(&app_id, LaunchType::DeepLink, None)?)
    }

    /// Open a YouTube video (id or URL) over DIAL; falls back to opening the app.
    pub fn youtube(&self, id_or_url: String) -> Res<YouTubeResult> {
        Ok(match self.tv.youtube(&id_or_url)? {
            YouTubeOpened::Video => YouTubeResult::Video,
            YouTubeOpened::AppOnly => YouTubeResult::AppOnly,
        })
    }

    /// Open Netflix over DIAL.
    pub fn netflix(&self) -> Res<()> {
        Ok(self.tv.netflix()?)
    }

    /// REST power state (never fails: unreachable = `Off`).
    pub fn power_state(&self) -> TvPower {
        (&self.tv.power_state()).into()
    }

    pub fn power_off(&self) -> Res<()> {
        Ok(self.tv.power_off()?)
    }

    /// Wake-on-LAN (needs the MAC) or, from network standby without a MAC, one power
    /// key; waits until the TV reports on, up to `timeout_ms`.
    pub fn power_on(&self, timeout_ms: u32) -> Res<()> {
        Ok(self.tv.power_on(Duration::from_millis(timeout_ms.into()))?)
    }

    pub fn volume(&self) -> Res<u8> {
        Ok(self.tv.volume()?)
    }

    pub fn set_volume(&self, volume: u8) -> Res<()> {
        Ok(self.tv.set_volume(volume.min(100))?)
    }

    pub fn muted(&self) -> Res<bool> {
        Ok(self.tv.mute()?)
    }

    pub fn set_muted(&self, muted: bool) -> Res<()> {
        Ok(self.tv.set_mute(muted)?)
    }

    /// Ask the TV for its info; also saves a missing MAC (enables Wake-on-LAN).
    pub fn device_info(&self) -> Res<DeviceSummary> {
        let i = self.tv.device_info()?;
        if self.tv.config().mac.is_none() {
            if let Some(m) = i.wifi_mac.clone().filter(|m| fleet::normalize_mac(m).is_some()) {
                self.tv.update_config(|c| c.mac = Some(m))?;
            }
        }
        Ok(DeviceSummary {
            name: i.name.clone(),
            model_name: i.model_name.clone(),
            resolution: i.resolution.clone(),
            network_type: i.network_type.clone(),
            wifi_mac: i.wifi_mac.clone(),
            ip: i.ip.clone(),
            os: i.os.clone(),
            power: i.power_state.as_ref().map(TvPower::from).unwrap_or(TvPower::On),
        })
    }
}

#[cfg(test)]
mod tests;
