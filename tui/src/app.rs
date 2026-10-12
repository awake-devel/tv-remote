//! Interactive TUI state and input handling. Rendering lives in `ui.rs`.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Position, Rect};
use ratatui::widgets::ListState;

use tvremote_core::cast::{CastEvent, CastRequest, CastSession, CastStatus, CompatReport, MediaInfo};
use tvremote_core::netcheck::{CapacityProgress, FixKind};
use tvremote_core::fleet::Candidate;
use tvremote_core::{
    App as TvApp, CloudState, ConnectionState, Error, Event, Fleet, Key, PowerState, Tv, TvEntry,
    TvEvent, VerdictLevel,
};

use crate::browser::Browser;
use crate::castflow::{Analysis, ProfileForm, VerdictView};
use crate::worker::{Cmd, Level, Msg, Workers};

/// Main screens (the status strip and log line are always visible).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Main,
    Apps,
    Browser,
    Profile,
    Verdict,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptKind {
    Text,
    Url,
    YouTube,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prompt {
    pub kind: PromptKind,
    pub buf: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmKind {
    NetTest,
    QuitWhileCasting,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Overlay {
    None,
    Help,
    Prompt(Prompt),
    Confirm(ConfirmKind),
    /// `T`: pick the TV to control (known TVs, then new ones from the last scan).
    TvPicker,
}

/// One row of the TV picker.
#[derive(Debug, Clone)]
pub enum PickerRow {
    Known(Box<TvEntry>),
    New(Box<Candidate>),
}

/// Probe result for one browser file.
#[derive(Debug, Clone)]
pub enum ProbeState {
    Done(Box<MediaInfo>, CompatReport),
    Failed(String),
}

/// Network test progress for the popup.
#[derive(Debug, Clone)]
pub struct NetTest {
    pub stage: String,
    pub ratio: f64,
    /// Set to abort the test (Esc).
    pub cancel: Arc<AtomicBool>,
    /// Quit once the cancelled test has stopped the TV playback.
    pub quit_after: bool,
}

impl NetTest {
    pub fn cancelling(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }

    fn request_cancel(&mut self) {
        self.cancel.store(true, Ordering::SeqCst);
        self.stage = "Cancelling: stopping the test pattern on the TV…".into();
    }
}

pub struct App {
    pub fleet: Fleet,
    pub tv: Tv,
    pub workers: Workers,
    msgs: Receiver<Msg>,
    events: Receiver<TvEvent>,

    // several TVs
    /// Known TVs (refreshed when the fleet changes).
    pub tv_list: Vec<TvEntry>,
    /// Last power state seen per TV id (status polling).
    pub tv_power: HashMap<String, PowerState>,
    /// Last SmartThings state per TV id.
    pub tv_cloud: HashMap<String, CloudState>,
    /// New TVs from the last scan in the picker.
    pub scan_new: Vec<Candidate>,
    pub scanning: bool,
    pub picker_state: ListState,
    pub emoji: bool,
    /// "Playing on the TV" already shown for the current cast.
    cast_announced: bool,
    pub quit: bool,
    pub screen: Screen,
    pub overlay: Overlay,

    // status
    pub host: String,
    pub conn: ConnectionState,
    pub power: Option<PowerState>,
    pub volume: Option<u8>,
    pub muted: Option<bool>,
    pub session: Option<CastSession>,
    /// A cast started elsewhere, as the current TV's renderer last reported it (polled
    /// every few seconds while nothing is cast from here).
    pub renderer: Option<CastStatus>,
    renderer_polled: Option<Instant>,
    /// Where the now-playing progress bar was drawn (clicks seek there); `None` when
    /// it is hidden or the cast is not seekable.
    pub seek_bar: Option<Rect>,
    pub log: VecDeque<(Instant, Level, String)>,

    // apps
    pub apps: Vec<TvApp>,
    pub apps_loading: bool,
    pub apps_state: ListState,

    // cast flow
    pub browser: Option<Browser>,
    pub files_state: ListState,
    pub bm_state: ListState,
    pub probes: HashMap<PathBuf, ProbeState>,
    /// File chosen before its probe finished: open the profile form when it arrives.
    pending_choice: Option<PathBuf>,
    pub vaapi: Option<bool>,
    vaapi_requested: bool,
    pub form: Option<ProfileForm>,
    pub verdict: Option<VerdictView>,
    next_gen: u64,

    pub net_test: Option<NetTest>,
}

fn home() -> PathBuf {
    tvremote_core::config::home_dir()
}

impl App {
    pub fn new(fleet: Fleet, tv: Tv, emoji: bool) -> App {
        let events = fleet.subscribe();
        let (workers, msgs) = Workers::start(tv.clone());
        let host = tv.host();
        let conn = tv.connection_state();
        App {
            cast_announced: false,
            tv_list: fleet.entries(),
            fleet,
            tv,
            workers,
            msgs,
            events,
            tv_power: HashMap::new(),
            tv_cloud: HashMap::new(),
            scan_new: Vec::new(),
            scanning: false,
            picker_state: ListState::default(),
            emoji,
            quit: false,
            screen: Screen::Main,
            overlay: Overlay::None,
            host,
            conn,
            power: None,
            volume: None,
            muted: None,
            session: None,
            renderer: None,
            renderer_polled: None,
            seek_bar: None,
            log: VecDeque::new(),
            apps: Vec::new(),
            apps_loading: false,
            apps_state: ListState::default(),
            browser: None,
            files_state: ListState::default(),
            bm_state: ListState::default(),
            probes: HashMap::new(),
            pending_choice: None,
            vaapi: None,
            vaapi_requested: false,
            form: None,
            verdict: None,
            next_gen: 1,
            net_test: None,
        }
    }

    /// Start background activity (remote channel, power polling, volume).
    pub fn start(&mut self) {
        self.tv.connect();
        // REST power for every TV every 5 s, SmartThings health every minute; no
        // WebSocket is opened for the other TVs.
        self.fleet.start_status_polling(
            std::time::Duration::from_secs(5),
            std::time::Duration::from_secs(60),
        );
        self.workers.cmd(Cmd::RefreshVolume);
        self.session = self.tv.current_cast();
        self.info("Ready. Press ? for help.");
    }

    pub fn push_log(&mut self, level: Level, s: impl Into<String>) {
        self.log.push_back((Instant::now(), level, s.into()));
        while self.log.len() > 50 {
            self.log.pop_front();
        }
    }

    fn info(&mut self, s: impl Into<String>) {
        self.push_log(Level::Info, s);
    }
    fn warn(&mut self, s: impl Into<String>) {
        self.push_log(Level::Warn, s);
    }
    fn error(&mut self, s: impl Into<String>) {
        self.push_log(Level::Error, s);
    }

    fn gen(&mut self) -> u64 {
        self.next_gen += 1;
        self.next_gen
    }

    fn key(&mut self, k: Key) {
        self.workers.cmd(Cmd::Key(k));
    }

    pub fn is_casting(&self) -> bool {
        self.session.as_ref().is_some_and(|s| !s.is_finished())
    }

    /// What is casting: the session from here, else a cast started elsewhere on the
    /// current TV.
    pub fn playing(&self) -> Option<CastStatus> {
        match &self.session {
            Some(s) if !s.is_finished() => Some(s.status()),
            _ => self.renderer.clone(),
        }
    }

    /// While something seekable casts, ← → < > seek and 0-9 jump (mpv-style).
    fn can_seek(&self) -> bool {
        self.playing().is_some_and(|s| s.seekable)
    }

    /// Seek ± 10 s while a seekable cast plays, else send `key` (← → keep moving
    /// through the TV's menus).
    fn seek_or_key(&mut self, forward: bool, key: Key) {
        if self.can_seek() {
            self.workers.cmd(Cmd::Seek {
                session: self.session.clone(),
                forward,
                key,
            });
        } else {
            self.key(key);
        }
    }

    /// `key`: the digit to send instead if the cast turns out to be gone.
    fn seek_fraction(&mut self, fraction: f64, key: Option<Key>) {
        self.workers.cmd(Cmd::SeekFraction {
            session: self.session.clone(),
            fraction,
            key,
        });
    }

    /// Poll the renderer every few seconds while nothing is cast from here, so casts
    /// started elsewhere show up (and can be controlled); not while the TV is known off.
    fn maybe_poll_renderer(&mut self) {
        const EVERY: Duration = Duration::from_secs(3);
        if self.is_casting() {
            self.renderer = None;
            return;
        }
        let off = matches!(self.power, Some(PowerState::Off | PowerState::Standby));
        if off {
            self.renderer = None;
            return;
        }
        if self.host.trim().is_empty() || self.renderer_polled.is_some_and(|t| t.elapsed() < EVERY) {
            return;
        }
        self.renderer_polled = Some(Instant::now());
        self.workers.poll_renderer();
    }

    // ------------------------------------------------------------------ background

    /// Drain worker messages and core events (called every tick).
    pub fn pump(&mut self) {
        while let Ok(m) = self.msgs.try_recv() {
            self.on_msg(m);
        }
        while let Ok(e) = self.events.try_recv() {
            self.on_event(e);
        }
        if self.session.as_ref().is_some_and(|s| s.is_finished()) {
            self.session = None;
        }
        self.maybe_poll_renderer();
    }

    fn on_event(&mut self, te: TvEvent) {
        let current = self.tv.id();
        match &te.event {
            Event::Power(p) => {
                self.tv_power.insert(te.tv.clone(), p.clone());
            }
            Event::Cloud(c) => {
                self.tv_cloud.insert(te.tv.clone(), c.clone());
            }
            Event::FleetChanged | Event::ConfigChanged => self.tv_list = self.fleet.entries(),
            _ => {}
        }
        // Cast events follow the session, whatever TV it plays on.
        let mine = te.tv == current || matches!(te.event, Event::Cast(_));
        if !mine {
            if let Event::Connection(ConnectionState::Unauthorized) = te.event {
                let name = self.tv_list.iter().find(|e| e.id == te.tv).map(|e| e.display_name()).unwrap_or(te.tv);
                self.error(format!("{name} refused the pairing: approve the prompt on that TV"));
            }
            return;
        }
        let e = te.event;
        match e {
            Event::Connection(c) => {
                match &c {
                    ConnectionState::Connected => self.info("Remote channel connected"),
                    ConnectionState::Unauthorized => {
                        self.error("The TV refused the pairing: approve the prompt on the TV")
                    }
                    ConnectionState::Retrying(r) if self.conn == ConnectionState::Connected => {
                        self.warn(format!("Remote channel lost: {r}"))
                    }
                    _ => {}
                }
                self.conn = c;
            }
            Event::Power(p) => {
                if p.is_on() && !self.power.as_ref().is_some_and(PowerState::is_on) {
                    self.workers.cmd(Cmd::RefreshVolume);
                }
                self.power = Some(p);
            }
            Event::TokenUpdated => self.info("New pairing token saved"),
            Event::ConfigChanged => self.host = self.tv.host(),
            Event::Cloud(_) | Event::FleetChanged => {}
            Event::RemoteEvent { name, .. } => {
                if name == "ms.remote.imeStart" {
                    self.info("A text field is focused on the TV: press t to type");
                }
            }
            Event::Cast(c) => self.on_cast_event(c),
        }
    }

    fn on_cast_event(&mut self, c: CastEvent) {
        match c {
            CastEvent::Loading { title } => {
                self.cast_announced = false;
                self.info(format!("Loading “{title}” on the TV…"))
            }
            CastEvent::Stall {
                message,
                suggestion,
            } => self.warn(format!("{message} {suggestion}")),
            CastEvent::Ended => self.info("Playback finished"),
            CastEvent::NeverStarted => {
                self.error("The TV never started playing (format probably unsupported)")
            }
            CastEvent::Stopped => self.info("Cast stopped"),
            CastEvent::Error(e) => self.error(format!("Cast: {e}")),
            CastEvent::State(tvremote_core::dlna::TransportState::Playing)
                if !self.cast_announced =>
            {
                self.cast_announced = true;
                self.info("Playing on the TV");
            }
            // Which seek / pause path a TV-friendly cast took.
            CastEvent::Note(n) => self.info(n),
            CastEvent::State(_) | CastEvent::Position { .. } | CastEvent::Throughput(_) => {}
        }
    }

    fn on_msg(&mut self, m: Msg) {
        match m {
            Msg::Log(l, s) => self.push_log(l, s),
            Msg::Volume { volume, muted } => {
                self.volume = volume;
                self.muted = muted;
            }
            Msg::Apps(r) => {
                self.apps_loading = false;
                match r {
                    Ok(mut apps) => {
                        apps.sort_by_key(|a| a.name.to_lowercase());
                        self.info(format!("{} apps", apps.len()));
                        self.apps = apps;
                        let sel = if self.apps.is_empty() { None } else { Some(0) };
                        self.apps_state.select(sel);
                    }
                    Err(e) => self.error(format!("App list: {e}")),
                }
            }
            Msg::Probed(path, r) => {
                let st = match r {
                    Ok(info) => {
                        let rep = tvremote_core::cast::check_compat(&info);
                        ProbeState::Done(Box::new(info), rep)
                    }
                    Err(e) => ProbeState::Failed(e),
                };
                self.probes.insert(path.clone(), st);
                if self.pending_choice.as_ref() == Some(&path) {
                    self.pending_choice = None;
                    if self.screen == Screen::Browser && self.overlay == Overlay::None {
                        self.choose_file(path);
                    }
                }
            }
            Msg::Vaapi(ok) => {
                self.vaapi = Some(ok);
                if let Some(f) = &mut self.form {
                    f.set_vaapi(ok);
                }
                if let Some(v) = &mut self.verdict {
                    v.form.set_vaapi(ok);
                }
            }
            Msg::Verdict { gen, bundle } => {
                if let Some(v) = &mut self.verdict {
                    if v.gen == gen {
                        v.bundle = Some(*bundle);
                    }
                }
            }
            Msg::DemandProgress { job, progress } => {
                if let Some(v) = &mut self.verdict {
                    if v.job_id == job {
                        v.progress = progress;
                    }
                }
            }
            Msg::DemandDone { job, result } => {
                let mut recompute = false;
                if let Some(v) = &mut self.verdict {
                    if v.job_id == job {
                        v.progress = 1.0;
                        match result {
                            Ok(p) => {
                                v.packets = Some(p);
                                v.analysis = Analysis::Done;
                                recompute = true;
                            }
                            Err(Error::Cancelled) => v.analysis = Analysis::Cancelled,
                            Err(e) => v.analysis = Analysis::Failed(e.to_string()),
                        }
                    }
                }
                if recompute {
                    self.request_verdict();
                }
            }
            Msg::CastStarted(r) => {
                if let Some(v) = &mut self.verdict {
                    v.casting = false;
                }
                match r {
                    Ok(s) => {
                        self.info(format!("Casting “{}”", s.status().title));
                        self.session = Some(s);
                        if let Some(v) = self.verdict.take() {
                            if let Some(j) = &v.job {
                                j.cancel();
                            }
                        }
                        self.form = None;
                        self.screen = Screen::Main;
                    }
                    Err(e) => self.error(format!("Cast failed: {e}")),
                }
            }
            Msg::NetTestProgress(p) => {
                let (stage, ratio) = match p {
                    CapacityProgress::GeneratingClip(f) => (
                        "Encoding the probe clip (first run, about a minute)…".to_string(),
                        f as f64,
                    ),
                    CapacityProgress::Starting => {
                        ("Asking the TV to play the test pattern…".into(), 0.0)
                    }
                    CapacityProgress::Measuring { elapsed, mbps } => (
                        format!("Measuring: {elapsed:.0} s, {mbps:.1} Mbps so far"),
                        (elapsed / tvremote_core::netcheck::MEASURE_SECS as f64).clamp(0.0, 1.0),
                    ),
                    CapacityProgress::Done => ("Done".into(), 1.0),
                };
                if let Some(n) = &mut self.net_test {
                    if !n.cancelling() {
                        n.stage = stage;
                        n.ratio = ratio;
                    }
                }
            }
            Msg::NetTestDone(r) => {
                let quit_after = self.net_test.take().is_some_and(|n| n.quit_after);
                match r {
                    Ok(m) => self.info(format!(
                        "Network capacity: {:.1} Mbps (best second {:.1} Mbps)",
                        m.mbps, m.peak_mbps
                    )),
                    Err(Error::Cancelled) => self.info("Network test cancelled: nothing was saved"),
                    Err(e) => self.error(format!("Network test failed: {e}")),
                }
                if quit_after {
                    self.request_quit();
                }
                if self.verdict.is_some() {
                    self.request_verdict();
                }
            }
            Msg::Scanned(r) => {
                self.scanning = false;
                if let Some(e) = &r.lan_error {
                    self.error(format!("LAN scan: {e}"));
                }
                if let Some(e) = &r.cloud_error {
                    self.warn(format!("SmartThings: {e}"));
                }
                for h in &r.host_changes {
                    self.info(format!("{}: new address {}", h.id, h.new));
                }
                self.scan_new = r.candidates.iter().filter(|c| c.known.is_none()).cloned().collect();
                self.tv_list = self.fleet.entries();
                self.info(format!(
                    "Scan: {} TV(s) found, {} new",
                    r.candidates.len(),
                    self.scan_new.len()
                ));
            }
            Msg::Renderer(st) => {
                if !self.is_casting() {
                    self.renderer = st;
                }
            }
            Msg::PowerDone { on, result } => match result {
                Ok(()) => self.info(if on {
                    "The TV is on"
                } else {
                    "Turning the TV off"
                }),
                Err(e) => self.error(format!("Power {}: {e}", if on { "on" } else { "off" })),
            },
        }
    }

    // ------------------------------------------------------------------ input

    pub fn on_key(&mut self, k: KeyEvent) {
        if k.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(k.code, KeyCode::Char('c') | KeyCode::Char('q'))
        {
            self.request_quit();
            return;
        }
        // Esc cancels a running network test (the popup covers the screen).
        if k.code == KeyCode::Esc {
            if let Some(n) = &mut self.net_test {
                if !n.cancelling() {
                    n.request_cancel();
                    self.info("Cancelling the network test…");
                }
                return;
            }
        }
        match self.overlay.clone() {
            Overlay::Help => {
                self.overlay = Overlay::None;
                return;
            }
            Overlay::Prompt(p) => {
                self.on_prompt_key(p, k);
                return;
            }
            Overlay::TvPicker => {
                self.on_picker_key(k);
                return;
            }
            Overlay::Confirm(c) => {
                self.overlay = Overlay::None;
                if matches!(k.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                    match c {
                        ConfirmKind::NetTest => {
                            let cancel = Arc::new(AtomicBool::new(false));
                            self.net_test = Some(NetTest {
                                stage: "Starting…".into(),
                                ratio: 0.0,
                                cancel: cancel.clone(),
                                quit_after: false,
                            });
                            self.info("Network test started (test pattern on the TV for ~10 s; Esc cancels)");
                            self.workers.net_test(cancel);
                        }
                        ConfirmKind::QuitWhileCasting => {
                            if let Some(s) = &self.session {
                                let _ = s.stop();
                            }
                            self.quit = true;
                        }
                    }
                } else {
                    self.info("Cancelled");
                }
                return;
            }
            Overlay::None => {}
        }
        if matches!(k.code, KeyCode::Char('?') | KeyCode::F(1)) {
            self.overlay = Overlay::Help;
            return;
        }
        match self.screen {
            Screen::Main => self.on_main_key(k),
            Screen::Apps => self.on_apps_key(k),
            Screen::Browser => self.on_browser_key(k),
            Screen::Profile => self.on_profile_key(k),
            Screen::Verdict => self.on_verdict_key(k),
        }
    }

    /// A click on the now-playing progress bar seeks to that point (any screen, no
    /// popup open).
    pub fn on_mouse(&mut self, m: MouseEvent) {
        if m.kind != MouseEventKind::Down(MouseButton::Left)
            || self.overlay != Overlay::None
            || self.net_test.is_some()
        {
            return;
        }
        let Some(bar) = self.seek_bar.filter(|r| r.contains(Position::new(m.column, m.row))) else {
            return;
        };
        if self.can_seek() {
            self.seek_fraction(crate::util::column_fraction(m.column, bar.x, bar.width), None);
        }
    }

    fn request_quit(&mut self) {
        // Stop the network test first so the TV does not keep showing the test pattern;
        // asking again quits at once.
        if let Some(n) = &mut self.net_test {
            if !n.quit_after {
                n.request_cancel();
                n.quit_after = true;
                n.stage = "Stopping the network test before quitting (again to force)…".into();
                return;
            }
            self.quit = true;
            return;
        }
        if self.is_casting() {
            self.overlay = Overlay::Confirm(ConfirmKind::QuitWhileCasting);
        } else {
            self.quit = true;
        }
    }

    fn open_prompt(&mut self, kind: PromptKind) {
        self.overlay = Overlay::Prompt(Prompt {
            kind,
            buf: String::new(),
        });
    }

    fn on_prompt_key(&mut self, mut p: Prompt, k: KeyEvent) {
        match k.code {
            KeyCode::Esc => {
                self.overlay = Overlay::None;
                return;
            }
            KeyCode::Enter => {
                self.overlay = Overlay::None;
                let text = p.buf.trim().to_string();
                if text.is_empty() {
                    return;
                }
                match p.kind {
                    PromptKind::Text => self.workers.cmd(Cmd::Text(p.buf)),
                    PromptKind::YouTube => self.workers.cmd(Cmd::YouTube(text)),
                    PromptKind::Url => {
                        if crate::util::is_url(&text) {
                            self.info(format!("Casting URL {text}…"));
                            self.workers.cast(CastRequest::url(text));
                        } else {
                            self.error("Not an http(s) URL");
                        }
                    }
                }
                return;
            }
            KeyCode::Backspace => {
                p.buf.pop();
            }
            KeyCode::Char('u') if k.modifiers.contains(KeyModifiers::CONTROL) => p.buf.clear(),
            KeyCode::Char(c) => p.buf.push(c),
            _ => {}
        }
        self.overlay = Overlay::Prompt(p);
    }

    fn on_main_key(&mut self, k: KeyEvent) {
        match k.code {
            KeyCode::Char('q') | KeyCode::Esc => self.request_quit(),
            KeyCode::Up => self.key(Key::Up),
            KeyCode::Down => self.key(Key::Down),
            KeyCode::Left => self.seek_or_key(false, Key::Left),
            KeyCode::Right => self.seek_or_key(true, Key::Right),
            KeyCode::Enter => self.key(Key::Enter),
            KeyCode::Backspace => self.key(Key::Return),
            KeyCode::PageUp => self.key(Key::ChannelUp),
            KeyCode::PageDown => self.key(Key::ChannelDown),
            KeyCode::Char('h') => self.key(Key::Home),
            KeyCode::Char('+') | KeyCode::Char('=') => self.key(Key::VolumeUp),
            KeyCode::Char('-') => self.key(Key::VolumeDown),
            KeyCode::Char('m') => self.key(Key::Mute),
            KeyCode::Char('M') => self.key(Key::Menu),
            KeyCode::Char('S') => self.key(Key::Source),
            KeyCode::Char('I') => self.key(Key::Info),
            KeyCode::Char('G') => self.key(Key::Guide),
            KeyCode::Char('x') => self.key(Key::Exit),
            KeyCode::Char('<') => self.seek_or_key(false, Key::Rewind),
            KeyCode::Char('>') => self.seek_or_key(true, Key::FastForward),
            KeyCode::F(n @ 2..=5) => {
                self.key([Key::Red, Key::Green, Key::Yellow, Key::Blue][(n - 2) as usize].clone())
            }
            // mpv-style: 0-9 jump to 0 %-90 % while a seekable cast plays (a TV number
            // key would leave the player anyway); TV number keys otherwise.
            KeyCode::Char(c @ '0'..='9') if self.can_seek() => {
                let d = c as u8 - b'0';
                self.seek_fraction(f64::from(d) / 10.0, Key::digit(d))
            }
            KeyCode::Char(c @ '0'..='9') => {
                if let Some(key) = Key::digit(c as u8 - b'0') {
                    self.key(key);
                }
            }
            KeyCode::Char('p') => {
                let on = self.power.as_ref().is_some_and(PowerState::is_on);
                self.info(if on {
                    "Turning the TV off…"
                } else {
                    "Waking the TV (Wake-on-LAN)…"
                });
                self.workers.power(on);
            }
            KeyCode::Char(' ') => self.workers.cmd(Cmd::PlayPause(self.session.clone())),
            KeyCode::Char('s') => self.workers.cmd(Cmd::Stop(self.session.clone())),
            KeyCode::Char('T') => self.open_picker(),
            KeyCode::Char('t') => self.open_prompt(PromptKind::Text),
            KeyCode::Char('a') => {
                self.screen = Screen::Apps;
                if self.apps.is_empty() && !self.apps_loading {
                    self.refresh_apps();
                }
            }
            KeyCode::Char('c') => self.open_browser(),
            KeyCode::Char('n') => self.overlay = Overlay::Confirm(ConfirmKind::NetTest),
            _ => {}
        }
    }

    // ------------------------------------------------------------------ TV picker

    /// Rows of the TV picker: known TVs, then new ones from the last scan.
    pub fn picker_rows(&self) -> Vec<PickerRow> {
        self.tv_list
            .iter()
            .cloned()
            .map(|e| PickerRow::Known(Box::new(e)))
            .chain(self.scan_new.iter().cloned().map(|c| PickerRow::New(Box::new(c))))
            .collect()
    }

    fn open_picker(&mut self) {
        self.tv_list = self.fleet.entries();
        let cur = self.tv.id();
        let i = self.tv_list.iter().position(|e| e.id == cur).unwrap_or(0);
        self.picker_state.select(Some(i));
        self.overlay = Overlay::TvPicker;
    }

    fn on_picker_key(&mut self, k: KeyEvent) {
        let rows = self.picker_rows();
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('T') => self.overlay = Overlay::None,
            KeyCode::Up | KeyCode::Char('k') => self.picker_state.select_previous(),
            KeyCode::Down | KeyCode::Char('j') => self.picker_state.select_next(),
            KeyCode::Char('s') => {
                if !self.scanning {
                    self.scanning = true;
                    self.info("Scanning the LAN and SmartThings…");
                    self.workers.scan(self.fleet.clone());
                }
            }
            KeyCode::Enter => {
                let Some(row) = self.picker_state.selected().and_then(|i| rows.get(i)).cloned() else {
                    return;
                };
                match row {
                    PickerRow::Known(e) => {
                        self.overlay = Overlay::None;
                        self.switch_tv(&e.id);
                    }
                    PickerRow::New(c) => match self.fleet.add_candidate(&c) {
                        Ok(id) => {
                            self.scan_new.retain(|x| x.key != c.key);
                            self.tv_list = self.fleet.entries();
                            self.info(format!("Added {} as {id}: Enter switches to it (approve on the TV)", c.name));
                            if let Some(i) = self.tv_list.iter().position(|e| e.id == id) {
                                self.picker_state.select(Some(i));
                            }
                        }
                        Err(e) => self.error(format!("Add: {e}")),
                    },
                }
            }
            _ => {}
        }
    }

    /// Control another TV: it becomes the active one; pages rebind. A running cast keeps
    /// playing on the TV it started on.
    pub fn switch_tv(&mut self, id: &str) {
        if id == self.tv.id() {
            return;
        }
        let tv = match self.fleet.tv(id) {
            Ok(tv) => tv,
            Err(e) => return self.error(e.to_string()),
        };
        if let Err(e) = self.fleet.set_active(id) {
            self.warn(format!("Could not save the active TV: {e}"));
        }
        self.workers.set_tv(tv.clone());
        self.tv = tv;
        self.renderer = None;
        self.renderer_polled = None;
        self.host = self.tv.host();
        self.conn = self.tv.connection_state();
        self.power = self.tv_power.get(id).cloned();
        self.volume = None;
        self.muted = None;
        self.apps.clear();
        self.apps_state.select(None);
        if !self.tv.host().trim().is_empty() {
            self.tv.connect();
        }
        self.workers.cmd(Cmd::RefreshVolume);
        let name = self.tv.display_name();
        self.info(if self.is_casting() {
            format!("Now controlling {name} (the cast keeps playing on its TV)")
        } else {
            format!("Now controlling {name}")
        });
    }

    fn refresh_apps(&mut self) {
        self.apps_loading = true;
        self.info("Loading the app list…");
        self.workers.fetch_apps();
    }

    fn on_apps_key(&mut self, k: KeyEvent) {
        let len = self.apps.len();
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') => self.screen = Screen::Main,
            KeyCode::Char('r') => self.refresh_apps(),
            KeyCode::Char('y') => self.open_prompt(PromptKind::YouTube),
            KeyCode::Up | KeyCode::Char('k') => self.apps_state.select_previous(),
            KeyCode::Down | KeyCode::Char('j') => self.apps_state.select_next(),
            KeyCode::Home => self.apps_state.select_first(),
            KeyCode::End => self.apps_state.select_last(),
            KeyCode::Enter => {
                if let Some(app) = self
                    .apps_state
                    .selected()
                    .filter(|i| *i < len)
                    .map(|i| self.apps[i].clone())
                {
                    self.info(format!("Launching {}…", app.name));
                    self.workers.cmd(Cmd::LaunchApp(app));
                }
            }
            _ => {}
        }
    }

    // ------------------------------------------------------------------ cast flow

    fn open_browser(&mut self) {
        if self.browser.is_none() {
            self.browser = Some(Browser::new(&home()));
        } else if let Some(b) = &mut self.browser {
            b.refresh_bookmarks(&home());
            b.reload();
        }
        self.screen = Screen::Browser;
        self.queue_probes();
        if !self.vaapi_requested {
            self.vaapi_requested = true;
            self.workers.check_vaapi();
        }
    }

    fn queue_probes(&mut self) {
        let Some(b) = &self.browser else { return };
        let files: Vec<PathBuf> = b
            .media_files()
            .into_iter()
            .filter(|p| !self.probes.contains_key(p))
            .collect();
        self.workers.set_probe_queue(files);
    }

    fn on_browser_key(&mut self, k: KeyEvent) {
        let Some(b) = &mut self.browser else {
            self.screen = Screen::Main;
            return;
        };
        let before = b.cwd.clone();
        let mut chosen = None;
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.pending_choice = None;
                self.workers.set_probe_queue(Vec::new());
                self.screen = Screen::Main;
                return;
            }
            KeyCode::Up | KeyCode::Char('k') => b.move_by(-1),
            KeyCode::Down | KeyCode::Char('j') => b.move_by(1),
            KeyCode::PageUp => b.move_by(-10),
            KeyCode::PageDown => b.move_by(10),
            KeyCode::Home => b.home_end(false),
            KeyCode::End => b.home_end(true),
            KeyCode::Tab | KeyCode::BackTab => {
                b.focus = match b.focus {
                    crate::browser::Pane::Files => crate::browser::Pane::Bookmarks,
                    crate::browser::Pane::Bookmarks => crate::browser::Pane::Files,
                }
            }
            KeyCode::Backspace | KeyCode::Left => b.parent(),
            KeyCode::Enter | KeyCode::Right => chosen = b.activate(),
            KeyCode::Char('.') => b.toggle_hidden(),
            KeyCode::Char('u') => {
                self.open_prompt(PromptKind::Url);
                return;
            }
            KeyCode::Char('r') => b.reload(),
            _ => {}
        }
        if let Some(err) = b.error.take() {
            self.error(err);
        }
        let changed = self.browser.as_ref().is_some_and(|b| b.cwd != before);
        if changed {
            self.queue_probes();
        }
        if let Some(path) = chosen {
            self.choose_file(path);
        } else if let Some(p) = self
            .browser
            .as_ref()
            .and_then(|b| b.current())
            .filter(|e| e.is_file())
        {
            // Probe the highlighted file first.
            if !self.probes.contains_key(&p.path) {
                self.workers.probe_first(p.path.clone());
            }
        }
    }

    fn choose_file(&mut self, path: PathBuf) {
        match self.probes.get(&path) {
            Some(ProbeState::Done(info, _)) => {
                self.form = Some(ProfileForm::new((**info).clone(), self.vaapi));
                self.screen = Screen::Profile;
            }
            Some(ProbeState::Failed(e)) => self.error(format!("ffprobe failed: {e}")),
            None => {
                self.info("Analysing this file (ffprobe)…");
                self.pending_choice = Some(path.clone());
                self.workers.probe_first(path);
            }
        }
    }

    fn on_profile_key(&mut self, k: KeyEvent) {
        let Some(f) = &mut self.form else {
            self.screen = Screen::Browser;
            return;
        };
        match k.code {
            KeyCode::Esc | KeyCode::Backspace | KeyCode::Char('q') => self.screen = Screen::Browser,
            KeyCode::Up | KeyCode::Char('k') => f.move_row(-1),
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => f.move_row(1),
            KeyCode::Left | KeyCode::Char('h') => f.cycle(-1),
            KeyCode::Right | KeyCode::Char('l') | KeyCode::Char(' ') => f.cycle(1),
            KeyCode::Enter => {
                if let Some(r) = f.blocked_reason() {
                    self.error(r);
                    return;
                }
                let form = f.clone();
                self.open_verdict(form);
            }
            _ => {}
        }
    }

    fn open_verdict(&mut self, form: ProfileForm) {
        let gen = self.gen();
        let job_id = self.gen();
        let same_file = self
            .verdict
            .as_ref()
            .is_some_and(|v| v.form.info.path == form.info.path);
        let mut view = VerdictView::new(form, gen, job_id);
        // Reuse a finished analysis of the same file.
        if same_file {
            if let Some(old) = self.verdict.take() {
                if old.analysis == Analysis::Done {
                    view.packets = old.packets;
                    view.analysis = Analysis::Done;
                    view.progress = 1.0;
                } else {
                    if let Some(j) = &old.job {
                        j.cancel();
                    }
                }
            }
        } else if let Some(old) = self.verdict.take() {
            if let Some(j) = &old.job {
                j.cancel();
            }
        }
        if view.analysis == Analysis::Running {
            view.job = Some(self.workers.analyze(job_id, &view.form.info));
        }
        self.verdict = Some(view);
        self.screen = Screen::Verdict;
        self.request_verdict();
    }

    fn request_verdict(&mut self) {
        let gen = self.gen();
        if let Some(v) = &mut self.verdict {
            v.gen = gen;
            self.workers
                .verdict(gen, v.form.info.clone(), v.form.mode(), v.packets.clone());
        }
    }

    fn on_verdict_key(&mut self, k: KeyEvent) {
        let Some(v) = &mut self.verdict else {
            self.screen = Screen::Profile;
            return;
        };
        if v.casting {
            return;
        }
        if v.confirm_bad {
            v.confirm_bad = false;
            if matches!(k.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                self.start_cast();
            } else {
                self.info("Not casting");
            }
            return;
        }
        match k.code {
            KeyCode::Esc => {
                if !v.cancel_analysis() {
                    // Keep form edits from fixes when going back.
                    self.form = Some(v.form.clone());
                    self.screen = Screen::Profile;
                } else {
                    self.info("Packet analysis cancelled (provisional estimate kept)");
                }
            }
            KeyCode::Backspace | KeyCode::Char('q') => {
                self.form = Some(v.form.clone());
                self.screen = Screen::Profile;
            }
            KeyCode::Char('n') => self.overlay = Overlay::Confirm(ConfirmKind::NetTest),
            KeyCode::Up | KeyCode::Char('k') => v.scroll = v.scroll.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => v.scroll = v.scroll.saturating_add(1),
            KeyCode::Enter | KeyCode::Char('c') => {
                let bad = v
                    .bundle
                    .as_ref()
                    .is_some_and(|b| b.verdict.level == VerdictLevel::Bad);
                if v.bundle.is_none() {
                    self.info("Computing the verdict…");
                } else if bad {
                    v.confirm_bad = true;
                } else {
                    self.start_cast();
                }
            }
            KeyCode::Char(c @ '1'..='9') => {
                let i = (c as u8 - b'1') as usize;
                let fix = v
                    .bundle
                    .as_ref()
                    .and_then(|b| visible_fixes(b, self.vaapi).get(i).cloned());
                let Some(fix) = fix else { return };
                match &fix.kind {
                    FixKind::DropExtraTracks { profile } | FixKind::ReduceVideo { profile, .. } => {
                        v.form.select_profile(profile);
                        v.bundle = None;
                        let d = fix.description.clone();
                        self.info(format!("Applied: {d}"));
                        self.request_verdict();
                    }
                    FixKind::CopyToTv => {
                        self.info("Copy the file to a USB drive/SSD plugged into the TV and play it from the TV's media player")
                    }
                }
            }
            _ => {}
        }
    }

    fn start_cast(&mut self) {
        let Some(v) = &mut self.verdict else { return };
        v.casting = true;
        let f = &v.form;
        let req = CastRequest {
            source: tvremote_core::CastSource::File(f.info.path.clone()),
            mode: f.mode(),
            title: None,
            media: Some(f.info.clone()),
        };
        let name = file_name(&f.info.path);
        self.info(format!("Starting the cast of {name}…"));
        self.workers.cast(req);
    }
}

/// Fixes the user can apply (VAAPI-only fixes hidden when VAAPI is known to be missing).
pub fn visible_fixes(
    b: &crate::worker::VerdictBundle,
    vaapi: Option<bool>,
) -> Vec<tvremote_core::netcheck::Fix> {
    b.verdict
        .fixes
        .iter()
        .filter(|f| !(f.requires_vaapi && vaapi == Some(false)))
        .cloned()
        .collect()
}

pub fn file_name(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| p.display().to_string())
}
