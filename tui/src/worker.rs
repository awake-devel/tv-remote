//! Background work for the TUI. Every core call blocks, so it runs here and the result
//! comes back to the UI loop as a [`Msg`] over an mpsc channel.
//!
//! * a **command worker** thread executes quick remote/DLNA commands in order (keys must
//!   not be reordered);
//! * a **probe worker** thread runs ffprobe for the file browser, one file at a time,
//!   from a queue the UI replaces whenever the folder changes;
//! * long jobs (app list, cast, network test, power on, verdicts) get their own thread.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use tvremote_core::cast::{vaapi_available, CastRequest, CastSession, MediaInfo};
use tvremote_core::dlna::TransportState;
use tvremote_core::netcheck::{
    self, CapacityMeasurement, CapacityProgress, LinkInfo, PacketProfile,
};
use tvremote_core::{App, CastMode, Demand, Error, Key, Tv, Verdict};

/// Severity of a log-line message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Warn,
    Error,
}

/// Everything the network verdict screen needs, computed off the UI thread.
#[derive(Debug, Clone)]
pub struct VerdictBundle {
    pub demand: Demand,
    pub verdict: Verdict,
    pub capacity: Option<CapacityMeasurement>,
    pub link: Option<LinkInfo>,
}

/// Results flowing back to the UI loop.
pub enum Msg {
    Log(Level, String),
    Volume {
        volume: Option<u8>,
        muted: Option<bool>,
    },
    Apps(Result<Vec<App>, Error>),
    Probed(PathBuf, Result<MediaInfo, String>),
    Vaapi(bool),
    Verdict {
        gen: u64,
        bundle: Box<VerdictBundle>,
    },
    DemandProgress {
        job: u64,
        progress: f32,
    },
    DemandDone {
        job: u64,
        result: Result<Arc<PacketProfile>, Error>,
    },
    CastStarted(Result<CastSession, Error>),
    NetTestProgress(CapacityProgress),
    NetTestDone(Result<CapacityMeasurement, Error>),
    PowerDone {
        on: bool,
        result: Result<(), Error>,
    },
    /// `T` picker: LAN + SmartThings detection finished.
    Scanned(Box<tvremote_core::ScanReport>),
}

/// Quick commands executed in order by the command worker.
pub enum Cmd {
    Key(Key),
    Text(String),
    RefreshVolume,
    /// Toggle play/pause: the cast session if any (whatever TV it runs on), else the
    /// current TV's renderer.
    PlayPause(Option<CastSession>),
    /// Stop: the cast session if any, else the current TV's renderer.
    Stop(Option<CastSession>),
    LaunchApp(App),
    YouTube(String),
}

fn log(tx: &Sender<Msg>, level: Level, s: impl Into<String>) {
    let _ = tx.send(Msg::Log(level, s.into()));
}

/// Handles to the background workers.
pub struct Workers {
    tv: Arc<Mutex<Tv>>,
    tx: Sender<Msg>,
    cmd_tx: Sender<Cmd>,
    probe_queue: Arc<(Mutex<VecDeque<PathBuf>>, Condvar)>,
}

impl Workers {
    /// Start the command and probe workers; returns the handles and the message receiver.
    pub fn start(tv: Tv) -> (Workers, Receiver<Msg>) {
        let tv = Arc::new(Mutex::new(tv));
        let (tx, rx) = channel();
        let (cmd_tx, cmd_rx) = channel::<Cmd>();
        {
            let tv = tv.clone();
            let tx = tx.clone();
            let _ = std::thread::Builder::new()
                .name("tui-cmd".into())
                .spawn(move || command_worker(tv, cmd_rx, tx));
        }
        let probe_queue: Arc<(Mutex<VecDeque<PathBuf>>, Condvar)> = Arc::default();
        {
            let q = probe_queue.clone();
            let tx = tx.clone();
            let _ = std::thread::Builder::new()
                .name("tui-probe".into())
                .spawn(move || probe_worker(q, tx));
        }
        (
            Workers {
                tv,
                tx,
                cmd_tx,
                probe_queue,
            },
            rx,
        )
    }

    /// The TV commands go to.
    pub fn tv(&self) -> Tv {
        current(&self.tv)
    }

    /// Switch the TV later commands go to (queued commands keep their order).
    pub fn set_tv(&self, tv: Tv) {
        if let Ok(mut g) = self.tv.lock() {
            *g = tv;
        }
    }

    pub fn cmd(&self, c: Cmd) {
        let _ = self.cmd_tx.send(c);
    }

    /// Replace the probe queue (e.g. after a folder change).
    pub fn set_probe_queue(&self, files: Vec<PathBuf>) {
        let (m, c) = &*self.probe_queue;
        if let Ok(mut q) = m.lock() {
            *q = files.into();
        }
        c.notify_all();
    }

    /// Probe this file next.
    pub fn probe_first(&self, file: PathBuf) {
        let (m, c) = &*self.probe_queue;
        if let Ok(mut q) = m.lock() {
            q.retain(|p| p != &file);
            q.push_front(file);
        }
        c.notify_all();
    }

    fn spawn(&self, name: &str, f: impl FnOnce(Tv, Sender<Msg>) + Send + 'static) {
        let tv = self.tv();
        let tx = self.tx.clone();
        if let Err(e) = std::thread::Builder::new()
            .name(name.into())
            .spawn(move || f(tv, tx))
        {
            log(
                &self.tx,
                Level::Error,
                format!("cannot start a worker thread: {e}"),
            );
        }
    }

    pub fn fetch_apps(&self) {
        self.spawn("tui-apps", |tv, tx| {
            let _ = tx.send(Msg::Apps(tv.installed_apps()));
        });
    }

    /// VAAPI probe (slow first call, cached by core).
    pub fn check_vaapi(&self) {
        self.spawn("tui-vaapi", |_tv, tx| {
            let _ = tx.send(Msg::Vaapi(vaapi_available()));
        });
    }

    /// Compute the verdict (reads caches, runs `iw`; local only, no TV traffic).
    pub fn verdict(
        &self,
        gen: u64,
        info: MediaInfo,
        mode: CastMode,
        packets: Option<Arc<PacketProfile>>,
    ) {
        self.spawn("tui-verdict", move |tv, tx| {
            let (demand, verdict) =
                tv.network_verdict(&info, &mode, packets.as_deref(), tv.tv_network());
            let bundle = VerdictBundle {
                demand,
                verdict,
                capacity: netcheck::load_capacity(),
                link: netcheck::detect_link(),
            };
            let _ = tx.send(Msg::Verdict {
                gen,
                bundle: Box::new(bundle),
            });
        });
    }

    /// Start the background packet analysis; returns the job handle (cancel with it).
    pub fn analyze(&self, job: u64, info: &MediaInfo) -> netcheck::DemandJob {
        let tx1 = self.tx.clone();
        let tx2 = self.tx.clone();
        self.tv().analyze_demand(
            info,
            move |p| {
                let _ = tx1.send(Msg::DemandProgress { job, progress: p });
            },
            move |result| {
                let _ = tx2.send(Msg::DemandDone { job, result });
            },
        )
    }

    pub fn cast(&self, req: CastRequest) {
        self.spawn("tui-cast", move |tv, tx| {
            let _ = tx.send(Msg::CastStarted(tv.cast(req)));
        });
    }

    /// Run the network test; set `cancel` to abort it (the result is then `Error::Cancelled`).
    pub fn net_test(&self, cancel: Arc<AtomicBool>) {
        self.spawn("tui-nettest", move |tv, tx| {
            let ptx = tx.clone();
            let r = tv.measure_network(&cancel, &move |p| {
                let _ = ptx.send(Msg::NetTestProgress(p));
            });
            let _ = tx.send(Msg::NetTestDone(r));
        });
    }

    /// Power off when on, Wake-on-LAN when off.
    /// Detect TVs (LAN + SmartThings) in the background.
    pub fn scan(&self, fleet: tvremote_core::Fleet) {
        let tx = self.tx.clone();
        let _ = std::thread::Builder::new().name("tui-scan".into()).spawn(move || {
            let r = fleet.scan(Duration::from_secs(3));
            let _ = tx.send(Msg::Scanned(Box::new(r)));
        });
    }

    pub fn power(&self, currently_on: bool) {
        self.spawn("tui-power", move |tv, tx| {
            let result = if currently_on {
                tv.power_off()
            } else {
                tv.power_on(Duration::from_secs(25))
            };
            let _ = tx.send(Msg::PowerDone {
                on: !currently_on,
                result,
            });
        });
    }
}

fn refresh_volume(tv: &Tv, tx: &Sender<Msg>) {
    let volume = tv.volume().ok();
    let muted = tv.mute().ok();
    let _ = tx.send(Msg::Volume { volume, muted });
}

fn current(tv: &Mutex<Tv>) -> Tv {
    match tv.lock() {
        Ok(g) => g.clone(),
        Err(e) => e.into_inner().clone(),
    }
}

fn command_worker(shared: Arc<Mutex<Tv>>, rx: Receiver<Cmd>, tx: Sender<Msg>) {
    for cmd in rx {
        let tv = current(&shared);
        match cmd {
            Cmd::Key(k) => {
                let vol = matches!(k, Key::VolumeUp | Key::VolumeDown | Key::Mute);
                match tv.send_key(&k) {
                    Ok(()) => {
                        if vol {
                            // Give the TV a moment to apply it before reading it back.
                            std::thread::sleep(Duration::from_millis(250));
                            refresh_volume(&tv, &tx);
                        }
                    }
                    Err(e) => log(&tx, Level::Error, format!("{}: {e}", k.label())),
                }
            }
            Cmd::Text(t) => match tv.send_text(&t) {
                Ok(()) => log(
                    &tx,
                    Level::Info,
                    format!("Typed {} characters on the TV", t.chars().count()),
                ),
                Err(e) => log(&tx, Level::Error, format!("Typing failed: {e}")),
            },
            Cmd::RefreshVolume => refresh_volume(&tv, &tx),
            Cmd::PlayPause(Some(s)) => {
                let r = if s.status().state == Some(TransportState::Playing) {
                    s.pause().map(|_| "Paused")
                } else {
                    s.resume().map(|_| "Playing")
                };
                match r {
                    Ok(s) => log(&tx, Level::Info, s),
                    Err(e) => log(&tx, Level::Error, format!("Play/pause: {e}")),
                }
            }
            Cmd::PlayPause(None) => {
                let session = tv.current_cast();
                let r = tv.transport().and_then(|(ti, _)| {
                    let playing = ti.state == TransportState::Playing;
                    match (&session, playing) {
                        (Some(s), true) => s.pause().map(|_| "Paused"),
                        (Some(s), false) => s.resume().map(|_| "Playing"),
                        (None, true) => tv.dlna().pause().map(|_| "Paused"),
                        (None, false) => tv.dlna().play().map(|_| "Playing"),
                    }
                });
                match r {
                    Ok(s) => log(&tx, Level::Info, s),
                    Err(e) => log(&tx, Level::Error, format!("Play/pause: {e}")),
                }
            }
            Cmd::Stop(session) => {
                let r = match session.filter(|s| !s.is_finished()).or_else(|| tv.current_cast()) {
                    Some(s) => s.stop(),
                    None => tv.dlna().stop(),
                };
                match r {
                    Ok(()) => log(&tx, Level::Info, "Stopped"),
                    Err(e) => log(&tx, Level::Error, format!("Stop: {e}")),
                }
            }
            Cmd::LaunchApp(app) => match tv.launch_app(&app) {
                Ok(()) => log(&tx, Level::Info, format!("Launched {}", app.name)),
                Err(e) => log(&tx, Level::Error, format!("Launch {}: {e}", app.name)),
            },
            Cmd::YouTube(u) => match tv.youtube(&u) {
                Ok(tvremote_core::YouTubeOpened::Video) => log(&tx, Level::Info, "Opened on YouTube"),
                Ok(tvremote_core::YouTubeOpened::AppOnly) => log(
                    &tx,
                    Level::Warn,
                    "This TV doesn't accept YouTube video links: opened the YouTube app instead",
                ),
                Err(e) => log(&tx, Level::Error, format!("YouTube: {e}")),
            },
        }
    }
}

fn probe_worker(q: Arc<(Mutex<VecDeque<PathBuf>>, Condvar)>, tx: Sender<Msg>) {
    let (m, c) = &*q;
    loop {
        let next = {
            let Ok(mut g) = m.lock() else { return };
            loop {
                if let Some(p) = g.pop_front() {
                    break p;
                }
                g = match c.wait(g) {
                    Ok(g) => g,
                    Err(_) => return,
                };
            }
        };
        let res = tvremote_core::media::probe(&next).map_err(|e| e.to_string());
        if tx.send(Msg::Probed(next, res)).is_err() {
            return;
        }
    }
}
