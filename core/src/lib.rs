//! # tvremote_core
//!
//! Blocking, thread-safe control library for a Samsung Tizen TV: remote keys and text
//! over the `samsung.remote.control` WebSocket, the local REST API, DLNA casting (direct
//! or TV-friendly via ffmpeg), network checks, DIAL, Wake-on-LAN, SmartThings, and SSDP.
//!
//! Start with [`Fleet`] (all known TVs, detection, management) and [`Tv`], the
//! per-TV facade used by the GUI and TUI. See `core/API.md` for the threading model
//! and typical call sequences.
//!
//! **Threading:** every method blocks (network I/O with short timeouts), so call them
//! from worker threads, never from a UI main loop. [`Tv`] is `Clone + Send + Sync`;
//! clones share everything. Background activity is reported through [`Tv::subscribe`].

#![warn(missing_docs)]

pub mod cast;
pub mod config;
pub mod dial;
pub mod discovery;
pub mod dlna;
pub mod error;
pub mod events;
pub mod fleet;
mod http;
pub mod keys;
pub mod media;
pub mod netcheck;
pub mod remote;
pub mod rest;
pub mod server;
pub mod smartthings;
pub mod wol;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub use cast::{
    CastEvent, CastMode, CastOutcome, CastProfile, CastRequest, CastSession, CastSource, CastStatus,
};
pub use config::{AppConfig, Config, SharedConfig, TvEntry};
pub use error::{Error, Result};
pub use events::{CloudState, ConnectionState, Event, EventBus, TvEvent};
pub use fleet::{Candidate, Fleet, LanState, MatchKey, Presence, Route, ScanReport, TvHealth};
pub use keys::{Key, KeyAction};
pub use media::{CompatLevel, CompatReport, MediaInfo};
pub use netcheck::{Demand, TvNetwork, Verdict, VerdictLevel};
pub use remote::{App, LaunchType};
pub use rest::{DeviceInfo, PowerState};

/// Crate version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

struct TvInner {
    config: SharedConfig,
    bus: EventBus,
    remote: Mutex<Option<remote::Remote>>,
    cast: Mutex<Option<CastSession>>,
    poll_stop: Mutex<Option<Arc<AtomicBool>>>,
    /// Last device info fetched (by `device_info`, `power_state` or the status poller).
    device_info: Mutex<Option<DeviceInfo>>,
    /// Last REST outcome (answered?, when): the LAN hint for fallback routing.
    lan_seen: Mutex<Option<(bool, Instant)>>,
    /// Last SmartThings health (state, when).
    cloud: Mutex<Option<(CloudState, Instant)>>,
    /// The linked SmartThings device, once fetched.
    st_device: Mutex<Option<smartthings::StDevice>>,
}

/// How long a LAN / cloud observation counts for routing decisions.
const HINT_TTL: Duration = Duration::from_secs(30);
const CLOUD_TTL: Duration = Duration::from_secs(120);

impl Drop for TvInner {
    fn drop(&mut self) {
        if let Ok(mut p) = self.poll_stop.lock() {
            if let Some(f) = p.take() {
                f.store(true, Ordering::SeqCst);
            }
        }
        if let Ok(mut r) = self.remote.lock() {
            if let Some(r) = r.take() {
                r.shutdown();
            }
        }
    }
}

/// Tizen app ids probed by [`Tv::installed_apps`] (id, fallback name). The TV's own
/// name wins when it reports one. Add ids here to make more apps show up.
pub const KNOWN_APPS: &[(&str, &str)] = &[
    ("111299001912", "YouTube"),
    ("3201907018807", "Netflix"),
    ("3201910019365", "Prime Video"),
    ("3201901017640", "Disney+"),
    ("3201807016597", "Apple TV"),
    ("3201606009684", "Spotify"),
    ("3202010022079", "Browser"),
    ("org.tizen.browser", "Internet"),
    ("3201512006963", "Plex"),
    ("3202203026841", "Twitch"),
    ("3201812017479", "myCANAL"),
    ("3201911019629", "france.tv"),
    ("3201709014773", "Molotov"),
    ("3201608010191", "Deezer"),
    ("3202301029760", "Max"),
    ("3201710015067", "Paramount+"),
    ("3201908019041", "Crunchyroll"),
    ("3201906018702", "Kodi"),
    ("3201601007230", "ARTE"),
    ("3201712015183", "TF1+"),
    ("3201801015366", "M6+"),
    ("3201702011851", "Steam Link"),
    ("3202203026723", "Xbox"),
    ("3201906018693", "Gaming Hub"),
    ("3201712015217", "SmartThings"),
];

/// The TV. Cheap to clone (all clones share state); `Send + Sync`.
///
/// Creating a `Tv` does no network I/O. The remote-control channel starts on
/// [`Tv::connect`] or on the first remote command.
#[derive(Clone)]
pub struct Tv {
    inner: Arc<TvInner>,
}

impl std::fmt::Debug for Tv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tv").field("id", &self.id()).field("host", &self.host()).finish()
    }
}

impl Tv {
    // ----- construction / config ---------------------------------------------------

    /// Load `~/.config/samsung-tv/config.json`; changes (new token, settings) are saved back.
    ///
    /// See [`Tv::open_or_default`]: a missing file is not an error (you get
    /// [`Config::default()`], with an empty `host`, and the file is created on the first
    /// save), but a file that exists and cannot be read or parsed is
    /// ([`Error::Config`]), so it is never overwritten. Check `tv.host().is_empty()` to
    /// detect the first run.
    pub fn open_default() -> Result<Tv> {
        Self::open_or_default(&Config::default_path())
    }

    /// Like [`Tv::open`], but a missing file gives [`Config::default()`] bound to `path`
    /// (the file and its directory are created on the first save). A file that exists
    /// but cannot be read or parsed is still an [`Error::Config`]: nothing is
    /// overwritten. To carry on anyway, use [`Tv::new`] (in memory, never saved).
    ///
    /// The handle is bound to the TV that is active when the file is opened (see
    /// [`Fleet`] to manage several TVs).
    pub fn open_or_default(path: &Path) -> Result<Tv> {
        Ok(Self::bind_active(AppConfig::load_or_default(path)?, path))
    }

    fn bind_active(app: AppConfig, path: &Path) -> Tv {
        let active = app.active.clone();
        let shared = SharedConfig::from_app(app, Some(path.to_path_buf()));
        Self::from_shared(match active {
            Some(id) => shared.scoped(&id),
            None => shared,
        })
    }

    /// Load a config file; changes are saved back to it. Fails if the file is missing
    /// (use [`Tv::open_or_default`] to start from defaults instead).
    pub fn open(path: &Path) -> Result<Tv> {
        Ok(Self::bind_active(AppConfig::load_from(path)?, path))
    }

    /// In-memory config (nothing is persisted).
    pub fn new(config: Config) -> Tv {
        Self::from_shared(SharedConfig::new(config, None))
    }

    /// From an existing shared config (scoped to one TV, or following the active one).
    pub fn from_shared(config: SharedConfig) -> Tv {
        Self::with_bus(config, EventBus::new())
    }

    /// From a shared config and an event bus (the [`Fleet`] passes a tagged bus).
    pub fn with_bus(config: SharedConfig, bus: EventBus) -> Tv {
        Tv {
            inner: Arc::new(TvInner {
                config,
                bus,
                remote: Mutex::new(None),
                cast: Mutex::new(None),
                poll_stop: Mutex::new(None),
                device_info: Mutex::new(None),
                lan_seen: Mutex::new(None),
                cloud: Mutex::new(None),
                st_device: Mutex::new(None),
            }),
        }
    }

    /// Local id of this TV in the config (`tv1`, …); empty before the first TV exists.
    pub fn id(&self) -> String {
        self.inner.config.tv_id().unwrap_or_default()
    }

    /// Display name of this TV.
    pub fn display_name(&self) -> String {
        self.config().display_name
    }

    /// True if both handles are clones of the same TV handle (shared connection).
    pub fn ptr_eq(&self, other: &Tv) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    /// Snapshot of the configuration. It contains tokens: `Config`'s `Debug` redacts
    /// them, but never print the fields yourself.
    pub fn config(&self) -> Config {
        self.inner.config.get()
    }

    /// The shared config handle.
    pub fn shared_config(&self) -> SharedConfig {
        self.inner.config.clone()
    }

    /// Modify and save the config; emits [`Event::ConfigChanged`]. If the host, port,
    /// name or token changed, a running remote channel reconnects.
    pub fn update_config(&self, f: impl FnOnce(&mut Config)) -> Result<()> {
        let before = self.config();
        self.inner.config.update(f)?;
        let after = self.config();
        if before.smartthings_device_id != after.smartthings_device_id
            || before.smartthings_token != after.smartthings_token
        {
            self.forget_cloud_cache();
        }
        self.inner.bus.emit(Event::ConfigChanged);
        let changed = before.host != after.host
            || before.port != after.port
            || before.name != after.name
            || before.token != after.token;
        if changed {
            if let Ok(g) = self.inner.remote.lock() {
                if let Some(r) = g.as_ref() {
                    r.reconnect();
                }
            }
        }
        Ok(())
    }

    /// TV host (IP).
    pub fn host(&self) -> String {
        self.inner.config.get().host
    }

    /// Subscribe to background events (connection, power, token, cast, config).
    pub fn subscribe(&self) -> Receiver<Event> {
        self.inner.bus.subscribe()
    }

    /// The event bus (to emit your own events or pass to lower-level APIs).
    pub fn event_bus(&self) -> EventBus {
        self.inner.bus.clone()
    }

    // ----- low-level clients ----------------------------------------------------------

    /// REST client for the current host.
    pub fn rest(&self) -> rest::Rest {
        rest::Rest::new(&self.host())
    }

    /// DLNA client for the current host.
    pub fn dlna(&self) -> dlna::Dlna {
        dlna::Dlna::new(&self.host())
    }

    /// DIAL client for the current host.
    pub fn dial(&self) -> dial::Dial {
        dial::Dial::new(&self.host())
    }

    // ----- remote channel ---------------------------------------------------------------

    fn remote(&self) -> remote::Remote {
        let mut g = match self.inner.remote.lock() {
            Ok(g) => g,
            Err(e) => e.into_inner(),
        };
        g.get_or_insert_with(|| remote::Remote::spawn(self.inner.config.clone(), self.inner.bus.clone()))
            .clone()
    }

    /// Start the remote-control channel thread (non-blocking, idempotent). Watch
    /// [`Event::Connection`] for progress. Without a token the TV shows an approval prompt.
    pub fn connect(&self) {
        let _ = self.remote();
    }

    /// Stop the remote-control channel.
    pub fn disconnect(&self) {
        if let Ok(mut g) = self.inner.remote.lock() {
            if let Some(r) = g.take() {
                r.shutdown();
            }
        }
    }

    /// Current remote channel state ([`ConnectionState::Disconnected`] if not started).
    pub fn connection_state(&self) -> ConnectionState {
        self.inner
            .remote
            .lock()
            .ok()
            .and_then(|g| g.as_ref().map(|r| r.state()))
            .unwrap_or(ConnectionState::Disconnected)
    }

    /// Click a key (starts the channel if needed).
    pub fn send_key(&self, key: &Key) -> Result<()> {
        self.remote().send_key(key, KeyAction::Click)
    }

    /// Send a key with an explicit action (Press / Release for long presses).
    pub fn send_key_action(&self, key: &Key, action: KeyAction) -> Result<()> {
        self.remote().send_key(key, action)
    }

    /// Long-press: Press, wait `hold`, Release.
    pub fn hold_key(&self, key: &Key, hold: Duration) -> Result<()> {
        let r = self.remote();
        r.send_key(key, KeyAction::Press)?;
        std::thread::sleep(hold);
        r.send_key(key, KeyAction::Release)
    }

    /// Type text into the focused field on the TV.
    pub fn send_text(&self, text: &str) -> Result<()> {
        self.remote().send_text(text)
    }

    /// Move the pointer by (dx, dy).
    pub fn mouse_move(&self, dx: i32, dy: i32) -> Result<()> {
        self.remote().mouse_move(dx, dy)
    }

    /// Left click.
    pub fn mouse_click(&self) -> Result<()> {
        self.remote().mouse_click()
    }

    /// Installed apps.
    ///
    /// Tizen 2025 firmware ignores `ed.installedApp.get` from third-party clients, so this
    /// first asks REST about every id in [`KNOWN_APPS`] (in parallel, ~1 s) and keeps the
    /// ones the TV reports. Only if none is found does it fall back to the remote channel
    /// (older firmware; up to ~10 s).
    pub fn installed_apps(&self) -> Result<Vec<App>> {
        let found = self.probe_known_apps();
        if !found.is_empty() {
            return Ok(found);
        }
        self.remote().installed_apps()
    }

    fn probe_known_apps(&self) -> Vec<App> {
        let handles: Vec<_> = KNOWN_APPS
            .iter()
            .map(|&(id, fallback_name)| {
                let rest = self.rest();
                std::thread::spawn(move || {
                    rest.app_status(id).ok().map(|st| App {
                        app_id: id.to_string(),
                        name: st.name.unwrap_or_else(|| fallback_name.to_string()),
                        app_type: 2,
                        icon: None,
                    })
                })
            })
            .collect();
        let mut apps: Vec<App> = handles
            .into_iter()
            .filter_map(|h| h.join().ok().flatten())
            .collect();
        apps.sort_by_key(|a| a.name.to_lowercase());
        apps
    }

    /// Launch an installed app: REST `POST /api/v2/applications/<id>` first (works for
    /// every app the REST probe finds), then the remote channel with its launch type.
    pub fn launch_app(&self, app: &App) -> Result<()> {
        match self.rest().launch_app(&app.app_id) {
            Ok(()) => Ok(()),
            Err(_) => self.remote().launch_app(&app.app_id, app.launch_type(), None),
        }
    }

    /// Launch an app by id, with a launch type and optional deep-link `meta_tag`.
    pub fn launch_app_id(&self, app_id: &str, kind: LaunchType, meta_tag: Option<&str>) -> Result<()> {
        self.remote().launch_app(app_id, kind, meta_tag)
    }

    /// App status via REST.
    pub fn app_status(&self, app_id: &str) -> Result<rest::AppStatus> {
        self.rest().app_status(app_id)
    }

    /// Close an app via REST.
    pub fn close_app(&self, app_id: &str) -> Result<()> {
        self.rest().close_app(app_id)
    }

    // ----- device / power ---------------------------------------------------------------

    /// `GET /api/v2/` device info. A successful result is also cached (see
    /// [`Tv::cached_device_info`]).
    pub fn device_info(&self) -> Result<DeviceInfo> {
        if self.host().trim().is_empty() {
            return Err(Error::Config("no LAN address for this TV".into()));
        }
        let res = self.rest().device_info();
        if let Ok(mut g) = self.inner.lan_seen.lock() {
            *g = Some((res.is_ok(), Instant::now()));
        }
        let info = res?;
        if let Ok(mut g) = self.inner.device_info.lock() {
            *g = Some(info.clone());
        }
        Ok(info)
    }

    /// What is known about the LAN path (from the last REST request, no I/O).
    pub fn lan_reach(&self) -> fleet::Reach {
        if self.host().trim().is_empty() {
            return fleet::Reach::No;
        }
        match self.inner.lan_seen.lock().ok().and_then(|g| *g) {
            Some((ok, t)) if t.elapsed() < HINT_TTL => {
                if ok {
                    fleet::Reach::Yes
                } else {
                    fleet::Reach::No
                }
            }
            _ => fleet::Reach::Unknown,
        }
    }

    /// What is known about the cloud path (no I/O): `None` if no device is linked or
    /// no token is set.
    pub fn cloud_reach(&self) -> Option<fleet::Reach> {
        if !self.has_smartthings() || self.config().smartthings_device_id.is_none() {
            return None;
        }
        Some(match self.inner.cloud.lock().ok().and_then(|g| g.clone()) {
            Some((CloudState::Online, t)) if t.elapsed() < CLOUD_TTL => fleet::Reach::Yes,
            Some((CloudState::Offline, t)) if t.elapsed() < CLOUD_TTL => fleet::Reach::No,
            _ => fleet::Reach::Unknown,
        })
    }

    /// The last cloud state seen (no I/O).
    pub fn cached_cloud_state(&self) -> CloudState {
        if self.cloud_reach().is_none() {
            return CloudState::NotLinked;
        }
        self.inner
            .cloud
            .lock()
            .ok()
            .and_then(|g| g.clone())
            .map(|(s, _)| s)
            .unwrap_or(CloudState::Unknown("not checked yet".into()))
    }

    /// Forget the cached SmartThings device and state (after a link change).
    pub fn forget_cloud_cache(&self) {
        if let Ok(mut g) = self.inner.st_device.lock() {
            *g = None;
        }
        if let Ok(mut g) = self.inner.cloud.lock() {
            *g = None;
        }
    }

    /// Ask SmartThings whether this TV is online (`GET /devices/{id}/health`), cache it,
    /// and emit [`Event::Cloud`] when it changed. [`CloudState::NotLinked`] without a
    /// token or a linked device (no I/O then).
    pub fn cloud_state(&self) -> CloudState {
        let Some(dev) = self.config().smartthings_device_id else { return CloudState::NotLinked };
        let st = match self.smartthings() {
            Ok(st) => st,
            Err(_) => return CloudState::NotLinked,
        };
        let state = match st.health(&dev) {
            Ok(h) if h.is_online() => CloudState::Online,
            Ok(_) => CloudState::Offline,
            Err(e) => CloudState::Unknown(e.to_string()),
        };
        let prev = self.inner.cloud.lock().ok().and_then(|mut g| g.replace((state.clone(), Instant::now())));
        if prev.map(|(s, _)| s) != Some(state.clone()) {
            self.inner.bus.emit(Event::Cloud(state.clone()));
        }
        state
    }

    /// Status of this TV: LAN reachability (REST), SmartThings health, power state
    /// (REST when reachable, else SmartThings `switch` when online). Blocks a few seconds
    /// at most.
    pub fn health(&self) -> fleet::TvHealth {
        let me = self.clone();
        let cloud_h = std::thread::spawn(move || me.cloud_state());
        let (lan, mut power) = if self.host().trim().is_empty() {
            (LanState::NoHost, PowerState::Off)
        } else {
            match self.device_info() {
                Ok(i) => (LanState::Reachable, i.power_state.unwrap_or(PowerState::On)),
                Err(_) => (LanState::Unreachable, PowerState::Off),
            }
        };
        let cloud = cloud_h.join().unwrap_or(CloudState::Unknown("crashed".into()));
        if lan != LanState::Reachable && cloud.is_online() {
            if let Ok(s) = self.smartthings_status() {
                power = match s.power_on {
                    Some(true) => PowerState::On,
                    Some(false) => PowerState::Standby,
                    None => power,
                };
            }
        }
        fleet::TvHealth { id: self.id(), name: self.display_name(), lan, cloud, power }
    }

    /// The last device info fetched by [`Tv::device_info`], [`Tv::power_state`] or the
    /// status poller ([`Tv::start_status_polling`]). No I/O; `None` until one succeeded.
    pub fn cached_device_info(&self) -> Option<DeviceInfo> {
        self.inner.device_info.lock().ok().and_then(|g| g.clone())
    }

    /// How the TV is connected according to the cached device info (no I/O); `None`
    /// if unknown. Pass it to [`Tv::network_verdict`].
    pub fn tv_network(&self) -> Option<netcheck::TvNetwork> {
        self.cached_device_info().and_then(|d| d.tv_network())
    }

    /// Power state (unreachable = [`PowerState::Off`]; never errors). Refreshes the
    /// device-info cache.
    pub fn power_state(&self) -> PowerState {
        match self.device_info() {
            Ok(info) => info.power_state.unwrap_or(PowerState::On),
            Err(_) => PowerState::Off,
        }
    }

    /// Turn the TV off (sends `KEY_POWER` if the screen is on; no-op otherwise).
    ///
    /// With a linked SmartThings device, falls back to (or, when the LAN is known to be
    /// unreachable, starts with) the cloud `switch.off` command (see
    /// [`fleet::plan_routes`]).
    pub fn power_off(&self) -> Result<()> {
        self.routed(fleet::Op::PowerOff, |tv| {
            match tv.device_info() {
                Ok(i) if i.power_state.clone().unwrap_or(PowerState::On).is_on() => {
                    tv.send_key(&Key::Power)?;
                    tv.inner.bus.emit(Event::Power(PowerState::Standby));
                    Ok(())
                }
                Ok(_) => Ok(()),
                // Unreachable: already off, unless SmartThings can tell otherwise.
                Err(e) if tv.cloud_reach().is_some() => Err(e),
                Err(_) => Ok(()),
            }
        }, |tv| {
            let (st, dev) = tv.st_pair()?;
            st.set_power(&dev, false)?;
            tv.inner.bus.emit(Event::Power(PowerState::Standby));
            Ok(())
        })
    }

    /// Run `op` over the routes [`fleet::plan_routes`] picks; the first success wins.
    /// Errors from every route tried are combined.
    fn routed<T>(
        &self,
        op: fleet::Op,
        lan: impl Fn(&Tv) -> Result<T>,
        cloud: impl Fn(&Tv) -> Result<T>,
    ) -> Result<T> {
        let routes = fleet::plan_routes(op, self.lan_reach(), self.cloud_reach());
        let mut errors: Vec<(Route, Error)> = Vec::new();
        for r in routes {
            let res = match r {
                Route::Lan => lan(self),
                Route::Cloud => cloud(self),
            };
            match res {
                Ok(v) => return Ok(v),
                Err(e) => errors.push((r, e)),
            }
        }
        match errors.len() {
            0 => Err(Error::Unsupported("no route to the TV".into())),
            1 => Err(errors.pop().map(|(_, e)| e).unwrap_or(Error::NotConnected)),
            _ => {
                let msg = errors
                    .iter()
                    .map(|(r, e)| format!("{}: {e}", if *r == Route::Lan { "LAN" } else { "SmartThings" }))
                    .collect::<Vec<_>>()
                    .join("; ");
                Err(Error::Http(msg))
            }
        }
    }

    fn st_pair(&self) -> Result<(smartthings::SmartThings, smartthings::StDevice)> {
        Ok((self.smartthings()?, self.linked_smartthings_device()?))
    }

    /// Turn the TV on: Wake-on-LAN (repeated every 2 s, needs `mac` in the config) and,
    /// if the TV is in network standby, one `KEY_POWER` over the remote channel. Polls
    /// the REST power state until it is on or `timeout` elapses.
    ///
    /// With a linked SmartThings device, the cloud `switch.on` command is also sent:
    /// first when the LAN is known to be unreachable and the cloud is reachable, else
    /// after 4 s without success. Success is then also detected through SmartThings.
    pub fn power_on(&self, timeout: Duration) -> Result<()> {
        let state = self.power_state();
        if state.is_on() {
            return Ok(());
        }
        let routes = fleet::plan_routes(fleet::Op::PowerOn, self.lan_reach(), self.cloud_reach());
        let use_cloud = routes.contains(&Route::Cloud);
        let cloud_first = routes.first() == Some(&Route::Cloud);
        let mac = self.config().mac;
        if mac.is_none() && state != PowerState::Standby && !use_cloud {
            return Err(Error::Config("no MAC address configured for Wake-on-LAN".into()));
        }
        let start = Instant::now();
        let deadline = start + timeout;
        let mut last_wol = None::<Instant>;
        let mut tried_key = false;
        let mut cloud_sent = false;
        let mut last_cloud_check = None::<Instant>;
        let mut cloud_err: Option<Error> = None;
        loop {
            if use_cloud && !cloud_sent && (cloud_first || start.elapsed() >= Duration::from_secs(4)) {
                cloud_sent = true;
                if let Err(e) = self.st_pair().and_then(|(st, dev)| st.set_power(&dev, true)) {
                    cloud_err = Some(e);
                }
            }
            if let Some(m) = &mac {
                if last_wol.is_none_or(|t| t.elapsed() >= Duration::from_secs(2)) {
                    if let Err(e) = wol::send_magic_packet(m, &[]) {
                        if !use_cloud {
                            return Err(e);
                        }
                    }
                    last_wol = Some(Instant::now());
                }
            }
            let st = if self.host().trim().is_empty() { PowerState::Off } else { self.power_state() };
            if st.is_on() {
                self.inner.bus.emit(Event::Power(PowerState::On));
                return Ok(());
            }
            if st == PowerState::Standby && !tried_key {
                tried_key = true;
                let _ = self.send_key(&Key::Power);
            }
            // LAN silent: ask SmartThings whether it worked (every 3 s).
            if cloud_sent && cloud_err.is_none() && st == PowerState::Off
                && last_cloud_check.is_none_or(|t| t.elapsed() >= Duration::from_secs(3))
            {
                last_cloud_check = Some(Instant::now());
                if self.smartthings_status().ok().and_then(|s| s.power_on) == Some(true) {
                    self.inner.bus.emit(Event::Power(PowerState::On));
                    return Ok(());
                }
            }
            if Instant::now() >= deadline {
                return Err(match cloud_err {
                    Some(e) if mac.is_none() => e,
                    _ => Error::Timeout("the TV did not turn on".into()),
                });
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    }

    /// Start a background thread that polls the power state every `interval` and emits
    /// [`Event::Power`] on changes (and nudges the remote channel to reconnect when the
    /// TV comes on). Replaces any previous poller; stops when the last `Tv` is dropped.
    pub fn start_status_polling(&self, interval: Duration) {
        self.stop_status_polling();
        let flag = Arc::new(AtomicBool::new(false));
        if let Ok(mut g) = self.inner.poll_stop.lock() {
            *g = Some(flag.clone());
        }
        let weak = Arc::downgrade(&self.inner);
        let _ = std::thread::Builder::new()
            .name("tv-status-poll".into())
            .spawn(move || {
                let mut last: Option<PowerState> = None;
                while !flag.load(Ordering::SeqCst) {
                    let Some(inner) = weak.upgrade() else { return };
                    let tv = Tv { inner };
                    let st = if tv.host().trim().is_empty() { PowerState::Off } else { tv.power_state() };
                    if last.as_ref() != Some(&st) {
                        if st.is_on() && last.is_some() {
                            if let Ok(g) = tv.inner.remote.lock() {
                                if let Some(r) = g.as_ref() {
                                    if !r.is_connected() {
                                        r.reconnect();
                                    }
                                }
                            }
                        }
                        tv.inner.bus.emit(Event::Power(st.clone()));
                        last = Some(st);
                    }
                    drop(tv);
                    let until = Instant::now() + interval;
                    while Instant::now() < until && !flag.load(Ordering::SeqCst) {
                        std::thread::sleep(Duration::from_millis(100));
                    }
                }
            });
    }

    /// True while [`Tv::start_status_polling`] runs.
    pub fn is_polling(&self) -> bool {
        self.inner.poll_stop.lock().map(|g| g.is_some()).unwrap_or(false)
    }

    /// Stop the status poller.
    pub fn stop_status_polling(&self) {
        if let Ok(mut g) = self.inner.poll_stop.lock() {
            if let Some(f) = g.take() {
                f.store(true, Ordering::SeqCst);
            }
        }
    }

    // ----- volume (DLNA RenderingControl) -----------------------------------------------

    // These four go over DLNA and fall back to SmartThings (`audioVolume` /
    // `audioMute`) when a device is linked, or start with SmartThings when the LAN is
    // known to be unreachable (see [`fleet::plan_routes`]).

    /// Volume 0-100.
    pub fn volume(&self) -> Result<u8> {
        self.routed(fleet::Op::GetVolume, |tv| tv.lan_dlna()?.volume(), |tv| {
            tv.smartthings_status()?.volume.ok_or_else(|| Error::Unsupported("SmartThings reports no volume".into()))
        })
    }

    /// Set volume 0-100.
    pub fn set_volume(&self, v: u8) -> Result<()> {
        self.routed(fleet::Op::SetVolume, |tv| tv.lan_dlna()?.set_volume(v), |tv| {
            let (st, dev) = tv.st_pair()?;
            st.set_volume(&dev, v)
        })
    }

    /// Mute state.
    pub fn mute(&self) -> Result<bool> {
        self.routed(fleet::Op::GetMute, |tv| tv.lan_dlna()?.mute(), |tv| {
            tv.smartthings_status()?.muted.ok_or_else(|| Error::Unsupported("SmartThings reports no mute state".into()))
        })
    }

    /// Set mute.
    pub fn set_mute(&self, mute: bool) -> Result<()> {
        self.routed(fleet::Op::SetMute, |tv| tv.lan_dlna()?.set_mute(mute), |tv| {
            let (st, dev) = tv.st_pair()?;
            st.set_mute(&dev, mute)
        })
    }

    fn lan_dlna(&self) -> Result<dlna::Dlna> {
        if self.host().trim().is_empty() {
            return Err(Error::Config("no LAN address for this TV".into()));
        }
        Ok(self.dlna())
    }

    // ----- DIAL ------------------------------------------------------------------------

    /// Open a YouTube video (id or URL) via DIAL.
    pub fn youtube(&self, id_or_url: &str) -> Result<()> {
        self.dial().youtube(id_or_url)
    }

    /// Launch Netflix via DIAL.
    pub fn netflix(&self) -> Result<()> {
        self.dial().netflix()
    }

    // ----- casting ---------------------------------------------------------------------

    /// ffprobe a local file.
    pub fn probe_media(&self, path: &Path) -> Result<MediaInfo> {
        media::probe(path)
    }

    /// Start a cast; the previous session (if any) is stopped first. Blocks until the
    /// TV accepted `Play` (up to ~15 s). Progress arrives as [`Event::Cast`].
    pub fn cast(&self, req: CastRequest) -> Result<CastSession> {
        if let Some(old) = self.current_cast() {
            let _ = old.stop();
        }
        let s = cast::start_cast(&self.dlna(), req, self.inner.bus.clone())?;
        if let Ok(mut g) = self.inner.cast.lock() {
            *g = Some(s.clone());
        }
        Ok(s)
    }

    /// The running cast session, if any.
    pub fn current_cast(&self) -> Option<CastSession> {
        self.inner
            .cast
            .lock()
            .ok()
            .and_then(|g| g.clone())
            .filter(|s| !s.is_finished())
    }

    /// Transport state and position of whatever the TV's DLNA renderer is playing
    /// (also works for casts started elsewhere, e.g. by `tv-cast`).
    pub fn transport(&self) -> Result<(dlna::TransportInfo, dlna::PositionInfo)> {
        let d = self.dlna();
        Ok((d.transport_info()?, d.position_info()?))
    }

    // ----- network check ----------------------------------------------------------------

    /// Start the background packet analysis for a file (see [`netcheck::analyze_in_background`]).
    pub fn analyze_demand(
        &self,
        info: &MediaInfo,
        on_progress: impl Fn(f32) + Send + 'static,
        on_done: impl FnOnce(Result<Arc<netcheck::PacketProfile>>) + Send + 'static,
    ) -> netcheck::DemandJob {
        netcheck::analyze_in_background(info.path.clone(), info.duration_secs, on_progress, on_done)
    }

    /// Verdict for casting `info` with `mode`: uses the packet profile if given (else the
    /// cache, else a provisional estimate), the cached capacity measurement, the
    /// current link, and how the TV is connected (`tv`: usually [`Tv::tv_network`];
    /// `None` = unknown, which assumes Wi-Fi with hedged wording). Local only (no TV
    /// traffic).
    pub fn network_verdict(
        &self,
        info: &MediaInfo,
        mode: &CastMode,
        packets: Option<&netcheck::PacketProfile>,
        tv: Option<netcheck::TvNetwork>,
    ) -> (Demand, Verdict) {
        let cached;
        let packets = match packets {
            Some(p) => Some(p),
            None => {
                cached = netcheck::load_cached_profile(&info.path);
                cached.as_ref()
            }
        };
        let demand = netcheck::compute_demand(info, mode, packets);
        let cap = netcheck::load_capacity();
        let link = netcheck::detect_link();
        let v = netcheck::verdict(&demand, cap.as_ref(), link.as_ref(), tv);
        (demand, v)
    }

    /// Run the network capacity test (casts a test pattern for ~10 s, then stops it).
    /// Any running cast is stopped first.
    ///
    /// Cancel it by setting `cancel` (e.g. an `Arc<AtomicBool>` shared with the UI
    /// thread, like [`netcheck::DemandJob::cancel`]): it then returns
    /// [`Error::Cancelled`] within ~0.5 s, stops the test cast on the TV and saves nothing.
    pub fn measure_network(
        &self,
        cancel: &AtomicBool,
        progress: &dyn Fn(netcheck::CapacityProgress),
    ) -> Result<netcheck::CapacityMeasurement> {
        if let Some(old) = self.current_cast() {
            let _ = old.stop();
        }
        netcheck::measure_capacity(&self.dlna(), cancel, progress)
    }

    // ----- SmartThings ------------------------------------------------------------------

    /// True if a SmartThings token is configured.
    pub fn has_smartthings(&self) -> bool {
        self.config().smartthings_token.is_some_and(|t| !t.trim().is_empty())
    }

    /// SmartThings client ([`Error::SmartThings`] if no token is configured).
    pub fn smartthings(&self) -> Result<smartthings::SmartThings> {
        let tok = self
            .config()
            .smartthings_token
            .ok_or_else(|| Error::SmartThings("no SmartThings token configured".into()))?;
        smartthings::SmartThings::new(&tok)
    }

    /// This TV's SmartThings device: the linked `smartthings_device_id`; else a TV of
    /// the account found by matching (see [`fleet::merge_candidates`]) against this TV's
    /// cached REST info or config; else, when both the account and the config hold a
    /// single TV, that one (the single-TV behaviour). A device found this way is linked
    /// (saved). Devices this TV unlinked are never picked.
    pub fn smartthings_tv(&self) -> Result<smartthings::StDevice> {
        let st = self.smartthings()?;
        let cfg = self.config();
        if let Some(id) = &cfg.smartthings_device_id {
            let dev = st.device(id)?;
            if let Ok(mut g) = self.inner.st_device.lock() {
                *g = Some(dev.clone());
            }
            return Ok(dev);
        }
        let app = self.inner.config.app();
        let me = app.entry(&cfg.id).cloned();
        let taken: Vec<String> = app.tvs.iter().filter(|e| e.id != cfg.id).filter_map(|e| e.smartthings_device_id.clone()).collect();
        let unlinked = me.as_ref().map(|e| e.smartthings_unlinked.clone()).unwrap_or_default();
        let clouds: Vec<fleet::CloudTv> = st
            .tvs()?
            .into_iter()
            .filter(|d| !taken.contains(&d.device_id) && !unlinked.contains(&d.device_id))
            .map(|device| fleet::CloudTv { device, health: None })
            .collect();
        // Match against what we know about this TV on the LAN.
        let info = self.cached_device_info().or_else(|| self.device_info().ok()).or_else(|| {
            let v = serde_json::json!({"device": {"modelName": cfg.model, "wifiMac": cfg.mac, "ip": cfg.host}});
            Some(DeviceInfo::from_json(v))
        });
        let lan = fleet::LanTv { ip: cfg.host.clone(), ssdp: None, info };
        let cands = fleet::merge_candidates(&[], std::slice::from_ref(&lan), &clouds);
        let picked = cands
            .iter()
            .find(|c| c.presence == Presence::Both)
            .and_then(|c| c.cloud.clone())
            .map(|c| c.device)
            .or_else(|| (clouds.len() == 1 && app.tvs.len() <= 1).then(|| clouds[0].device.clone()));
        let dev = picked.ok_or_else(|| {
            Error::SmartThings(if clouds.is_empty() {
                "no (unlinked) TV found in this SmartThings account".into()
            } else {
                "several SmartThings TVs: link one to this TV (Device page or `tvs link`)".into()
            })
        })?;
        let id = dev.device_id.clone();
        self.update_config(|c| c.smartthings_device_id = Some(id))?;
        if let Ok(mut g) = self.inner.st_device.lock() {
            *g = Some(dev.clone());
        }
        Ok(dev)
    }

    /// The linked SmartThings device (cached after the first fetch); never auto-links.
    pub fn linked_smartthings_device(&self) -> Result<smartthings::StDevice> {
        let Some(id) = self.config().smartthings_device_id else {
            return Err(Error::SmartThings("no SmartThings device linked to this TV".into()));
        };
        if let Some(d) = self.inner.st_device.lock().ok().and_then(|g| g.clone()).filter(|d| d.device_id == id) {
            return Ok(d);
        }
        let dev = self.smartthings()?.device(&id)?;
        if let Ok(mut g) = self.inner.st_device.lock() {
            *g = Some(dev.clone());
        }
        Ok(dev)
    }

    /// SmartThings status of the linked device.
    pub fn smartthings_status(&self) -> Result<smartthings::TvStatus> {
        let Some(id) = self.config().smartthings_device_id else {
            return Err(Error::SmartThings("no SmartThings device linked to this TV".into()));
        };
        self.smartthings()?.status(&id)
    }

    // ----- discovery ----------------------------------------------------------------------

    /// SSDP search for this TV; if found at a different IP, the host is updated and
    /// saved. Returns the TV's IP if found.
    ///
    /// With a MAC configured, the TV is the one whose REST `wifiMac` matches; without,
    /// the only Samsung TV that answered (if exactly one did).
    pub fn rediscover(&self, timeout: Duration) -> Result<Option<String>> {
        let found = Fleet::scan_lan(timeout)?;
        let mac = self.config().mac.as_deref().and_then(fleet::normalize_mac);
        let me = match mac {
            Some(m) => found.iter().find(|l| l.mac().and_then(fleet::normalize_mac).as_deref() == Some(m.as_str())),
            None if found.len() == 1 => found.first(),
            None => found.iter().find(|l| l.ip == self.host()),
        };
        let Some(l) = me else { return Ok(None) };
        let ip = l.ip.clone();
        if ip != self.host() {
            let ip2 = ip.clone();
            self.update_config(|c| c.host = ip2)?;
        }
        Ok(Some(ip))
    }
}

/// Default location of the config file.
pub fn default_config_path() -> PathBuf {
    Config::default_path()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn facade_is_thread_safe_and_offline() {
        assert_send_sync::<Tv>();
        assert_send_sync::<CastSession>();
        assert_send_sync::<remote::Remote>();
        assert_send_sync::<server::FileServer>();
        let tv = Tv::new(Config { host: "192.0.2.1".into(), ..Default::default() });
        assert_eq!(tv.connection_state(), ConnectionState::Disconnected);
        assert!(!tv.has_smartthings());
        assert!(tv.current_cast().is_none());
        let rx = tv.subscribe();
        tv.update_config(|c| c.name = "Test".into()).unwrap();
        assert!(matches!(rx.try_recv(), Ok(Event::ConfigChanged)));
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("tvremote-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn open_or_default_creates_on_first_save() {
        let dir = temp_dir("open-missing");
        let path = dir.join("sub").join("config.json");
        let tv = Tv::open_or_default(&path).unwrap();
        assert!(tv.host().is_empty());
        assert!(!path.exists(), "nothing is written before the first save");
        tv.update_config(|c| c.host = "192.0.2.7".into()).unwrap();
        assert_eq!(Config::load_from(&path).unwrap().host, "192.0.2.7");
        assert!(Tv::open(&dir.join("nope.json")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn legacy_file_keeps_its_token_through_tv_and_fleet() {
        let dir = temp_dir("legacy");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        std::fs::write(
            &path,
            r#"{"host":"192.0.2.169","port":8002,"name":"Ryoku","token":"22222222","model":"QE85Q7FAAUXXC","mac":"AA:BB:CC:DD:EE:01"}"#,
        )
        .unwrap();
        let tv = Tv::open(&path).unwrap();
        assert_eq!(tv.id(), "tv1");
        let cfg = tv.config();
        assert_eq!((cfg.host.as_str(), cfg.token.as_deref()), ("192.0.2.169", Some("22222222")));
        // The remote channel would connect with exactly this URL: no new approval prompt.
        assert!(remote::ws_url(&cfg.host, cfg.port, &cfg.name, cfg.token.as_deref()).ends_with("&token=22222222"));
        tv.update_config(|c| c.display_name = "TV du salon".into()).unwrap();
        let app = AppConfig::load_from(&path).unwrap();
        assert_eq!(app.tvs.len(), 1);
        assert_eq!(app.tvs[0].name, "TV du salon");
        assert_eq!(app.tvs[0].token.as_deref(), Some("22222222"));
        let fleet = Fleet::open(&path).unwrap();
        assert_eq!(fleet.active().config().token.as_deref(), Some("22222222"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fallback_without_cloud_stays_on_the_lan() {
        let tv = Tv::new(Config::default());
        assert_eq!(tv.cloud_reach(), None);
        assert_eq!(tv.cached_cloud_state(), CloudState::NotLinked);
        assert_eq!(tv.lan_reach(), fleet::Reach::No, "no host");
        // No host and no cloud: fails fast with the LAN error, no I/O.
        assert!(matches!(tv.set_volume(10), Err(Error::Config(_))));
        assert_eq!(tv.cloud_state(), CloudState::NotLinked);
    }

    #[test]
    fn open_or_default_refuses_a_broken_file() {
        let dir = temp_dir("open-broken");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        std::fs::write(&path, "{ not json").unwrap();
        assert!(matches!(Tv::open_or_default(&path), Err(Error::Config(_))));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ not json");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
