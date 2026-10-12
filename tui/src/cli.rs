//! Command-line interface (same binary as the TUI). Without a subcommand the
//! interactive TUI starts.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use clap::{Args, Parser, Subcommand, ValueEnum};
use serde_json::json;

use tvremote_core::cast::{
    check_compat, vaapi_available, CastEvent, CastOutcome, CastProfile, CastRequest, CastSource,
    MediaInfo, StreamKind, VideoMode,
};
use tvremote_core::netcheck::{self, FixKind, PacketProfile};
use tvremote_core::fleet::{Candidate, Fleet, LanState};
use tvremote_core::{
    AuthMethod, CastMode, CloudState, CompatLevel, Error, Event, LaunchType, PowerState, StAuth, Tv, VerdictLevel,
};

use crate::util::{
    compat_badge, fmt_age, fmt_duration, is_url, mbps, parse_seek, terminal_supports_emoji,
    verdict_badge, SeekArg,
};

#[derive(Debug, Parser)]
#[command(
    name = "tv-remote-tui",
    version,
    about = "Remote control for Samsung TVs (TUI + CLI)",
    long_about = "Remote control for Samsung TVs.\n\n\
                  Without a subcommand, starts the interactive terminal UI. \
                  With a subcommand, runs it and exits (non-zero exit code on error)."
)]
pub struct Cli {
    /// Config file (default: ~/.config/samsung-tv/config.json).
    #[arg(long, global = true, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// JSON output (info, status, apps, netcheck, discover, smartthings, tvs).
    #[arg(long, global = true)]
    pub json: bool,

    /// Which TV to control (id, name, host or unique prefix; see `tvs list`).
    /// Default: the active TV (`tvs use`).
    #[arg(long, global = true, value_name = "NAME|ID")]
    pub tv: Option<String>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Device information (REST /api/v2/).
    Info,
    /// Power, volume and what the TV's media renderer is playing.
    Status,
    /// Send a remote key: KEY_VOLUP, volup, VolumeUp, 'Vol +', KEY_HOME, ...
    Key {
        /// Key name: KEY_* code, short code, core name like VolumeUp, or button label.
        /// Unknown KEY_* codes are sent as-is.
        key: String,
        /// Long-press: hold the key for SECS seconds (default 1).
        #[arg(long, value_name = "SECS", num_args = 0..=1, default_missing_value = "1")]
        hold: Option<f64>,
    },
    /// Type text into the focused text field on the TV.
    Text {
        /// The text (several words are joined with spaces).
        #[arg(required = true, num_args = 1..)]
        text: Vec<String>,
    },
    /// List installed apps.
    Apps,
    /// Launch an installed app by id (see `apps`).
    Launch {
        /// App id, e.g. 111299001912 (YouTube).
        app_id: String,
    },
    /// Open a YouTube video (URL or 11-character id) via DIAL.
    Youtube {
        /// URL or video id.
        video: String,
    },
    /// Show the volume, or set it (0-100).
    Volume {
        #[arg(value_parser = clap::value_parser!(u8).range(0..=100))]
        level: Option<u8>,
    },
    /// Show the mute state, or set it.
    Mute {
        #[arg(value_enum)]
        state: Option<MuteArg>,
    },
    /// Turn the TV on (Wake-on-LAN, plus SmartThings if linked).
    On {
        /// Give up after this many seconds.
        #[arg(long, default_value_t = 25)]
        timeout: u64,
    },
    /// Turn the TV off.
    Off,
    /// Cast a local file or an http(s) URL; stays attached until playback ends (Ctrl-C stops it).
    Cast(CastArgs),
    /// "Can the network carry this?": prints demand, capacity and the verdict.
    Netcheck(NetcheckArgs),
    /// Measure the network capacity (shows a test pattern on the TV for ~10 s).
    Nettest {
        /// Confirm: a test pattern will play on the TV (and the first run encodes a probe clip, ~1 min).
        #[arg(long)]
        yes: bool,
    },
    /// Pause what is casting (this TV's media renderer).
    Pause,
    /// Resume a paused cast.
    Play,
    /// Pause when playing, resume when paused.
    Toggle,
    /// Stop the cast.
    Stop,
    /// Seek (not possible in live streams such as TV-friendly casts).
    Seek {
        /// Position as HH:MM:SS, MM:SS or seconds; +SECS / -SECS (or +M:SS) to move
        /// from the current position; or a percentage like 50%.
        #[arg(value_parser = parse_seek, allow_hyphen_values = true, value_name = "POSITION")]
        position: SeekArg,
    },
    /// Find Samsung TVs / media renderers on the LAN (SSDP).
    Discover {
        /// Search time in seconds.
        #[arg(long, default_value_t = 3)]
        timeout: u64,
        /// Save the TV's new IP to the config if it changed.
        #[arg(long)]
        update: bool,
    },
    /// SmartThings cloud: OAuth login (app-import, login, refresh, logout, auth-status)
    /// and device commands (needs the OAuth login or a legacy smartthings_token).
    #[command(name = "smartthings", alias = "st")]
    SmartThings {
        #[command(subcommand)]
        cmd: StCommand,
    },
    /// Several TVs: list, detect (LAN + SmartThings), add, rename, remove, switch, link.
    Tvs {
        #[command(subcommand)]
        cmd: TvsCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum TvsCommand {
    /// Known TVs with their LAN and SmartThings status (`*` = active).
    #[command(alias = "ls")]
    List {
        /// Don't contact the TVs or SmartThings: config only.
        #[arg(long)]
        offline: bool,
    },
    /// Detect TVs on the LAN (SSDP + REST) and in SmartThings, and match them.
    /// Known TVs that changed IP (same MAC) get their address updated.
    Scan {
        /// LAN search time in seconds.
        #[arg(long, default_value_t = 3)]
        timeout: u64,
    },
    /// Add a TV: a number, IP, `st:<device id>` or name from the last `tvs scan`, or
    /// any IP address. The first command sent to a new TV asks for approval on its screen.
    Add {
        /// Candidate from `tvs scan`, or an IP address / host name.
        candidate: String,
        /// Display name (default: the SmartThings label or the TV's own name).
        #[arg(long)]
        name: Option<String>,
        /// Also make it the active TV.
        #[arg(long = "use")]
        make_active: bool,
    },
    /// Rename a TV.
    Rename {
        /// TV (id, name, host or prefix).
        tv: String,
        /// New display name.
        name: String,
    },
    /// Forget a TV (its pairing token is deleted).
    #[command(alias = "rm")]
    Remove {
        /// TV (id, name, host or prefix).
        tv: String,
    },
    /// Make a TV the active one (the default for every command, the TUI and the GUI).
    Use {
        /// TV (id, name, host or prefix).
        tv: String,
    },
    /// Link a TV to a SmartThings device (see `st devices` or `tvs scan`).
    Link {
        /// TV (id, name, host or prefix).
        tv: String,
        /// SmartThings device id (`st:` prefix allowed).
        device_id: String,
    },
    /// Unlink a TV from its SmartThings device (it will not be auto-linked again).
    Unlink {
        /// TV (id, name, host or prefix).
        tv: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum MuteArg {
    On,
    Off,
    Toggle,
}

#[derive(Debug, Subcommand)]
pub enum StCommand {
    /// The TV's SmartThings status (picture mode, input, ...).
    Status,
    /// All devices in the account.
    Devices,
    /// Set the picture mode by id or name, any case (see `status` for the list).
    PictureMode { mode: String },
    /// Switch the input source by id (HDMI1) or name, any case.
    Input { source: String },
    /// Set the sound mode by id or name, any case (see `status` for the list).
    SoundMode { mode: String },
    /// Import the OAuth app created with the SmartThings CLI (`apps:create -o FILE`).
    #[command(name = "app-import")]
    AppImport {
        /// Output file of `apps:create` (default: ~/.config/samsung-tv/st-app-created.json).
        file: Option<PathBuf>,
    },
    /// Log in to SmartThings (OAuth): opens the browser; after approving, paste the
    /// httpbin.org address it lands on (or just the code).
    Login {
        /// Kept for compatibility: pasting is now the default.
        #[arg(long, hide = true)]
        paste: bool,
        /// Wait for the redirect on 127.0.0.1:47811 instead of pasting. SmartThings
        /// answers "403 Access forbidden to this app" for localhost redirects on some
        /// accounts (seen 2026-10), so this is opt-in.
        #[arg(long, conflicts_with = "paste")]
        local: bool,
    },
    /// Refresh the OAuth tokens now (run daily by the systemd user timer). Exits 4 if
    /// a new login is needed.
    Refresh {
        /// Print nothing on success (errors still go to stderr).
        #[arg(long)]
        quiet: bool,
    },
    /// Forget the OAuth tokens (keeps the imported app, so `login` works again).
    Logout,
    /// Connected? Access-token expiry, refresh-token age, PAT fallback.
    #[command(name = "auth-status")]
    AuthStatus,
}

/// Track and quality options shared by `cast` and `netcheck`.
#[derive(Debug, Clone, Default, Args)]
pub struct ProfileArgs {
    /// Audio stream to send (absolute ffprobe index, listed by `netcheck`); implies TV-friendly.
    #[arg(long, value_name = "N")]
    pub audio: Option<u32>,
    /// Text subtitle stream to keep as a soft track (absolute index); implies TV-friendly.
    #[arg(long, value_name = "N")]
    pub sub: Option<u32>,
    /// Re-encode the video to this bitrate with VAAPI; implies TV-friendly.
    #[arg(long, value_name = "MBPS")]
    pub reduce: Option<f64>,
    /// TV-friendly mode (ffmpeg: one audio track, DTS → AC3; no seeking).
    #[arg(long)]
    pub friendly: bool,
    /// Skip the packet analysis (use the cached one or a provisional estimate).
    #[arg(long)]
    pub quick: bool,
}

#[derive(Debug, Clone, Args)]
pub struct CastArgs {
    /// File path or http(s) URL.
    pub source: String,
    #[command(flatten)]
    pub profile: ProfileArgs,
    /// Cast even if the network verdict is ❌ or the file looks unsupported.
    #[arg(long)]
    pub force: bool,
    /// Title shown on the TV.
    #[arg(long)]
    pub title: Option<String>,
}

#[derive(Debug, Clone, Args)]
pub struct NetcheckArgs {
    /// Media file.
    pub file: PathBuf,
    #[command(flatten)]
    pub profile: ProfileArgs,
}

// ---------------------------------------------------------------------------- plumbing

/// A CLI failure: message + exit code.
#[derive(Debug)]
pub struct Failure {
    pub code: i32,
    pub msg: String,
}

impl Failure {
    pub fn new(code: i32, msg: impl Into<String>) -> Failure {
        Failure {
            code,
            msg: msg.into(),
        }
    }
}

impl From<Error> for Failure {
    fn from(e: Error) -> Failure {
        let code = if matches!(e, Error::SmartThingsLoginExpired) { EXIT_RELOGIN } else { 1 };
        Failure::new(code, e.to_string())
    }
}

type CliResult = Result<(), Failure>;

/// Exit code used when the user interrupts with Ctrl-C.
pub const EXIT_INTERRUPTED: i32 = 130;
/// Exit code for a refused ❌ verdict / unsupported file.
pub const EXIT_REFUSED: i32 = 3;
/// Exit code for a missing confirmation (`nettest` without `--yes`).
pub const EXIT_USAGE: i32 = 2;
/// Exit code when the SmartThings OAuth login expired (`smartthings login` again).
pub const EXIT_RELOGIN: i32 = 4;

static INTERRUPTED: AtomicBool = AtomicBool::new(false);
static INTERRUPTS: AtomicU32 = AtomicU32::new(0);

/// Ctrl-C: the first press asks the running command to stop cleanly; a second press exits.
fn install_ctrlc() {
    let _ = ctrlc::set_handler(|| {
        INTERRUPTED.store(true, Ordering::SeqCst);
        if INTERRUPTS.fetch_add(1, Ordering::SeqCst) >= 1 {
            eprintln!("\nInterrupted.");
            std::process::exit(EXIT_INTERRUPTED);
        }
    });
}

fn interrupted() -> bool {
    INTERRUPTED.load(Ordering::SeqCst)
}

fn print_json(v: &serde_json::Value) {
    println!("{}", serde_json::to_string_pretty(v).unwrap_or_default());
}

fn to_json<T: serde::Serialize>(v: &T) -> serde_json::Value {
    serde_json::to_value(v).unwrap_or(serde_json::Value::Null)
}

/// Open the fleet (reads the config; no network traffic). A missing file starts empty
/// and is created on the first change (`tvs add`, `smartthings app-import`, …), with or
/// without `--config`; for `--config` a note says so, in case the path has a typo.
pub fn open_fleet(config: Option<&Path>) -> Result<Fleet, Failure> {
    let r = match config {
        Some(p) => {
            if !p.exists() {
                eprintln!("note: {} does not exist yet; it is created on the first change", p.display());
            }
            Fleet::open_or_default(p)
        }
        None => Fleet::open_default(),
    };
    r.map_err(|e| Failure::new(1, format!("cannot load the config: {e}")))
}

/// Open the fleet and pick the TV: `--tv` if given, else the active one.
pub fn open_tv(config: Option<&Path>, sel: Option<&str>) -> Result<(Fleet, Tv), Failure> {
    let fleet = open_fleet(config)?;
    let tv = match sel {
        Some(s) => fleet.tv_by(s).map_err(|e| Failure::new(EXIT_USAGE, e.to_string()))?,
        None => fleet.active(),
    };
    Ok((fleet, tv))
}

/// Run a subcommand; returns the exit code.
pub fn run(cli: Cli, cmd: Command) -> i32 {
    install_ctrlc();
    // `tvs` subcommands take their own TV argument; `--tv` is ignored there.
    let sel = if matches!(cmd, Command::Tvs { .. }) { None } else { cli.tv.as_deref() };
    let (fleet, tv) = match open_tv(cli.config.as_deref(), sel) {
        Ok(x) => x,
        Err(f) => {
            eprintln!("error: {}", f.msg);
            return f.code;
        }
    };
    let res = match cmd {
        Command::Tvs { cmd } => cmd_tvs(&fleet, cmd, cli.json),
        cmd => dispatch(&tv, cmd, cli.json),
    };
    match res {
        Ok(()) => 0,
        Err(f) => {
            if !f.msg.is_empty() {
                eprintln!("error: {}", f.msg);
            }
            f.code
        }
    }
}

fn dispatch(tv: &Tv, cmd: Command, json: bool) -> CliResult {
    // Volume, mute and power also work through SmartThings for a linked TV.
    let cloud_ok = tv.cloud_reach().is_some()
        && matches!(cmd, Command::Volume { .. } | Command::Mute { .. } | Command::On { .. } | Command::Off);
    let needs_host = !cloud_ok
        && !matches!(
            cmd,
            Command::Discover { .. } | Command::Netcheck(_) | Command::SmartThings { .. } | Command::Tvs { .. }
        );
    if needs_host && tv.host().trim().is_empty() {
        return Err(Failure::new(
            1,
            if tv.id().is_empty() {
                "no TV configured: run `tv-remote-tui tvs scan` then `tvs add <n>` (or `discover --update`)".to_string()
            } else {
                format!("{} has no LAN address (cloud-only): run `tvs scan` on its network", tv.display_name())
            },
        ));
    }
    match cmd {
        Command::Info => cmd_info(tv, json),
        Command::Status => cmd_status(tv, json),
        Command::Key { key, hold } => {
            let k: tvremote_core::Key = key
                .parse()
                .map_err(|e: Error| Failure::new(EXIT_USAGE, e.to_string()))?;
            if matches!(k, tvremote_core::Key::Raw(_)) {
                eprintln!("note: {} is not a known key; sending it as-is", k.code());
            }
            match hold {
                Some(s) if s.is_finite() && s > 0.0 => {
                    tv.hold_key(&k, Duration::from_secs_f64(s.min(30.0)))?
                }
                Some(_) => {
                    return Err(Failure::new(
                        EXIT_USAGE,
                        "--hold needs a positive number of seconds",
                    ))
                }
                None => tv.send_key(&k)?,
            }
            println!("{}", k.code());
            Ok(())
        }
        Command::Text { text } => {
            tv.send_text(&text.join(" "))?;
            Ok(())
        }
        Command::Apps => cmd_apps(tv, json),
        Command::Launch { app_id } => {
            // Look up the app type for the right launch kind; fall back to DEEP_LINK.
            match tv.installed_apps() {
                Ok(apps) => match apps.iter().find(|a| a.app_id == app_id) {
                    Some(a) => {
                        tv.launch_app(a)?;
                        println!("Launched {}", a.name);
                    }
                    None => {
                        eprintln!("note: {app_id} is not in the installed app list; trying anyway");
                        tv.launch_app_id(&app_id, LaunchType::DeepLink, None)?;
                    }
                },
                Err(e) => {
                    eprintln!("note: could not read the app list ({e}); launching as DEEP_LINK");
                    tv.launch_app_id(&app_id, LaunchType::DeepLink, None)?;
                }
            }
            Ok(())
        }
        Command::Youtube { video } => {
            if tvremote_core::dial::parse_youtube_id(&video).is_none() {
                return Err(Failure::new(
                    EXIT_USAGE,
                    format!("not a YouTube URL or video id: {video}"),
                ));
            }
            match tv.youtube(&video)? {
                tvremote_core::YouTubeOpened::Video => println!("Opened on YouTube"),
                tvremote_core::YouTubeOpened::AppOnly => eprintln!(
                    "note: this TV doesn't accept YouTube video links; opened the YouTube app instead"
                ),
            }
            Ok(())
        }
        Command::Volume { level } => {
            match level {
                Some(v) => tv.set_volume(v)?,
                None => println!("{}", tv.volume()?),
            }
            Ok(())
        }
        Command::Mute { state } => {
            match state {
                None => println!("{}", if tv.mute()? { "on" } else { "off" }),
                Some(MuteArg::On) => tv.set_mute(true)?,
                Some(MuteArg::Off) => tv.set_mute(false)?,
                Some(MuteArg::Toggle) => {
                    let m = !tv.mute()?;
                    tv.set_mute(m)?;
                    println!("{}", if m { "on" } else { "off" });
                }
            }
            Ok(())
        }
        Command::On { timeout } => {
            let wol = tv.config().mac.is_some();
            let cloud = tv.config().smartthings_device_id.is_some() && tv.has_smartthings();
            eprintln!(
                "Waking the TV ({})…",
                match (wol, cloud) {
                    (true, true) => "Wake-on-LAN + SmartThings",
                    (false, true) => "SmartThings",
                    _ => "Wake-on-LAN",
                }
            );
            tv.power_on(Duration::from_secs(timeout))?;
            println!("on");
            Ok(())
        }
        Command::Off => {
            tv.power_off()?;
            Ok(())
        }
        Command::Cast(a) => cmd_cast(tv, a),
        Command::Netcheck(a) => cmd_netcheck(tv, a, json),
        Command::Nettest { yes } => cmd_nettest(tv, yes),
        Command::Pause => {
            println!("{}", if tv.pause()? { "paused" } else { "already paused" });
            Ok(())
        }
        Command::Play => {
            println!("{}", if tv.resume()? { "playing" } else { "already playing" });
            Ok(())
        }
        Command::Toggle => {
            let paused = tv.play_pause()? == tvremote_core::dlna::TransportState::Paused;
            println!("{}", if paused { "paused" } else { "playing" });
            Ok(())
        }
        Command::Stop => {
            tv.stop_cast()?;
            println!("stopped");
            Ok(())
        }
        Command::Seek { position } => {
            let to = match position {
                SeekArg::To(to) => tv.seek_to(to)?,
                SeekArg::By { delta, forward } => tv.seek_relative(delta, forward)?,
                SeekArg::Fraction(f) => tv.seek_fraction(f)?,
            };
            println!("{}", fmt_duration(to));
            Ok(())
        }
        Command::Discover { timeout, update } => cmd_discover(tv, timeout, update, json),
        Command::SmartThings { cmd } => cmd_smartthings(tv, cmd, json),
        Command::Tvs { .. } => Err(Failure::new(1, "internal: tvs is handled by run()")),
    }
}

// ---------------------------------------------------------------------------- tvs

fn lan_text(l: &LanState) -> &'static str {
    match l {
        LanState::Reachable => "reachable",
        LanState::Unreachable => "unreachable",
        LanState::NoHost => "no address",
    }
}

fn cloud_text(c: &CloudState) -> String {
    match c {
        CloudState::NotLinked => "–".into(),
        CloudState::Online => "online".into(),
        CloudState::Offline => "offline".into(),
        CloudState::Unknown(e) => format!("? ({e})"),
    }
}

fn power_text(p: &PowerState) -> String {
    match p {
        PowerState::On => "on".into(),
        PowerState::Standby => "standby".into(),
        PowerState::Off => "off".into(),
        PowerState::Unknown(s) => s.clone(),
    }
}

fn short_id(s: &str) -> String {
    if s.chars().count() > 13 {
        format!("{}…", s.chars().take(12).collect::<String>())
    } else {
        s.to_string()
    }
}

fn resolve(fleet: &Fleet, sel: &str) -> Result<String, Failure> {
    fleet.resolve(sel).map_err(|e| Failure::new(EXIT_USAGE, e.to_string()))
}

/// Find a candidate of the last scan: its 1-based number, key, IP, device id or name.
pub fn find_candidate<'a>(cands: &'a [Candidate], sel: &str) -> Option<&'a Candidate> {
    let s = sel.trim();
    if let Ok(n) = s.parse::<usize>() {
        if n >= 1 && n <= cands.len() && !s.contains('.') {
            return cands.get(n - 1);
        }
    }
    cands.iter().find(|c| c.matches(s))
}

fn cmd_tvs(fleet: &Fleet, cmd: TvsCommand, json: bool) -> CliResult {
    match cmd {
        TvsCommand::List { offline } => {
            let cfg = fleet.config();
            let health: Vec<_> = if offline { Vec::new() } else { fleet.health_all() };
            if json {
                let v: Vec<_> = cfg
                    .tvs
                    .iter()
                    .map(|e| {
                        let h = health.iter().find(|h| h.id == e.id);
                        json!({
                            "id": e.id, "name": e.display_name(), "host": e.host, "port": e.port,
                            "active": cfg.active.as_deref() == Some(e.id.as_str()),
                            "mac": e.mac, "model": e.model, "paired": e.token.is_some(),
                            "smartthings_device_id": e.smartthings_device_id,
                            "smartthings_unlinked": e.smartthings_unlinked,
                            "last_seen": to_json(&e.last_seen),
                            "lan": h.map(|h| to_json(&h.lan)),
                            "cloud": h.map(|h| to_json(&h.cloud)),
                            "power": h.map(|h| to_json(&h.power)),
                        })
                    })
                    .collect();
                print_json(&json!(v));
                return Ok(());
            }
            if cfg.tvs.is_empty() {
                println!("No TV configured. Run `tv-remote-tui tvs scan`, then `tvs add <n>`.");
                return Ok(());
            }
            println!("  {:<5} {:<20} {:<15} {:<11} {:<9} {:<8} SMARTTHINGS", "ID", "NAME", "HOST", "LAN", "CLOUD", "POWER");
            for e in &cfg.tvs {
                let mark = if cfg.active.as_deref() == Some(e.id.as_str()) { "*" } else { " " };
                let h = health.iter().find(|h| h.id == e.id);
                let (lan, cloud, power) = match h {
                    Some(h) => (lan_text(&h.lan).to_string(), cloud_text(&h.cloud), power_text(&h.power)),
                    None => ("–".into(), "–".into(), "–".into()),
                };
                println!(
                    "{mark} {:<5} {:<20} {:<15} {:<11} {:<9} {:<8} {}",
                    e.id,
                    ellipsize(&e.display_name(), 20),
                    if e.host.is_empty() { "–" } else { &e.host },
                    lan,
                    cloud,
                    power,
                    e.smartthings_device_id.as_deref().map(short_id).unwrap_or_else(|| "–".into()),
                );
            }
            if !fleet.has_smartthings() && !offline {
                println!("(no SmartThings token: cloud status unavailable)");
            }
            Ok(())
        }
        TvsCommand::Scan { timeout } => {
            if !json {
                eprintln!(
                    "Scanning the LAN ({}s){}…",
                    timeout.clamp(1, 30),
                    if fleet.has_smartthings() { " and SmartThings" } else { "" }
                );
            }
            let r = fleet.scan(Duration::from_secs(timeout.clamp(1, 30)));
            if json {
                print_json(&to_json(&r));
                return Ok(());
            }
            print_scan(&r);
            Ok(())
        }
        TvsCommand::Add { candidate, name, make_active } => {
            let last = Fleet::last_scan().map(|r| r.candidates).unwrap_or_default();
            let found = find_candidate(&last, &candidate).cloned();
            let id = match found {
                Some(c) => fleet.add_candidate(&c)?,
                None => {
                    let host = candidate.trim();
                    if host.starts_with("st:") || host.parse::<usize>().is_ok() {
                        return Err(Failure::new(
                            EXIT_USAGE,
                            format!("'{host}' is not in the last scan: run `tvs scan` first"),
                        ));
                    }
                    if let Some(e) = fleet.entries().into_iter().find(|e| e.host == host) {
                        // Already configured: nothing to probe or add (names/--use still apply).
                        eprintln!("note: {host} is already {} ({})", e.id, e.display_name());
                        e.id
                    } else {
                        match Fleet::probe_host(host) {
                            Some(lan) => {
                                let c = tvremote_core::fleet::merge_candidates(&fleet.entries(), &[lan], &[]);
                                fleet.add_candidate(&c[0])?
                            }
                            None => {
                                eprintln!("note: {host} did not answer on port 8001; added anyway");
                                fleet.add_host(host, None)?
                            }
                        }
                    }
                }
            };
            if let Some(n) = name {
                fleet.rename(&id, &n)?;
            }
            if make_active {
                fleet.set_active(&id)?;
            }
            let e = fleet.config().entry(&id).cloned();
            let Some(e) = e else { return Err(Failure::new(1, "the TV vanished from the config")) };
            if json {
                print_json(&json!({"id": e.id, "name": e.display_name(), "host": e.host,
                                   "smartthings_device_id": e.smartthings_device_id}));
                return Ok(());
            }
            println!("{}  {}  {}", e.id, e.display_name(), if e.host.is_empty() { "(cloud only)" } else { &e.host });
            if e.token.is_none() && !e.host.is_empty() {
                println!("The first remote command (e.g. `--tv {} key home`) shows an approval prompt on that TV.", e.id);
            }
            Ok(())
        }
        TvsCommand::Rename { tv, name } => {
            let id = resolve(fleet, &tv)?;
            fleet.rename(&id, &name)?;
            Ok(())
        }
        TvsCommand::Remove { tv } => {
            let id = resolve(fleet, &tv)?;
            let name = fleet.config().entry(&id).map(|e| e.display_name()).unwrap_or_default();
            fleet.remove(&id)?;
            println!("Removed {id} ({name})");
            Ok(())
        }
        TvsCommand::Use { tv } => {
            let id = resolve(fleet, &tv)?;
            fleet.set_active(&id)?;
            println!("Active TV: {id} ({})", fleet.tv(&id)?.display_name());
            Ok(())
        }
        TvsCommand::Link { tv, device_id } => {
            let id = resolve(fleet, &tv)?;
            let dev = device_id.trim().trim_start_matches("st:");
            fleet.link_smartthings(&id, dev)?;
            Ok(())
        }
        TvsCommand::Unlink { tv } => {
            let id = resolve(fleet, &tv)?;
            fleet.unlink_smartthings(&id)?;
            Ok(())
        }
    }
}

fn ellipsize(s: &str, n: usize) -> String {
    if s.chars().count() > n {
        format!("{}…", s.chars().take(n.saturating_sub(1)).collect::<String>())
    } else {
        s.to_string()
    }
}

fn print_scan(r: &tvremote_core::ScanReport) {
    if let Some(e) = &r.lan_error {
        eprintln!("LAN scan failed: {e}");
    }
    if let Some(e) = &r.cloud_error {
        eprintln!("SmartThings failed: {e}");
    }
    for h in &r.host_changes {
        println!("{}: address changed {} → {} (saved)", h.id, if h.old.is_empty() { "–" } else { &h.old }, h.new);
    }
    if r.candidates.is_empty() {
        println!("No TV found.");
        if !r.cloud_checked {
            println!("(no SmartThings token: LAN only)");
        }
        return;
    }
    println!(" {:>2}  {:<9} {:<20} {:<15} {:<15} {:<26} KNOWN", "#", "WHERE", "NAME", "IP", "MODEL", "SMARTTHINGS");
    for (i, c) in r.candidates.iter().enumerate() {
        let st = match (c.device_id(), c.matched_by) {
            (Some(d), Some(k)) => format!("{} ({k:?})", short_id(d)),
            (Some(d), None) => short_id(d),
            _ => "–".into(),
        };
        let online = c.cloud.as_ref().and_then(|c| c.online()).map(|o| if o { "" } else { " offline" }).unwrap_or("");
        println!(
            " {:>2}  {:<9} {:<20} {:<15} {:<15} {:<26} {}",
            i + 1,
            c.presence.label(),
            ellipsize(&c.name, 20),
            c.ip().unwrap_or("–"),
            ellipsize(c.model.as_deref().unwrap_or("–"), 15),
            format!("{st}{online}"),
            c.known.as_ref().map(|(id, _)| id.as_str()).unwrap_or("new"),
        );
    }
    if r.candidates.iter().any(|c| c.known.is_none()) {
        println!("Add one with `tv-remote-tui tvs add <#>`.");
    }
    if !r.cloud_checked {
        println!("(no SmartThings token: LAN only)");
    }
}

// ---------------------------------------------------------------------------- simple commands

fn opt(s: &Option<String>) -> &str {
    s.as_deref().unwrap_or("–")
}

fn cmd_info(tv: &Tv, json: bool) -> CliResult {
    let d = tv.device_info()?;
    if json {
        print_json(&d.raw);
        return Ok(());
    }
    println!("Name        {}", opt(&d.name));
    println!("Model       {} ({})", opt(&d.model_name), opt(&d.model));
    println!(
        "Power       {}",
        d.power_state
            .as_ref()
            .map(|p| format!("{p:?}"))
            .unwrap_or("–".into())
    );
    println!("Resolution  {}", opt(&d.resolution));
    println!("Network     {}", opt(&d.network_type));
    println!("IP          {}", d.ip.clone().unwrap_or_else(|| tv.host()));
    println!("Wi-Fi MAC   {}", opt(&d.wifi_mac));
    println!("OS          {} {}", opt(&d.os), opt(&d.version));
    Ok(())
}

fn cmd_status(tv: &Tv, json: bool) -> CliResult {
    let power = tv.power_state();
    let reachable = power != tvremote_core::PowerState::Off;
    let volume = if reachable { tv.volume().ok() } else { None };
    let muted = if reachable { tv.mute().ok() } else { None };
    let transport = if reachable { tv.transport().ok() } else { None };
    if json {
        print_json(&json!({
            "tv": tv.id(),
            "name": tv.display_name(),
            "host": tv.host(),
            "power": to_json(&power),
            "volume": volume,
            "muted": muted,
            "transport": transport.as_ref().map(|(t, p)| json!({
                "state": to_json(&t.state),
                "position_secs": p.position.map(|d| d.as_secs_f64()),
                "duration_secs": p.duration.map(|d| d.as_secs_f64()),
                "uri": p.track_uri,
            })),
        }));
        return Ok(());
    }
    println!("TV          {} ({}) {}", tv.display_name(), tv.id(), tv.host());
    println!("Power       {power:?}");
    match (volume, muted) {
        (Some(v), Some(true)) => println!("Volume      {v} (muted)"),
        (Some(v), _) => println!("Volume      {v}"),
        _ => println!("Volume      –"),
    }
    match transport {
        Some((t, p)) => {
            let pos = p.position.map(fmt_duration).unwrap_or("–".into());
            let dur = p.duration.map(fmt_duration).unwrap_or("–".into());
            println!("Renderer    {:?}  {pos} / {dur}", t.state);
            if !p.track_uri.is_empty() {
                println!("Media       {}", p.track_uri);
            }
        }
        None => println!("Renderer    –"),
    }
    Ok(())
}

fn cmd_apps(tv: &Tv, json: bool) -> CliResult {
    let mut apps = tv.installed_apps()?;
    apps.sort_by_key(|a| a.name.to_lowercase());
    if json {
        print_json(&to_json(&apps));
        return Ok(());
    }
    for a in apps {
        println!("{:<16} {}", a.app_id, a.name);
    }
    Ok(())
}

fn cmd_discover(tv: &Tv, timeout: u64, update: bool, json: bool) -> CliResult {
    let t = Duration::from_secs(timeout.clamp(1, 30));
    if update {
        match tv.rediscover(t)? {
            Some(ip) => println!("TV found at {ip} (config host: {})", tv.host()),
            None => return Err(Failure::new(1, "no Samsung TV answered")),
        }
        return Ok(());
    }
    let found = tvremote_core::discovery::discover(t)?;
    if json {
        print_json(&to_json(&found));
        return Ok(());
    }
    if found.is_empty() {
        println!("Nothing answered.");
    }
    for d in found {
        let mark = if d.is_samsung_tv() { "TV " } else { "   " };
        println!(
            "{mark} {:<15} {}  {}",
            d.ip,
            d.server.as_deref().unwrap_or(""),
            d.location.as_deref().unwrap_or("")
        );
    }
    Ok(())
}

/// `1 h 05 min`, `12 min`, `3 days 4 h`.
fn fmt_span(secs: u64) -> String {
    match secs {
        0..=3599 => format!("{} min", secs / 60),
        3600..=172_799 => format!("{} h {:02} min", secs / 3600, (secs / 60) % 60),
        _ => format!("{} days {} h", secs / 86_400, (secs / 3600) % 24),
    }
}

fn cmd_st_auth(tv: &Tv, cmd: StCommand, json: bool) -> CliResult {
    use tvremote_core::smartthings_auth::{
        default_app_file, open_in_browser, random_state, CallbackListener, LOGIN_TIMEOUT, PASTE_REDIRECT,
    };
    let auth = StAuth::new(&tv.shared_config());
    match cmd {
        StCommand::AppImport { file } => {
            let file = file.unwrap_or_else(default_app_file);
            let app = auth.import_app(&file)?;
            println!(
                "Imported the SmartThings app {}{}.",
                app.app_name.as_deref().unwrap_or("(unnamed)"),
                app.app_id.as_deref().map(|a| format!(" ({a})")).unwrap_or_default()
            );
            println!("Next: tv-remote-tui smartthings login");
        }
        StCommand::Login { paste: _, local } => {
            if !auth.status().app_imported {
                return Err(Failure::new(
                    1,
                    "no OAuth app imported: run `tv-remote-tui smartthings app-import [FILE]` first \
                     (template: docs/smartthings-app.json)",
                ));
            }
            let state = random_state();
            let listener = if !local {
                None
            } else {
                match CallbackListener::bind() {
                    Ok(l) => Some(l),
                    Err(e) => {
                        eprintln!("note: cannot listen on 127.0.0.1:47811 ({e}); switching to the paste flow");
                        None
                    }
                }
            };
            let redirect = listener.as_ref().map(|l| l.redirect_uri()).unwrap_or_else(|| PASTE_REDIRECT.to_string());
            let url = auth.authorize_url(&state, &redirect)?;
            println!("Open this address, log in to Samsung, pick your location and approve:\n\n  {url}\n");
            if open_in_browser(&url) {
                eprintln!("(opening it in your browser)");
            }
            match listener {
                Some(l) => {
                    eprintln!("Waiting for the browser (up to 5 minutes; Ctrl-C to cancel)…");
                    match auth.login_with_listener(&l, &state, LOGIN_TIMEOUT, &INTERRUPTED) {
                        Err(Error::Cancelled) => return Err(Failure::new(EXIT_INTERRUPTED, "login cancelled")),
                        r => r?,
                    }
                }
                None => {
                    eprintln!(
                        "After approving, the browser lands on an httpbin.org page.\n\
                         Paste its address (or just the code) here and press Enter:"
                    );
                    let mut line = String::new();
                    std::io::stdin()
                        .read_line(&mut line)
                        .map_err(|e| Failure::new(1, format!("cannot read the pasted URL: {e}")))?;
                    auth.login_with_input(&line, Some(&state), PASTE_REDIRECT)?;
                }
            }
            println!("SmartThings connected (OAuth). Tokens refresh automatically; the daily timer keeps the login alive.");
        }
        StCommand::Refresh { quiet } => {
            let st = auth.status();
            if !st.connected {
                if st.needs_login {
                    return Err(Failure::new(EXIT_RELOGIN, Error::SmartThingsLoginExpired.to_string()));
                }
                if !quiet {
                    println!("SmartThings OAuth is not connected: nothing to refresh");
                }
                return Ok(());
            }
            auth.refresh()?;
            if !quiet {
                let st = auth.status();
                println!(
                    "Refreshed: access token valid {}",
                    st.access_expires_in.map(|s| fmt_span(s.max(0) as u64)).unwrap_or_else(|| "–".into())
                );
            }
        }
        StCommand::Logout => {
            auth.logout()?;
            println!("SmartThings OAuth tokens forgotten (the app stays imported; `smartthings login` to reconnect).");
        }
        StCommand::AuthStatus => {
            let st = auth.status();
            if json {
                print_json(&to_json(&st));
                return Ok(());
            }
            let method = match st.method {
                AuthMethod::OAuth => "OAuth",
                AuthMethod::Pat => "personal access token (legacy)",
                AuthMethod::None => "none",
            };
            println!("Status          {}", st.summary());
            println!("Using           {method}");
            println!(
                "OAuth app       {}",
                if st.app_imported {
                    format!("imported{}", st.app_id.as_deref().map(|a| format!(" ({a})")).unwrap_or_default())
                } else {
                    "not imported (smartthings app-import)".into()
                }
            );
            println!(
                "OAuth login     {}",
                if st.connected {
                    "connected"
                } else if st.needs_login {
                    "expired: run `smartthings login`"
                } else {
                    "not connected"
                }
            );
            if st.connected {
                println!(
                    "Access token    {}",
                    match st.access_expires_in {
                        Some(s) if s > 0 => format!("expires in {}", fmt_span(s as u64)),
                        Some(_) => "expired (refreshed on next use)".into(),
                        None => "none (fetched on next use)".into(),
                    }
                );
                println!(
                    "Refresh token   {}",
                    st.refresh_age
                        .map(|a| format!("issued {} (single-use, lasts ~30 days)", fmt_age(a)))
                        .unwrap_or_else(|| "–".into())
                );
            }
            println!("PAT fallback    {}", if st.pat_present { "present" } else { "none" });
            if let Some(sc) = &st.scope {
                println!("Scopes          {sc}");
            }
        }
        _ => return Err(Failure::new(1, "internal: not an auth command")),
    }
    Ok(())
}

fn cmd_smartthings(tv: &Tv, cmd: StCommand, json: bool) -> CliResult {
    if matches!(
        cmd,
        StCommand::AppImport { .. } | StCommand::Login { .. } | StCommand::Refresh { .. } | StCommand::Logout | StCommand::AuthStatus
    ) {
        return cmd_st_auth(tv, cmd, json);
    }
    if !tv.has_smartthings() {
        // Login expired → exit 4 with the "log in again" message.
        tv.smartthings()?;
        return Err(Failure::new(
            1,
            "SmartThings is not set up: `smartthings app-import` then `smartthings login` \
             (or set a personal access token in the GUI)",
        ));
    }
    let st = tv.smartthings()?;
    match cmd {
        StCommand::Devices => {
            let devs = st.devices()?;
            if json {
                print_json(&to_json(&devs));
            } else {
                for d in devs {
                    println!(
                        "{}  {}{}",
                        d.device_id,
                        d.label,
                        if d.is_tv() { "  [TV]" } else { "" }
                    );
                }
            }
        }
        StCommand::Status => {
            let dev = tv.smartthings_tv()?;
            let s = st.status(&dev.device_id)?;
            // SmartThings keeps serving the last reported values of an offline TV (e.g.
            // unplugged): say so, instead of presenting them as the current state.
            let health = st.health(&dev.device_id).ok();
            let offline = health.as_ref().is_some_and(|h| !h.is_online());
            if json {
                print_json(&json!({ "device": to_json(&dev), "health": to_json(&health), "status": to_json(&s) }));
                return Ok(());
            }
            println!("Device        {} ({})", dev.label, dev.device_id);
            match &health {
                Some(h) if h.is_online() => println!("Cloud         online"),
                Some(h) => println!(
                    "Cloud         {}{}: the TV is unreachable (unplugged or no network); the values below are the last it reported",
                    h.state.to_lowercase(),
                    h.last_updated.as_deref().map(|d| format!(" since {d}")).unwrap_or_default()
                ),
                None => println!("Cloud         ? (health unavailable)"),
            }
            if offline {
                println!("Power         {}", if s.power_on == Some(true) { "on (last known)" } else { "off" });
            }
            if !offline {
                println!(
                    "Power         {}",
                    s.power_on
                        .map(|b| if b { "on" } else { "off" })
                        .unwrap_or("–")
                );
            }
            println!(
                "Volume        {}",
                s.volume.map(|v| v.to_string()).unwrap_or("–".into())
            );
            println!(
                "Muted         {}",
                s.muted.map(|b| b.to_string()).unwrap_or("–".into())
            );
            println!("Input         {}", opt(&s.input_source));
            let list = |v: &[(String, String)]| {
                v.iter()
                    .map(|(id, name)| if id == name { id.clone() } else { format!("{name} ({id})") })
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            if !s.input_sources.is_empty() {
                println!("  available   {}", list(&s.input_sources));
            }
            println!("Picture mode  {}", opt(&s.picture_mode));
            if !s.picture_modes.is_empty() {
                println!("  available   {}", list(&s.picture_modes));
            }
            if s.sound_mode.is_some() || !s.sound_modes.is_empty() {
                println!("Sound mode    {}", opt(&s.sound_mode));
                println!("  available   {}", list(&s.sound_modes));
            }
            println!("Capabilities  {}", dev.capabilities.join(", "));
        }
        StCommand::PictureMode { mode } => {
            let dev = tv.smartthings_tv()?;
            st.set_picture_mode(&dev, &mode)?;
        }
        StCommand::Input { source } => {
            let dev = tv.smartthings_tv()?;
            st.set_input_source(&dev, &source)?;
        }
        StCommand::SoundMode { mode } => {
            let dev = tv.smartthings_tv()?;
            st.set_sound_mode(&dev, &mode)?;
        }
        StCommand::AppImport { .. }
        | StCommand::Login { .. }
        | StCommand::Refresh { .. }
        | StCommand::Logout
        | StCommand::AuthStatus => unreachable!("handled by cmd_st_auth"),
    }
    Ok(())
}

fn cmd_nettest(tv: &Tv, yes: bool) -> CliResult {
    if !yes {
        return Err(Failure::new(
            EXIT_USAGE,
            "the network test plays a noise test pattern on the TV for ~10 s (and the first run encodes \
             a 150 Mbps probe clip, about a minute). Re-run with --yes to start it.",
        ));
    }
    let tty = std::io::stderr().is_terminal();
    // Ctrl-C sets INTERRUPTED, which cancels the test: core stops the test pattern on
    // the TV and saves nothing. A second Ctrl-C exits at once.
    let res = tv.measure_network(&INTERRUPTED, &|p| {
        use netcheck::CapacityProgress as P;
        let line = match p {
            P::GeneratingClip(f) => format!("Encoding the probe clip: {:.0} %", f * 100.0),
            P::Starting => "Starting the test pattern on the TV…".into(),
            P::Measuring { elapsed, mbps } => format!("Measuring: {elapsed:.0} s, {mbps:.1} Mbps"),
            P::Done => "Done".into(),
        };
        if tty {
            eprint!("\r\x1b[2K{line}");
            let _ = std::io::stderr().flush();
        } else {
            eprintln!("{line}");
        }
    });
    if tty {
        eprintln!();
    }
    let m = match res {
        Ok(m) => m,
        Err(Error::Cancelled) => {
            return Err(Failure::new(
                EXIT_INTERRUPTED,
                "network test interrupted: the test pattern was stopped and nothing was saved",
            ))
        }
        Err(e) => return Err(e.into()),
    };
    println!(
        "Capacity {:.1} Mbps (best second {:.1} Mbps, {:.0} MB in {:.1} s)",
        m.mbps,
        m.peak_mbps,
        m.bytes as f64 / 1e6,
        m.secs
    );
    Ok(())
}

// ---------------------------------------------------------------------------- profile + verdict

/// Turn the CLI options into a cast mode for this file. Returns the mode and notes to print.
pub fn build_mode(
    info: &MediaInfo,
    p: &ProfileArgs,
    vaapi: &dyn Fn() -> bool,
    force: bool,
) -> Result<(CastMode, Vec<String>), Failure> {
    let mut notes = Vec::new();
    let report = check_compat(info);
    let mut friendly = p.friendly || p.audio.is_some() || p.sub.is_some() || p.reduce.is_some();
    // Point at the other languages (one track each), e.g. English on a dubbed remux.
    let languages = || {
        let mut langs: Vec<String> = Vec::new();
        for a in info.audio_streams() {
            let lang = a.language.clone().unwrap_or_else(|| "und".into());
            if !langs.iter().any(|l| l.starts_with(&format!("{lang} "))) {
                langs.push(format!("{lang} --audio {}", a.index));
            }
        }
        (langs.len() > 1).then(|| format!("Audio languages: {}", langs.join(", ")))
    };
    if !friendly && report.needs_tv_friendly() {
        // A direct cast would not play (DTS default track, HEVC in AVI, …).
        friendly = true;
        notes.push("This file needs TV-friendly mode for the TV to play it; using it.".into());
        if p.audio.is_none() {
            notes.extend(languages());
        }
    } else if !friendly && report.level == CompatLevel::NeedsTvFriendly {
        // ⚠ for another reason (an extra DTS track, an untested codec): the default
        // tracks play as they are, so keep the seekable direct mode.
        notes.push(
            "Casting directly: the default tracks should play as they are (see the ! lines); \
             --friendly or --audio N picks another track."
                .into(),
        );
        notes.extend(languages());
    }
    if report.level == CompatLevel::Unsupported && !force {
        let why = report
            .issues
            .first()
            .map(|i| i.message.clone())
            .unwrap_or_default();
        return Err(Failure::new(
            EXIT_REFUSED,
            format!("the TV cannot play this file: {why} (use --force to try anyway)"),
        ));
    }
    if !friendly {
        return Ok((CastMode::Direct, notes));
    }
    // ffmpeg's VAAPI probe is slow (~1 s): only run it when a re-encode is wanted.
    let have_vaapi = (report.needs_video_reencode || p.reduce.is_some()) && vaapi();
    let mut prof = CastProfile::recommended(info, have_vaapi);
    if let Some(a) = p.audio {
        match info.stream(a) {
            Some(s) if s.kind == StreamKind::Audio => prof.audio = Some(a),
            _ => {
                return Err(Failure::new(
                    EXIT_USAGE,
                    format!("stream {a} is not an audio stream (see `netcheck` for the list)"),
                ))
            }
        }
    }
    if let Some(s) = p.sub {
        match info.stream(s) {
            Some(x) if x.is_text_subtitle() => prof.subtitle = Some(s),
            Some(x) if x.kind == StreamKind::Subtitle => {
                return Err(Failure::new(
                    EXIT_USAGE,
                    format!(
                        "subtitle {s} is image-based ({}); only text subtitles can be kept",
                        x.codec
                    ),
                ))
            }
            _ => {
                return Err(Failure::new(
                    EXIT_USAGE,
                    format!("stream {s} is not a subtitle stream"),
                ))
            }
        }
    }
    if let Some(m) = p.reduce {
        if !(m.is_finite() && (1.0..=80.0).contains(&m)) {
            return Err(Failure::new(
                EXIT_USAGE,
                "--reduce must be between 1 and 80 Mbps",
            ));
        }
        prof.video = VideoMode::Reduce { target_mbps: m };
    }
    // `recommended` never picks Reduce without VAAPI; refuse a file that needs it.
    if !have_vaapi && (p.reduce.is_some() || (report.needs_video_reencode && !force)) {
        return Err(Failure::new(
            EXIT_REFUSED,
            "video reduction needs VAAPI (hevc_vaapi), which is not available on this PC",
        ));
    }
    if !have_vaapi && report.needs_video_reencode {
        notes.push("VAAPI is unavailable; copying the video as-is (--force).".into());
    }
    Ok((CastMode::TvFriendly(prof), notes))
}

/// Flags that reproduce a profile, e.g. `--audio 2 --reduce 30`.
pub fn profile_flags(p: &CastProfile) -> String {
    let mut v = Vec::new();
    if let Some(a) = p.audio {
        v.push(format!("--audio {a}"));
    }
    if let Some(s) = p.subtitle {
        v.push(format!("--sub {s}"));
    }
    if let VideoMode::Reduce { target_mbps } = p.video {
        v.push(format!("--reduce {target_mbps:.0}"));
    }
    if v.is_empty() {
        v.push("--friendly".into());
    }
    v.join(" ")
}

fn describe_mode(info: &MediaInfo, mode: &CastMode) -> String {
    match mode {
        CastMode::Direct => "direct (file as-is, all tracks, seekable)".into(),
        CastMode::TvFriendly(p) => {
            let audio = p
                .audio
                .and_then(|a| info.stream(a))
                .map(crate::castflow::audio_label)
                .unwrap_or_else(|| "none".into());
            let video = crate::castflow::video_label(&p.video);
            let sub = p
                .subtitle
                .and_then(|s| info.stream(s))
                .map(|s| s.label())
                .unwrap_or_else(|| "none".into());
            format!("TV-friendly: audio {audio}; video {video}; subtitles {sub}")
        }
    }
}

/// Packet profile for the verdict: cache, else analysis (unless `quick`).
fn packets_for(info: &MediaInfo, quick: bool) -> Result<Option<PacketProfile>, Failure> {
    if let Some(p) = netcheck::load_cached_profile(&info.path) {
        return Ok(Some(p));
    }
    if quick {
        return Ok(None);
    }
    let tty = std::io::stderr().is_terminal();
    let cancel = AtomicBool::new(false);
    let last = std::sync::Mutex::new(Instant::now() - Duration::from_secs(10));
    let done = AtomicBool::new(false);
    let res = std::thread::scope(|s| {
        s.spawn(|| {
            while !done.load(Ordering::SeqCst) {
                if interrupted() {
                    cancel.store(true, Ordering::SeqCst);
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        });
        let r = netcheck::analyze_packets(&info.path, info.duration_secs, &cancel, &|p| {
            let mut l = last.lock().unwrap_or_else(|e| e.into_inner());
            if tty {
                eprint!(
                    "\r\x1b[2KAnalysing packets: {:.0} %  (Ctrl-C to stop; --quick skips this)",
                    p * 100.0
                );
                let _ = std::io::stderr().flush();
            } else if l.elapsed() >= Duration::from_secs(5) {
                eprintln!("Analysing packets: {:.0} %", p * 100.0);
                *l = Instant::now();
            }
        });
        done.store(true, Ordering::SeqCst);
        r
    });
    if tty {
        eprint!("\r\x1b[2K");
    }
    match res {
        Ok(p) => Ok(Some(p)),
        Err(Error::Cancelled) => Err(Failure::new(
            EXIT_INTERRUPTED,
            "interrupted (use --quick to skip the packet analysis)",
        )),
        Err(e) => {
            eprintln!("warning: packet analysis failed ({e}); using a provisional estimate");
            Ok(None)
        }
    }
}

/// `vaapi`: whether a re-encode is possible here (❌ instead of ⚠ when a file needs one
/// and VAAPI is missing, like the GUI and TUI badges).
fn print_streams(info: &MediaInfo, emoji: bool, vaapi: bool) {
    let rep = check_compat(info);
    println!(
        "{} {}",
        compat_badge(rep.effective_level(vaapi), emoji),
        info.path.display()
    );
    for s in &info.streams {
        let kind = match s.kind {
            StreamKind::Video => "video",
            StreamKind::Audio => "audio",
            StreamKind::Subtitle => "sub  ",
            _ => continue,
        };
        let extra = match s.kind {
            StreamKind::Audio if !tvremote_core::cast::is_tv_safe_audio(&s.codec) => {
                " → AC3 in TV-friendly mode"
            }
            StreamKind::Subtitle if !s.is_text_subtitle() => " (image-based, cannot be kept)",
            _ => "",
        };
        println!("  {kind} {}{extra}", s.label());
    }
    for i in &rep.issues {
        println!("  ! {}", i.message);
    }
}

struct VerdictOut {
    demand: tvremote_core::Demand,
    verdict: tvremote_core::Verdict,
    capacity: Option<netcheck::CapacityMeasurement>,
    link: Option<netcheck::LinkInfo>,
}

fn compute_verdict(
    tv: &Tv,
    info: &MediaInfo,
    mode: &CastMode,
    quick: bool,
) -> Result<VerdictOut, Failure> {
    let packets = packets_for(info, quick)?;
    // How the TV is connected (one quick REST call; unknown if the TV does not answer).
    let tv_net = tv.device_info().ok().and_then(|d| d.tv_network());
    let (demand, verdict) = tv.network_verdict(info, mode, packets.as_ref(), tv_net);
    Ok(VerdictOut {
        demand,
        verdict,
        capacity: netcheck::load_capacity(),
        link: netcheck::detect_link(),
    })
}

fn print_verdict(v: &VerdictOut, emoji: bool) {
    let d = &v.demand;
    if d.provisional {
        println!(
            "Demand    peak ~{:.1} Mbps (provisional: average {} × 1.5)",
            d.peak_mbps(),
            mbps(d.stats.avg_bps)
        );
    } else {
        println!(
            "Demand    peak {:.1} Mbps over 10 s, {} over 2 s, average {}",
            d.peak_mbps(),
            mbps(d.stats.peak_2s_bps),
            mbps(d.stats.avg_bps)
        );
    }
    match &v.capacity {
        Some(c) => println!(
            "Capacity  {:.1} Mbps, measured {}{}",
            c.mbps,
            fmt_age(netcheck::now_unix().saturating_sub(c.measured_at)),
            if v.verdict.capacity_stale && v.verdict.capacity_mbps.is_none() {
                " on another network connection: not used (run `nettest --yes` again)"
            } else if v.verdict.capacity_stale {
                " (stale: run `nettest --yes` again)"
            } else {
                ""
            }
        ),
        None => println!("Capacity  not measured (run `tv-remote-tui nettest --yes`)"),
    }
    if let Some(l) = &v.link {
        println!("Link      {}", l.describe());
    }
    println!(
        "Verdict   {} {}",
        verdict_badge(v.verdict.level, emoji),
        v.verdict.explanation
    );
    // Like the GUI and the TUI: a re-encode fix is only offered when VAAPI works here
    // (`cast --reduce` would refuse it with exit code 3).
    let fixes: Vec<_> = v.verdict.fixes.iter().filter(|f| !f.requires_vaapi || vaapi_available()).collect();
    if !fixes.is_empty() {
        println!("Fixes:");
        for f in fixes {
            let how = match &f.kind {
                FixKind::DropExtraTracks { profile } | FixKind::ReduceVideo { profile, .. } => {
                    format!("  [{}]", profile_flags(profile))
                }
                FixKind::CopyToTv => String::new(),
            };
            let vaapi = if f.requires_vaapi {
                " (needs VAAPI)"
            } else {
                ""
            };
            println!(
                "  {} {} → {:.1} Mbps{vaapi}{how}",
                verdict_badge(f.resulting_level, emoji),
                f.description,
                f.resulting_mbps
            );
        }
    }
}

fn cmd_netcheck(tv: &Tv, a: NetcheckArgs, json: bool) -> CliResult {
    let emoji = terminal_supports_emoji();
    let info = tv.probe_media(&a.file)?;
    let (mode, notes) = build_mode(&info, &a.profile, &vaapi_available, true)?;
    let v = compute_verdict(tv, &info, &mode, a.profile.quick)?;
    let report = check_compat(&info);
    // The VAAPI probe is slow (~1 s, cached): only needed when the file needs a re-encode.
    let vaapi = report.needs_video_reencode && vaapi_available();
    if json {
        print_json(&json!({
            "file": info.path,
            "compat": to_json(&report),
            "compat_level": to_json(&report.effective_level(vaapi)),
            "vaapi": vaapi,
            "mode": to_json(&mode),
            "demand": to_json(&v.demand),
            "verdict": to_json(&v.verdict),
            "capacity": to_json(&v.capacity),
            "link": to_json(&v.link),
        }));
        return Ok(());
    }
    print_streams(&info, emoji, vaapi);
    for n in notes {
        println!("note: {n}");
    }
    println!("Mode      {}", describe_mode(&info, &mode));
    print_verdict(&v, emoji);
    Ok(())
}

// ---------------------------------------------------------------------------- cast

fn cmd_cast(tv: &Tv, a: CastArgs) -> CliResult {
    let emoji = terminal_supports_emoji();
    let req = if is_url(&a.source) {
        eprintln!("Casting a URL: the TV fetches it itself, so there is no network check.");
        CastRequest {
            source: CastSource::Url(a.source.clone()),
            mode: CastMode::Direct,
            title: a.title.clone(),
            media: None,
        }
    } else {
        let path = PathBuf::from(&a.source);
        if !path.is_file() {
            return Err(Failure::new(1, format!("no such file: {}", path.display())));
        }
        let info = tv.probe_media(&path)?;
        let (mode, notes) = build_mode(&info, &a.profile, &vaapi_available, a.force)?;
        for n in notes {
            eprintln!("note: {n}");
        }
        eprintln!("Mode      {}", describe_mode(&info, &mode));
        let v = compute_verdict(tv, &info, &mode, a.profile.quick)?;
        print_verdict(&v, emoji);
        match v.verdict.level {
            VerdictLevel::Bad if !a.force => {
                return Err(Failure::new(
                    EXIT_REFUSED,
                    "the network verdict is ❌ (will stutter). Apply a fix above, or use --force to cast anyway",
                ))
            }
            VerdictLevel::Bad => eprintln!("warning: casting despite a ❌ verdict (--force)"),
            VerdictLevel::Risky => eprintln!("warning: playback might stutter in busy scenes"),
            VerdictLevel::Ok => {}
        }
        if interrupted() {
            return Err(Failure::new(EXIT_INTERRUPTED, "interrupted"));
        }
        CastRequest {
            source: CastSource::File(path),
            mode,
            title: a.title.clone(),
            media: Some(info),
        }
    };

    let events = tv.subscribe();
    eprintln!("Starting the cast…");
    let session = tv.cast(req)?;
    if interrupted() {
        let _ = session.stop();
        return Err(Failure::new(
            EXIT_INTERRUPTED,
            "interrupted; playback stopped",
        ));
    }
    let st = session.status();
    eprintln!(
        "Casting “{}” ({}). Ctrl-C stops playback.",
        st.title,
        if st.tv_friendly {
            "TV-friendly"
        } else {
            "direct"
        }
    );

    let tty = std::io::stderr().is_terminal();
    let mut last_print = Instant::now() - Duration::from_secs(60);
    loop {
        if interrupted() {
            if tty {
                eprintln!();
            }
            let r = session.stop();
            eprintln!("Stopped.");
            return r.map_err(Failure::from);
        }
        match events.recv_timeout(Duration::from_millis(250)) {
            Ok(Event::Cast(CastEvent::Stall {
                message,
                suggestion,
            })) => {
                if tty {
                    eprintln!();
                }
                eprintln!("warning: {message} {suggestion}");
            }
            Ok(Event::Cast(CastEvent::Error(e))) => {
                if tty {
                    eprintln!();
                }
                eprintln!("warning: {e}");
            }
            Ok(_) | Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => std::thread::sleep(Duration::from_millis(250)),
        }
        if let Some(outcome) = session.status().outcome {
            if tty {
                eprintln!();
            }
            return match outcome {
                CastOutcome::Ended => {
                    eprintln!("Playback finished.");
                    Ok(())
                }
                CastOutcome::Stopped => {
                    eprintln!("Playback stopped.");
                    Ok(())
                }
                CastOutcome::NeverStarted => Err(Failure::new(
                    1,
                    "the TV never started playing (format probably unsupported; try --friendly)",
                )),
                CastOutcome::Lost => Err(Failure::new(1, "lost contact with the TV")),
            };
        }
        let interval = if tty {
            Duration::from_millis(900)
        } else {
            Duration::from_secs(10)
        };
        if last_print.elapsed() >= interval {
            last_print = Instant::now();
            let s = session.status();
            let state = s
                .state
                .as_ref()
                .map(|t| format!("{t:?}"))
                .unwrap_or_else(|| "Loading".into());
            let pos = s.position.map(fmt_duration).unwrap_or("–".into());
            let dur = s.duration.map(fmt_duration).unwrap_or("–".into());
            let rate = s
                .last_mbps
                .map(|m| format!("  {m:.1} Mbps"))
                .unwrap_or_default();
            let line = format!("{state}  {pos} / {dur}{rate}");
            if tty {
                eprint!("\r\x1b[2K{line}");
                let _ = std::io::stderr().flush();
            } else {
                eprintln!("{line}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;
    use tvremote_core::cast::parse_ffprobe;

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(std::iter::once("tv-remote-tui").chain(args.iter().copied())).unwrap()
    }

    #[test]
    fn clap_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn parse_tv_flag_and_tvs_commands() {
        let c = parse(&["--tv", "chambre", "key", "home"]);
        assert_eq!(c.tv.as_deref(), Some("chambre"));
        let c = parse(&["volume", "20", "--tv", "tv2"]);
        assert_eq!(c.tv.as_deref(), Some("tv2"), "--tv is global");
        assert!(parse(&["--tv", "salon"]).command.is_none(), "TUI on a chosen TV");
        assert!(matches!(
            parse(&["tvs", "list"]).command,
            Some(Command::Tvs { cmd: TvsCommand::List { offline: false } })
        ));
        assert!(matches!(
            parse(&["tvs", "ls", "--offline", "--json"]).command,
            Some(Command::Tvs { cmd: TvsCommand::List { offline: true } })
        ));
        assert!(matches!(
            parse(&["tvs", "scan", "--timeout", "5"]).command,
            Some(Command::Tvs { cmd: TvsCommand::Scan { timeout: 5 } })
        ));
        match parse(&["tvs", "add", "2", "--name", "Chambre", "--use"]).command {
            Some(Command::Tvs { cmd: TvsCommand::Add { candidate, name, make_active } }) => {
                assert_eq!((candidate.as_str(), name.as_deref(), make_active), ("2", Some("Chambre"), true))
            }
            other => panic!("{other:?}"),
        }
        match parse(&["tvs", "rename", "tv1", "TV du salon"]).command {
            Some(Command::Tvs { cmd: TvsCommand::Rename { tv, name } }) => {
                assert_eq!((tv.as_str(), name.as_str()), ("tv1", "TV du salon"))
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(parse(&["tvs", "rm", "tv2"]).command, Some(Command::Tvs { cmd: TvsCommand::Remove { .. } })));
        assert!(matches!(parse(&["tvs", "use", "salon"]).command, Some(Command::Tvs { cmd: TvsCommand::Use { .. } })));
        match parse(&["tvs", "link", "tv1", "st:abc"]).command {
            Some(Command::Tvs { cmd: TvsCommand::Link { tv, device_id } }) => {
                assert_eq!((tv.as_str(), device_id.as_str()), ("tv1", "st:abc"))
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(parse(&["tvs", "unlink", "tv1"]).command, Some(Command::Tvs { cmd: TvsCommand::Unlink { .. } })));
        assert!(Cli::try_parse_from(["tv-remote-tui", "tvs", "link", "tv1"]).is_err());
        assert!(Cli::try_parse_from(["tv-remote-tui", "tvs"]).is_err());
    }

    #[test]
    fn candidates_by_number_ip_or_name() {
        let json = serde_json::json!([
            {"key": "lan:192.0.2.5", "name": "Salon", "model": null, "presence": "LanOnly", "matched_by": null, "known": null,
             "lan": {"ip": "192.0.2.5", "ssdp": null, "info": null}, "cloud": null},
            {"key": "st:dev-2", "name": "Chambre", "model": null, "presence": "CloudOnly", "matched_by": null, "known": null,
             "lan": null, "cloud": {"device": {"device_id": "dev-2", "label": "Chambre", "name": "", "ocf_device_type": null,
             "capabilities": []}, "health": null}}
        ]);
        let cands: Vec<Candidate> = serde_json::from_value(json).unwrap();
        assert_eq!(find_candidate(&cands, "1").unwrap().name, "Salon");
        assert_eq!(find_candidate(&cands, "2").unwrap().name, "Chambre");
        assert_eq!(find_candidate(&cands, "192.0.2.5").unwrap().name, "Salon");
        assert_eq!(find_candidate(&cands, "st:dev-2").unwrap().name, "Chambre");
        assert_eq!(find_candidate(&cands, "dev-2").unwrap().name, "Chambre");
        assert_eq!(find_candidate(&cands, "chambre").unwrap().name, "Chambre");
        assert!(find_candidate(&cands, "3").is_none());
        assert!(find_candidate(&cands, "192.0.2.9").is_none());
    }

    #[test]
    fn tvs_commands_work_offline_on_a_temp_config() {
        let dir = std::env::temp_dir().join(format!("tvremote-cli-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        std::fs::write(&path, r#"{"host":"192.0.2.169","port":8002,"name":"Ryoku","token":"33333333","mac":"AA:BB:CC:DD:EE:01"}"#).unwrap();
        let fleet = open_fleet(Some(&path)).unwrap();
        let id = fleet.add_host("192.0.2.170", Some("Chambre")).unwrap();
        cmd_tvs(&fleet, TvsCommand::Rename { tv: "tv1".into(), name: "Salon".into() }, false).unwrap();
        cmd_tvs(&fleet, TvsCommand::Use { tv: "cham".into() }, false).unwrap();
        cmd_tvs(&fleet, TvsCommand::Link { tv: "salon".into(), device_id: "st:dev-1".into() }, false).unwrap();
        cmd_tvs(&fleet, TvsCommand::List { offline: true }, true).unwrap();
        let (_, tv) = open_tv(Some(&path), None).unwrap();
        assert_eq!(tv.id(), id, "`tvs use` persisted");
        let (_, tv1) = open_tv(Some(&path), Some("salon")).unwrap();
        assert_eq!(tv1.config().token.as_deref(), Some("33333333"));
        assert_eq!(tv1.config().smartthings_device_id.as_deref(), Some("dev-1"));
        assert!(open_tv(Some(&path), Some("nope")).is_err());
        cmd_tvs(&fleet, TvsCommand::Unlink { tv: "tv1".into() }, false).unwrap();
        cmd_tvs(&fleet, TvsCommand::Remove { tv: "chambre".into() }, false).unwrap();
        assert_eq!(open_fleet(Some(&path)).unwrap().ids(), vec!["tv1".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn smartthings_auth_commands_offline() {
        let dir = std::env::temp_dir().join(format!("tvremote-cli-st-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        std::fs::write(&path, r#"{"tvs": [], "smartthings_token": "fake-pat"}"#).unwrap();
        let app_file = dir.join("st-app-created.json");
        std::fs::write(&app_file, r#"{"app": {"appName": "tv-remote-test", "appId": "app-9", "appType": "API_ONLY"},
            "oauthClientId": "fake-client", "oauthClientSecret": "fake-secret"}"#).unwrap();
        let (_, tv) = open_tv(Some(&path), None).unwrap();
        // Nothing to refresh before a login: the timer exits 0.
        cmd_smartthings(&tv, StCommand::Refresh { quiet: true }, false).unwrap();
        cmd_smartthings(&tv, StCommand::AppImport { file: Some(app_file) }, false).unwrap();
        cmd_smartthings(&tv, StCommand::AuthStatus, true).unwrap();
        cmd_smartthings(&tv, StCommand::Logout, false).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v["smartthings_oauth"]["app_id"], "app-9");
        assert_eq!(v["smartthings_token"], "fake-pat", "the PAT stays as a fallback");
        // A login that expired (refresh rejected earlier): refresh exits 4.
        let mut app = tvremote_core::AppConfig::load_from(&path).unwrap();
        app.smartthings_oauth.as_mut().unwrap().refreshed_at = Some(1);
        app.save_to(&path).unwrap();
        let (_, tv) = open_tv(Some(&path), None).unwrap();
        let err = cmd_smartthings(&tv, StCommand::Refresh { quiet: true }, false).unwrap_err();
        assert_eq!(err.code, EXIT_RELOGIN);
        assert_eq!(Failure::from(Error::SmartThingsLoginExpired).code, EXIT_RELOGIN);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_subcommand_means_tui() {
        assert!(parse(&[]).command.is_none());
    }

    #[test]
    fn parse_commands() {
        assert!(matches!(
            parse(&["key", "KEY_VOLUP"]).command,
            Some(Command::Key { hold: None, .. })
        ));
        assert!(
            matches!(parse(&["key", "home", "--hold"]).command, Some(Command::Key { hold: Some(h), .. }) if h == 1.0)
        );
        assert!(
            matches!(parse(&["key", "power", "--hold", "3"]).command, Some(Command::Key { hold: Some(h), .. }) if h == 3.0)
        );
        match parse(&["text", "hello", "world"]).command {
            Some(Command::Text { text }) => assert_eq!(text.join(" "), "hello world"),
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            parse(&["volume"]).command,
            Some(Command::Volume { level: None })
        ));
        assert!(matches!(
            parse(&["volume", "30"]).command,
            Some(Command::Volume { level: Some(30) })
        ));
        assert!(Cli::try_parse_from(["x", "volume", "101"]).is_err());
        assert!(matches!(
            parse(&["mute", "toggle"]).command,
            Some(Command::Mute {
                state: Some(MuteArg::Toggle)
            })
        ));
        assert!(Cli::try_parse_from(["x", "mute", "maybe"]).is_err());
        assert!(matches!(parse(&["seek", "1:02:03"]).command,
            Some(Command::Seek { position }) if position == SeekArg::To(Duration::from_secs(3723))));
        assert!(matches!(parse(&["seek", "+10"]).command,
            Some(Command::Seek { position: SeekArg::By { delta, forward: true } }) if delta == Duration::from_secs(10)));
        assert!(matches!(parse(&["seek", "-1:30"]).command,
            Some(Command::Seek { position: SeekArg::By { delta, forward: false } }) if delta == Duration::from_secs(90)));
        assert!(matches!(parse(&["seek", "-10", "--tv", "tv2"]).command,
            Some(Command::Seek { position: SeekArg::By { forward: false, .. } })));
        assert!(matches!(parse(&["seek", "50%"]).command,
            Some(Command::Seek { position: SeekArg::Fraction(f) }) if f == 0.5));
        assert!(Cli::try_parse_from(["x", "seek", "150%"]).is_err());
        assert!(Cli::try_parse_from(["x", "seek", "-0"]).is_err(), "a no-op seek");
        assert!(Cli::try_parse_from(["x", "seek", "1e2"]).is_err());
        assert!(matches!(parse(&["toggle"]).command, Some(Command::Toggle)));
        assert!(matches!(parse(&["pause", "--tv", "tv2"]).command, Some(Command::Pause)));
        assert!(matches!(parse(&["play"]).command, Some(Command::Play)));
        assert!(Cli::try_parse_from(["x", "seek", "1:99:00"]).is_err());
        assert!(matches!(
            parse(&["nettest"]).command,
            Some(Command::Nettest { yes: false })
        ));
        assert!(matches!(
            parse(&["nettest", "--yes"]).command,
            Some(Command::Nettest { yes: true })
        ));
        assert!(matches!(
            parse(&["st", "picture-mode", "Movie"]).command,
            Some(Command::SmartThings {
                cmd: StCommand::PictureMode { .. }
            })
        ));
        assert!(matches!(
            parse(&["smartthings", "input", "HDMI1"]).command,
            Some(Command::SmartThings {
                cmd: StCommand::Input { .. }
            })
        ));
        assert!(matches!(
            parse(&["smartthings", "app-import"]).command,
            Some(Command::SmartThings { cmd: StCommand::AppImport { file: None } })
        ));
        assert!(matches!(
            parse(&["st", "app-import", "/tmp/x.json"]).command,
            Some(Command::SmartThings { cmd: StCommand::AppImport { file: Some(_) } })
        ));
        assert!(matches!(
            parse(&["smartthings", "login"]).command,
            Some(Command::SmartThings { cmd: StCommand::Login { paste: false, local: false } })
        ));
        assert!(matches!(
            parse(&["smartthings", "login", "--paste"]).command,
            Some(Command::SmartThings { cmd: StCommand::Login { paste: true, local: false } })
        ));
        assert!(matches!(
            parse(&["smartthings", "refresh", "--quiet"]).command,
            Some(Command::SmartThings { cmd: StCommand::Refresh { quiet: true } })
        ));
        assert!(matches!(
            parse(&["smartthings", "logout"]).command,
            Some(Command::SmartThings { cmd: StCommand::Logout })
        ));
        assert!(matches!(
            parse(&["smartthings", "auth-status"]).command,
            Some(Command::SmartThings { cmd: StCommand::AuthStatus })
        ));
        let c = parse(&["--json", "status"]);
        assert!(c.json);
        let c = parse(&["status", "--json"]);
        assert!(c.json);
    }

    #[test]
    fn parse_cast() {
        match parse(&[
            "cast",
            "/v/film.mkv",
            "--audio",
            "8",
            "--sub",
            "12",
            "--reduce",
            "30",
            "--force",
        ])
        .command
        {
            Some(Command::Cast(a)) => {
                assert_eq!(a.source, "/v/film.mkv");
                assert_eq!(a.profile.audio, Some(8));
                assert_eq!(a.profile.sub, Some(12));
                assert_eq!(a.profile.reduce, Some(30.0));
                assert!(a.force);
                assert!(!a.profile.friendly);
            }
            other => panic!("{other:?}"),
        }
        match parse(&["netcheck", "f.mkv", "--friendly", "--quick"]).command {
            Some(Command::Netcheck(a)) => assert!(a.profile.friendly && a.profile.quick),
            other => panic!("{other:?}"),
        }
    }

    fn remux() -> MediaInfo {
        let json = r#"{
          "format": {"format_name": "matroska,webm", "duration": "600.0", "size": "5000000000", "bit_rate": "66000000"},
          "streams": [
            {"index": 0, "codec_type": "video", "codec_name": "hevc", "width": 3840, "height": 2160,
             "avg_frame_rate": "24000/1001", "tags": {"BPS": "60000000"}, "disposition": {"default": 1}},
            {"index": 1, "codec_type": "audio", "codec_name": "dts", "profile": "DTS-HD MA",
             "tags": {"language": "eng"}, "disposition": {"default": 1}},
            {"index": 2, "codec_type": "audio", "codec_name": "ac3", "tags": {"language": "fre"}},
            {"index": 3, "codec_type": "subtitle", "codec_name": "subrip"},
            {"index": 4, "codec_type": "subtitle", "codec_name": "hdmv_pgs_subtitle"}
          ]}"#;
        parse_ffprobe(json, Path::new("/v/film.mkv")).unwrap()
    }

    #[test]
    fn mode_building() {
        let info = remux();
        let no_vaapi = || false;
        let yes_vaapi = || true;
        // DTS default → TV-friendly automatically.
        let (m, notes) = build_mode(&info, &ProfileArgs::default(), &no_vaapi, false).unwrap();
        assert!(matches!(m, CastMode::TvFriendly(ref p) if p.audio == Some(1)));
        assert!(notes[0].contains("needs TV-friendly"));
        // A second note lists the other audio languages when there are several.
        assert!(notes.len() <= 2);
        assert!(notes.iter().skip(1).all(|n| n.starts_with("Audio languages:")));

        let args = ProfileArgs {
            audio: Some(2),
            sub: Some(3),
            ..Default::default()
        };
        let (m, _) = build_mode(&info, &args, &no_vaapi, false).unwrap();
        let CastMode::TvFriendly(p) = m else { panic!() };
        assert_eq!((p.audio, p.subtitle), (Some(2), Some(3)));
        assert_eq!(profile_flags(&p), "--audio 2 --sub 3");

        let bad = |a: ProfileArgs| build_mode(&info, &a, &yes_vaapi, false).unwrap_err().code;
        assert_eq!(
            bad(ProfileArgs {
                audio: Some(0),
                ..Default::default()
            }),
            EXIT_USAGE
        );
        assert_eq!(
            bad(ProfileArgs {
                sub: Some(4),
                ..Default::default()
            }),
            EXIT_USAGE
        );
        assert_eq!(
            bad(ProfileArgs {
                reduce: Some(500.0),
                ..Default::default()
            }),
            EXIT_USAGE
        );

        let args = ProfileArgs {
            reduce: Some(30.0),
            ..Default::default()
        };
        assert_eq!(
            build_mode(&info, &args, &no_vaapi, true).unwrap_err().code,
            EXIT_REFUSED
        );
        let (m, _) = build_mode(&info, &args, &yes_vaapi, false).unwrap();
        let CastMode::TvFriendly(p) = m else { panic!() };
        assert_eq!(p.video, VideoMode::Reduce { target_mbps: 30.0 });
        assert_eq!(profile_flags(&p), "--audio 1 --reduce 30");
        assert_eq!(profile_flags(&CastProfile::default()), "--friendly");
    }

    #[test]
    fn an_extra_dts_track_does_not_force_tv_friendly() {
        // Default AC3 (French) plus a non-default DTS (English): ⚠, but the default
        // tracks play directly, so the seekable direct mode is kept (as in the GUI).
        let mut info = remux();
        info.streams[1].default = false;
        info.streams[2].default = true;
        assert_eq!(check_compat(&info).level, CompatLevel::NeedsTvFriendly);
        let (m, notes) = build_mode(&info, &ProfileArgs::default(), &|| false, false).unwrap();
        assert!(matches!(m, CastMode::Direct), "{m:?}");
        assert!(notes[0].starts_with("Casting directly"), "{notes:?}");
        assert!(notes.iter().any(|n| n.contains("eng --audio 1")), "{notes:?}");
        // Asking for the DTS track switches to TV-friendly (converted to AC3).
        let (m, _) = build_mode(&info, &ProfileArgs { audio: Some(1), ..Default::default() }, &|| false, false).unwrap();
        assert!(matches!(m, CastMode::TvFriendly(ref p) if p.audio == Some(1)));
    }
}
