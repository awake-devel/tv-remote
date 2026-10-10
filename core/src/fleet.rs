//! Several TVs: detection (LAN + SmartThings), matching, and the [`Fleet`] manager.
//!
//! * **Detection** ([`Fleet::scan`]): SSDP on the LAN, each answer enriched with the
//!   TV's REST `/api/v2/` info ([`LanTv`]); SmartThings devices where
//!   [`StDevice::is_tv`] ([`CloudTv`]). [`merge_candidates`] (pure) pairs them into one
//!   list of [`Candidate`]s labelled [`Presence::LanOnly`], [`Presence::CloudOnly`] or
//!   [`Presence::Both`], and recognises TVs already in the config.
//! * **Matching** cloud ↔ LAN, strongest key first ([`MatchKey`]): an existing link in
//!   the config; the MAC address (only if SmartThings exposes one); the OCF `di` vs the
//!   REST `duid`; model number (`ocf.modelNumber` / `ocf.mnmo` vs REST
//!   `device.modelName`) **and** name (`ocf.name` / `ocf.n` vs REST `device.name`);
//!   finally the model alone when it is unique on both sides. A device the user
//!   unlinked from a TV (`smartthings_unlinked`) is never paired with it again.
//! * **Management**: [`Fleet`] holds the shared config and hands out one [`Tv`] per
//!   entry (created lazily, so each remote WebSocket starts only when used).
//! * **Fallback** ([`plan_routes`], pure): which path (LAN or SmartThings) to try first
//!   for power, volume and mute, given what is known about each path.

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::config::{AppConfig, Config, SharedConfig, TvEntry};
use crate::discovery::{self, DiscoveredDevice};
use crate::error::{Error, Result};
use crate::events::{Broadcast, CloudState, Event, EventBus, TvEvent};
use crate::rest::{DeviceInfo, PowerState, Rest};
use crate::smartthings::{DeviceHealth, SmartThings, StDevice};
use crate::Tv;

// ---------------------------------------------------------------------------------------
// Detection results

/// A TV found on the LAN: its SSDP answer and, if it answered, its REST device info.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LanTv {
    /// IP address.
    pub ip: String,
    /// The SSDP answer, if found by SSDP.
    pub ssdp: Option<DiscoveredDevice>,
    /// `GET /api/v2/` info, if the TV answered.
    pub info: Option<DeviceInfo>,
}

impl LanTv {
    /// From an SSDP answer and the REST info.
    pub fn new(ssdp: Option<DiscoveredDevice>, ip: &str, info: Option<DeviceInfo>) -> LanTv {
        LanTv { ip: ip.to_string(), ssdp, info }
    }
    /// Name reported by the TV (`[TV] Samsung Q7F 85`).
    pub fn name(&self) -> Option<&str> {
        self.info.as_ref()?.name.as_deref()
    }
    /// Model number (`device.modelName`, e.g. `QE85Q7FAAUXXC`).
    pub fn model(&self) -> Option<&str> {
        self.info.as_ref()?.model_name.as_deref()
    }
    /// Wi-Fi MAC (`device.wifiMac`).
    pub fn mac(&self) -> Option<&str> {
        self.info.as_ref()?.wifi_mac.as_deref()
    }
    /// `device.duid`.
    pub fn duid(&self) -> Option<&str> {
        self.info.as_ref()?.duid.as_deref()
    }
    /// `device.PowerState` (`None` if REST did not answer).
    pub fn power(&self) -> Option<PowerState> {
        let i = self.info.as_ref()?;
        Some(i.power_state.clone().unwrap_or(PowerState::On))
    }
}

/// A TV of the SmartThings account.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloudTv {
    /// The device (with its OCF info).
    pub device: StDevice,
    /// `GET /devices/{id}/health`, if fetched.
    pub health: Option<DeviceHealth>,
}

impl CloudTv {
    /// Name used for matching: `ocf.name`, else the device name.
    pub fn tv_name(&self) -> Option<&str> {
        self.device.ocf.name.as_deref().or(Some(self.device.name.as_str())).filter(|s| !s.is_empty())
    }
    /// Model number (`ocf.modelNumber` / `ocf.mnmo`).
    pub fn model(&self) -> Option<&str> {
        self.device.ocf.model_number.as_deref()
    }
    /// Online according to the health endpoint (`None` = unknown).
    pub fn online(&self) -> Option<bool> {
        self.health.as_ref().map(DeviceHealth::is_online)
    }
}

/// Where a candidate was seen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Presence {
    /// Found on the LAN only.
    LanOnly,
    /// Found in SmartThings only.
    CloudOnly,
    /// Found on both, and matched.
    Both,
}

impl Presence {
    /// Short label: `LAN`, `cloud`, `LAN+cloud`.
    pub fn label(self) -> &'static str {
        match self {
            Presence::LanOnly => "LAN",
            Presence::CloudOnly => "cloud",
            Presence::Both => "LAN+cloud",
        }
    }
}

/// How a LAN TV and a SmartThings device were paired, strongest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum MatchKey {
    /// Already linked in the config (manual link or earlier match).
    Linked,
    /// Same MAC address.
    Mac,
    /// OCF `di` equals the REST `duid`.
    Uuid,
    /// Same model number and same name.
    ModelAndName,
    /// Same model number, the only one with that model on both sides.
    ModelOnly,
}

/// How a candidate was recognised as a TV already in the config.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum KnownBy {
    /// Same MAC address as the entry.
    Mac,
    /// Same host as the entry.
    Host,
    /// Its SmartThings device is linked to the entry.
    SmartThings,
}

/// One detected TV (merged from the LAN and SmartThings).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Candidate {
    /// Reference for `tvs add`: `lan:<ip>` or `st:<deviceId>`.
    pub key: String,
    /// Best display name (SmartThings label, else the TV's own name without `[TV] `).
    pub name: String,
    /// Model number, if known.
    pub model: Option<String>,
    /// LAN side.
    pub lan: Option<LanTv>,
    /// SmartThings side.
    pub cloud: Option<CloudTv>,
    /// LAN-only / cloud-only / both.
    pub presence: Presence,
    /// How LAN and cloud were paired (for [`Presence::Both`]).
    pub matched_by: Option<MatchKey>,
    /// The known TV (config id) this candidate is, and how it was recognised.
    pub known: Option<(String, KnownBy)>,
}

impl Candidate {
    /// IP on the LAN, if any.
    pub fn ip(&self) -> Option<&str> {
        self.lan.as_ref().map(|l| l.ip.as_str())
    }
    /// SmartThings device id, if any.
    pub fn device_id(&self) -> Option<&str> {
        self.cloud.as_ref().map(|c| c.device.device_id.as_str())
    }
    /// MAC from the LAN side, else from the cloud side.
    pub fn mac(&self) -> Option<&str> {
        self.lan.as_ref().and_then(LanTv::mac).or_else(|| self.cloud.as_ref().and_then(|c| c.device.mac.as_deref()))
    }
    /// Does `sel` designate this candidate? (its key, IP, device id, or name, any case)
    pub fn matches(&self, sel: &str) -> bool {
        let s = sel.trim();
        self.key == s
            || self.ip() == Some(s)
            || self.device_id() == Some(s)
            || self.device_id().is_some_and(|d| format!("st:{d}") == s)
            || self.name.eq_ignore_ascii_case(s)
    }
}

// ---------------------------------------------------------------------------------------
// Normalisation helpers (pure)

/// `aa:bb:cc:dd:ee:ff` lower-case, or `None` if not a MAC.
pub fn normalize_mac(s: &str) -> Option<String> {
    let b = crate::wol::parse_mac(s).ok()?;
    if b == [0; 6] {
        return None;
    }
    Some(b.iter().map(|x| format!("{x:02x}")).collect::<Vec<_>>().join(":"))
}

/// Name for comparison: lower-case alphanumerics, without a leading `[TV]`.
pub fn normalize_name(s: &str) -> String {
    let t = s.trim();
    let t = t.strip_prefix("[TV]").or_else(|| t.strip_prefix("[tv]")).unwrap_or(t);
    crate::dlna::xml_unescape(t).chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

/// Display form of a TV's own name (`[TV] Samsung Q7F` → `Samsung Q7F`).
pub fn display_tv_name(s: &str) -> String {
    let t = s.trim();
    t.strip_prefix("[TV]").unwrap_or(t).trim().to_string()
}

fn norm_uuid(s: &str) -> String {
    s.trim().trim_start_matches("uuid:").to_ascii_lowercase()
}

fn model_parts(s: &str) -> Vec<String> {
    s.split(['|', '/', ' '])
        .map(|p| p.chars().filter(char::is_ascii_alphanumeric).collect::<String>().to_ascii_uppercase())
        .filter(|p| p.len() >= 4)
        .collect()
}

/// Same model number? Equal after normalisation (alphanumerics, upper-case), comparing
/// every `|`-separated part (SmartThings sometimes prefixes a platform code). Pure.
pub fn same_model(a: &str, b: &str) -> bool {
    let (pa, pb) = (model_parts(a), model_parts(b));
    pa.iter().any(|x| pb.contains(x))
}

fn key_for(lan: &LanTv, cloud: &CloudTv) -> Option<MatchKey> {
    if let (Some(a), Some(b)) = (lan.mac().and_then(normalize_mac), cloud.device.mac.as_deref().and_then(normalize_mac)) {
        return (a == b).then_some(MatchKey::Mac);
    }
    if let (Some(a), Some(b)) = (lan.duid(), cloud.device.ocf.di.as_deref()) {
        if norm_uuid(a) == norm_uuid(b) {
            return Some(MatchKey::Uuid);
        }
    }
    let model = matches!((lan.model(), cloud.model()), (Some(a), Some(b)) if same_model(a, b));
    let name = match (lan.name(), cloud.tv_name()) {
        (Some(a), Some(b)) => {
            let (a, b) = (normalize_name(a), normalize_name(b));
            !a.is_empty() && a == b
        }
        _ => false,
    };
    if model && name {
        Some(MatchKey::ModelAndName)
    } else if model {
        Some(MatchKey::ModelOnly)
    } else {
        None
    }
}

/// The known entry a LAN TV is: same MAC, else same host.
fn known_lan(known: &[TvEntry], lan: &LanTv) -> Option<(String, KnownBy)> {
    if let Some(m) = lan.mac().and_then(normalize_mac) {
        if let Some(e) = known.iter().find(|e| e.mac.as_deref().and_then(normalize_mac).as_deref() == Some(m.as_str())) {
            return Some((e.id.clone(), KnownBy::Mac));
        }
    }
    known
        .iter()
        .find(|e| !e.host.is_empty() && e.host == lan.ip)
        .map(|e| (e.id.clone(), KnownBy::Host))
}

fn known_cloud(known: &[TvEntry], cloud: &CloudTv) -> Option<(String, KnownBy)> {
    known
        .iter()
        .find(|e| e.smartthings_device_id.as_deref() == Some(cloud.device.device_id.as_str()))
        .map(|e| (e.id.clone(), KnownBy::SmartThings))
}

/// Merge LAN and SmartThings detections into one candidate list. Pure.
///
/// Pairs are made strongest key first (see [`MatchKey`]); a pair is refused when the
/// LAN TV is a known entry that is linked to another device or that unlinked this
/// device, or when the device is linked to another entry. Unpaired TVs become
/// LAN-only or cloud-only candidates. Order: LAN order, then cloud-only ones.
pub fn merge_candidates(known: &[TvEntry], lan: &[LanTv], cloud: &[CloudTv]) -> Vec<Candidate> {
    let lan_known: Vec<_> = lan.iter().map(|l| known_lan(known, l)).collect();
    let cloud_known: Vec<_> = cloud.iter().map(|c| known_cloud(known, c)).collect();
    let mut pair_of_lan: Vec<Option<(usize, MatchKey)>> = vec![None; lan.len()];
    let mut cloud_used = vec![false; cloud.len()];

    let allowed = |i: usize, j: usize| -> bool {
        let dev = &cloud[j].device.device_id;
        if let Some((id, _)) = &lan_known[i] {
            let e = known.iter().find(|e| &e.id == id);
            if let Some(e) = e {
                if e.smartthings_unlinked.iter().any(|d| d == dev) {
                    return false;
                }
                if e.smartthings_device_id.as_ref().is_some_and(|d| d != dev) {
                    return false;
                }
            }
        }
        match (&cloud_known[j], &lan_known[i]) {
            (Some((a, _)), Some((b, _))) => a == b,
            (Some(_), None) => false,
            _ => true,
        }
    };

    // 1. Links already in the config.
    for j in 0..cloud.len() {
        let Some((cid, _)) = &cloud_known[j] else { continue };
        if let Some(i) = (0..lan.len()).find(|&i| pair_of_lan[i].is_none() && lan_known[i].as_ref().is_some_and(|(id, _)| id == cid)) {
            pair_of_lan[i] = Some((j, MatchKey::Linked));
            cloud_used[j] = true;
        }
    }
    // 2. Automatic keys, strongest first.
    for want in [MatchKey::Mac, MatchKey::Uuid, MatchKey::ModelAndName, MatchKey::ModelOnly] {
        for i in 0..lan.len() {
            if pair_of_lan[i].is_some() {
                continue;
            }
            let hits: Vec<usize> = (0..cloud.len())
                .filter(|&j| !cloud_used[j] && allowed(i, j) && key_for(&lan[i], &cloud[j]) == Some(want))
                .collect();
            if hits.len() != 1 {
                continue;
            }
            let j = hits[0];
            if want == MatchKey::ModelOnly {
                // The model must be unique among the unpaired LAN TVs too.
                let model = lan[i].model().unwrap_or_default();
                let others = (0..lan.len())
                    .filter(|&k| k != i && pair_of_lan[k].is_none() && lan[k].model().is_some_and(|m| same_model(m, model)))
                    .count();
                if others > 0 {
                    continue;
                }
            }
            pair_of_lan[i] = Some((j, want));
            cloud_used[j] = true;
        }
    }

    let mut out = Vec::new();
    for (i, l) in lan.iter().enumerate() {
        let c = pair_of_lan[i].map(|(j, k)| (cloud[j].clone(), k));
        let name = c
            .as_ref()
            .map(|(c, _)| c.device.label.clone())
            .filter(|s| !s.trim().is_empty())
            .or_else(|| l.name().map(display_tv_name))
            .unwrap_or_else(|| format!("Samsung TV ({})", l.ip));
        let model = l.model().map(str::to_string).or_else(|| c.as_ref().and_then(|(c, _)| c.model().map(str::to_string)));
        let known = lan_known[i].clone().or_else(|| pair_of_lan[i].and_then(|(j, _)| cloud_known[j].clone()));
        out.push(Candidate {
            key: format!("lan:{}", l.ip),
            name,
            model,
            presence: if c.is_some() { Presence::Both } else { Presence::LanOnly },
            matched_by: c.as_ref().map(|(_, k)| *k),
            cloud: c.map(|(c, _)| c),
            lan: Some(l.clone()),
            known,
        });
    }
    for (j, c) in cloud.iter().enumerate() {
        if cloud_used[j] {
            continue;
        }
        let name = [Some(c.device.label.as_str()), c.tv_name().map(|n| n.trim_start_matches("[TV]").trim())]
            .into_iter()
            .flatten()
            .find(|s| !s.trim().is_empty())
            .unwrap_or("SmartThings TV")
            .to_string();
        out.push(Candidate {
            key: format!("st:{}", c.device.device_id),
            name,
            model: c.model().map(str::to_string),
            lan: None,
            cloud: Some(c.clone()),
            presence: Presence::CloudOnly,
            matched_by: None,
            known: cloud_known[j].clone(),
        });
    }
    out
}

/// A host change applied by a scan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostChange {
    /// TV id.
    pub id: String,
    /// Previous host.
    pub old: String,
    /// New host.
    pub new: String,
}

/// Apply what a scan learned to the known TVs. Pure (`now` = Unix seconds).
///
/// * a known TV recognised **by MAC** at another IP gets its host updated;
/// * known TVs get `last_seen` (LAN, cloud when online, IP) and a missing MAC/model;
/// * a known TV paired with a SmartThings device it is not linked to yet (and did not
///   unlink) is linked to it, unless that device is linked to another TV.
pub fn apply_scan(app: &mut AppConfig, cands: &[Candidate], now: u64) -> Vec<HostChange> {
    let mut changes = Vec::new();
    for c in cands {
        let Some((id, by)) = &c.known else { continue };
        let linked_elsewhere = c
            .device_id()
            .is_some_and(|d| app.tvs.iter().any(|e| &e.id != id && e.smartthings_device_id.as_deref() == Some(d)));
        let Some(e) = app.entry_mut(id) else { continue };
        if let Some(l) = &c.lan {
            if *by == KnownBy::Mac && e.host != l.ip {
                changes.push(HostChange { id: id.clone(), old: e.host.clone(), new: l.ip.clone() });
                e.host = l.ip.clone();
            }
            e.last_seen.lan = Some(now);
            e.last_seen.ip = Some(l.ip.clone());
            if e.mac.is_none() {
                e.mac = l.mac().map(str::to_string);
            }
            if e.model.is_none() {
                e.model = l.model().map(str::to_string);
            }
        }
        if let Some(cl) = &c.cloud {
            if cl.online() == Some(true) {
                e.last_seen.cloud = Some(now);
            }
            let dev = &cl.device.device_id;
            if e.smartthings_device_id.is_none() && !linked_elsewhere && !e.smartthings_unlinked.contains(dev) {
                e.smartthings_device_id = Some(dev.clone());
            }
        }
    }
    changes
}

/// A new config entry for a candidate (id `id`). Pure.
pub fn entry_from_candidate(c: &Candidate, id: &str) -> TvEntry {
    let mut e = TvEntry::new(id, &c.name, c.ip().unwrap_or(""));
    e.mac = c.mac().map(str::to_string);
    e.model = c.model.clone();
    e.smartthings_device_id = c.device_id().map(str::to_string);
    e
}

// ---------------------------------------------------------------------------------------
// Fallback routing (pure)

/// An operation that can go over the LAN or through SmartThings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// Turn on.
    PowerOn,
    /// Turn off.
    PowerOff,
    /// Read the volume.
    GetVolume,
    /// Set the volume.
    SetVolume,
    /// Read the mute state.
    GetMute,
    /// Set the mute state.
    SetMute,
}

/// A path to the TV.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Route {
    /// Local network (REST / DLNA / WebSocket / WoL).
    Lan,
    /// SmartThings cloud.
    Cloud,
}

/// What is known about a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Reach {
    /// Answered recently.
    Yes,
    /// Failed recently.
    No,
    /// Not checked recently.
    Unknown,
}

/// The routes to try, in order, for `op`. `cloud` is `None` when no SmartThings device
/// is linked (or no token). Pure.
///
/// * No cloud → LAN only.
/// * LAN reachable or unknown → LAN first, cloud as fallback (unless the cloud is
///   known offline).
/// * LAN known unreachable → cloud first (unless known offline), then LAN (Wake-on-LAN
///   for power on; a retry for the rest). Power off with the LAN unreachable and the
///   cloud offline is LAN only (a no-op if the TV is off).
pub fn plan_routes(op: Op, lan: Reach, cloud: Option<Reach>) -> Vec<Route> {
    let Some(cloud) = cloud else { return vec![Route::Lan] };
    let cloud_ok = cloud != Reach::No;
    match (lan, cloud_ok) {
        (_, false) => vec![Route::Lan],
        (Reach::No, true) if op == Op::PowerOff => vec![Route::Cloud],
        (Reach::No, true) => vec![Route::Cloud, Route::Lan],
        (_, true) => vec![Route::Lan, Route::Cloud],
    }
}

// ---------------------------------------------------------------------------------------
// Status

/// LAN state of a TV.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LanState {
    /// The REST API answered (the TV is on or in network standby).
    Reachable,
    /// No answer.
    Unreachable,
    /// No host configured (cloud-only TV).
    NoHost,
}

/// Status of one TV (see [`Tv::health`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TvHealth {
    /// Local id.
    pub id: String,
    /// Display name.
    pub name: String,
    /// LAN reachability.
    pub lan: LanState,
    /// SmartThings state.
    pub cloud: CloudState,
    /// Power state: from REST when reachable, else from SmartThings `switch` when online.
    pub power: PowerState,
}

/// Unix time in seconds.
pub fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

// ---------------------------------------------------------------------------------------
// Fleet

/// What [`Fleet::scan`] found.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ScanReport {
    /// Merged candidates.
    pub candidates: Vec<Candidate>,
    /// LAN detection error, if it failed.
    pub lan_error: Option<String>,
    /// SmartThings error, if it failed (`None` also when no token is configured).
    pub cloud_error: Option<String>,
    /// Whether SmartThings was queried.
    pub cloud_checked: bool,
    /// Hosts updated for known TVs (matched by MAC).
    pub host_changes: Vec<HostChange>,
}

struct FleetInner {
    config: SharedConfig,
    bus: Broadcast<TvEvent>,
    tvs: Mutex<BTreeMap<String, Tv>>,
    /// The unscoped handle used before any TV exists (first run).
    placeholder: Mutex<Option<Tv>>,
    poll_stop: Mutex<Option<Arc<AtomicBool>>>,
}

impl Drop for FleetInner {
    fn drop(&mut self) {
        if let Ok(mut p) = self.poll_stop.lock() {
            if let Some(f) = p.take() {
                f.store(true, Ordering::SeqCst);
            }
        }
    }
}

/// All known TVs. Cheap to clone (clones share state); `Send + Sync`. No I/O on
/// creation; each TV's remote channel starts only when that [`Tv`] is used.
#[derive(Clone)]
pub struct Fleet {
    inner: Arc<FleetInner>,
}

impl std::fmt::Debug for Fleet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Fleet").field("tvs", &self.ids()).finish()
    }
}

impl Fleet {
    /// Load `~/.config/samsung-tv/config.json` (either format; see [`crate::config`]). A
    /// missing file gives an empty fleet bound to that path; an unreadable one is an
    /// [`Error::Config`] and is never overwritten.
    pub fn open_default() -> Result<Fleet> {
        Self::open_or_default(&Config::default_path())
    }

    /// Like [`Fleet::open_default`] for an explicit path.
    pub fn open_or_default(path: &Path) -> Result<Fleet> {
        Ok(Self::from_shared(SharedConfig::from_app(AppConfig::load_or_default(path)?, Some(path.to_path_buf()))))
    }

    /// Load a config file; a missing file is an error.
    pub fn open(path: &Path) -> Result<Fleet> {
        Ok(Self::from_shared(SharedConfig::from_app(AppConfig::load_from(path)?, Some(path.to_path_buf()))))
    }

    /// In memory (nothing is saved).
    pub fn new(cfg: AppConfig) -> Fleet {
        Self::from_shared(SharedConfig::from_app(cfg, None))
    }

    /// From a shared config (its scope is ignored).
    pub fn from_shared(config: SharedConfig) -> Fleet {
        let config = config.unscoped();
        Fleet {
            inner: Arc::new(FleetInner {
                config,
                bus: Broadcast::default(),
                tvs: Mutex::new(BTreeMap::new()),
                placeholder: Mutex::new(None),
                poll_stop: Mutex::new(None),
            }),
        }
    }

    /// Snapshot of the whole config. Contains tokens: never print them.
    pub fn config(&self) -> AppConfig {
        self.inner.config.app()
    }

    /// Backing file.
    pub fn path(&self) -> Option<PathBuf> {
        self.inner.config.path().map(Path::to_path_buf)
    }

    /// The shared config handle (unscoped).
    pub fn shared_config(&self) -> SharedConfig {
        self.inner.config.clone()
    }

    /// Known TVs in display order.
    pub fn entries(&self) -> Vec<TvEntry> {
        self.config().tvs
    }

    /// Ids of the known TVs.
    pub fn ids(&self) -> Vec<String> {
        self.entries().into_iter().map(|e| e.id).collect()
    }

    /// Id of the active TV.
    pub fn active_id(&self) -> Option<String> {
        self.config().active
    }

    /// Subscribe to the events of every TV, tagged with the TV id; fleet-wide events
    /// ([`Event::FleetChanged`]) have an empty id.
    pub fn subscribe(&self) -> Receiver<TvEvent> {
        self.inner.bus.subscribe()
    }

    fn changed(&self) {
        self.inner.bus.emit(TvEvent { tv: String::new(), event: Event::FleetChanged });
    }

    /// Resolve a selector (id, name, host, unique prefix) to a TV id.
    pub fn resolve(&self, sel: &str) -> Result<String> {
        Ok(self.config().resolve(sel)?.id.clone())
    }

    /// The handle of TV `id` (created on first use; clones share its connection).
    pub fn tv(&self, id: &str) -> Result<Tv> {
        if self.config().entry(id).is_none() {
            return Err(Error::Config(format!("no TV with id '{id}'")));
        }
        let mut g = self.inner.tvs.lock().map_err(|_| Error::Config("fleet lock poisoned".into()))?;
        Ok(g.entry(id.to_string())
            .or_insert_with(|| {
                Tv::with_bus(self.inner.config.scoped(id), EventBus::tagged(id, self.inner.bus.clone()))
            })
            .clone())
    }

    /// The handle of the TV named by `sel` (see [`Fleet::resolve`]).
    pub fn tv_by(&self, sel: &str) -> Result<Tv> {
        self.tv(&self.resolve(sel)?)
    }

    /// The active TV. With no TV configured yet (first run) this is a handle that
    /// follows the active TV: setting its host creates the first entry.
    pub fn active(&self) -> Tv {
        if let Some(id) = self.active_id() {
            if let Ok(tv) = self.tv(&id) {
                return tv;
            }
        }
        let mut g = match self.inner.placeholder.lock() {
            Ok(g) => g,
            Err(e) => e.into_inner(),
        };
        g.get_or_insert_with(|| Tv::with_bus(self.inner.config.clone(), EventBus::tagged("", self.inner.bus.clone())))
            .clone()
    }

    /// True if `tv` is the first-run handle of [`Fleet::active`] (not bound to a TV id).
    /// Once a TV exists, switch to [`Fleet::tv`] of the active id.
    pub fn is_placeholder(&self, tv: &Tv) -> bool {
        self.inner.placeholder.lock().ok().and_then(|g| g.clone()).is_some_and(|p| p.ptr_eq(tv))
    }

    /// Make TV `id` the active one.
    pub fn set_active(&self, id: &str) -> Result<()> {
        let id = id.to_string();
        self.inner.config.update_app(|a| {
            if a.entry(&id).is_some() {
                a.active = Some(id.clone());
                Ok(())
            } else {
                Err(Error::Config(format!("no TV with id '{id}'")))
            }
        })??;
        self.changed();
        Ok(())
    }

    /// Rename TV `id`.
    pub fn rename(&self, id: &str, name: &str) -> Result<()> {
        let name = name.trim().to_string();
        if name.is_empty() {
            return Err(Error::Config("empty name".into()));
        }
        self.edit(id, |e| e.name = name)
    }

    fn edit(&self, id: &str, f: impl FnOnce(&mut TvEntry)) -> Result<()> {
        let idc = id.to_string();
        self.inner.config.update_app(|a| match a.entry_mut(&idc) {
            Some(e) => {
                f(e);
                Ok(())
            }
            None => Err(Error::Config(format!("no TV with id '{idc}'"))),
        })??;
        if let Ok(tv) = self.tv(id) {
            tv.event_bus().emit(Event::ConfigChanged);
        }
        self.changed();
        Ok(())
    }

    /// Forget TV `id` (its remote channel and status poller stop; its pairing token is
    /// deleted). The first remaining TV becomes active if it was active.
    pub fn remove(&self, id: &str) -> Result<()> {
        let idc = id.to_string();
        self.inner.config.update_app(|a| {
            let n = a.tvs.len();
            a.tvs.retain(|e| e.id != idc);
            if a.tvs.len() == n {
                return Err(Error::Config(format!("no TV with id '{idc}'")));
            }
            if a.active.as_deref() == Some(idc.as_str()) {
                a.active = None;
            }
            Ok(())
        })??;
        if let Ok(mut g) = self.inner.tvs.lock() {
            if let Some(tv) = g.remove(id) {
                tv.stop_status_polling();
                tv.disconnect();
            }
        }
        self.changed();
        Ok(())
    }

    /// Add a detected TV; returns its new id. If the candidate is already known, its
    /// id is returned and nothing is added. The first TV becomes active.
    pub fn add_candidate(&self, c: &Candidate) -> Result<String> {
        if let Some((id, _)) = &c.known {
            if self.config().entry(id).is_some() {
                return Ok(id.clone());
            }
        }
        let c = c.clone();
        let id = self.inner.config.update_app(move |a| {
            let id = a.next_id();
            let mut e = entry_from_candidate(&c, &id);
            if let Some(d) = &e.smartthings_device_id {
                if a.tvs.iter().any(|t| t.smartthings_device_id.as_ref() == Some(d)) {
                    e.smartthings_device_id = None;
                }
            }
            e.last_seen.lan = c.lan.as_ref().map(|_| unix_now());
            e.last_seen.ip = c.ip().map(str::to_string);
            a.tvs.push(e);
            if a.active.is_none() {
                a.active = Some(id.clone());
            }
            id
        })?;
        self.changed();
        Ok(id)
    }

    /// Add a TV by address (no network I/O); returns its id. A TV with that host
    /// already known is returned as is.
    pub fn add_host(&self, host: &str, name: Option<&str>) -> Result<String> {
        let host = host.trim().to_string();
        if host.is_empty() {
            return Err(Error::Config("empty host".into()));
        }
        if let Some(e) = self.entries().into_iter().find(|e| e.host == host) {
            return Ok(e.id);
        }
        let name = name.map(str::to_string).unwrap_or_else(|| format!("Samsung TV ({host})"));
        let id = self.inner.config.update_app(move |a| {
            let id = a.next_id();
            a.tvs.push(TvEntry::new(&id, &name, &host));
            if a.active.is_none() {
                a.active = Some(id.clone());
            }
            id
        })?;
        self.changed();
        Ok(id)
    }

    /// Link TV `id` to SmartThings device `device_id` (manual override). The device is
    /// unlinked from any other TV.
    pub fn link_smartthings(&self, id: &str, device_id: &str) -> Result<()> {
        let dev = device_id.trim().to_string();
        if dev.is_empty() {
            return Err(Error::Config("empty SmartThings device id".into()));
        }
        let idc = id.to_string();
        self.inner.config.update_app(|a| {
            if a.entry(&idc).is_none() {
                return Err(Error::Config(format!("no TV with id '{idc}'")));
            }
            for e in a.tvs.iter_mut() {
                if e.id == idc {
                    e.smartthings_device_id = Some(dev.clone());
                    e.smartthings_unlinked.retain(|d| d != &dev);
                } else if e.smartthings_device_id.as_deref() == Some(dev.as_str()) {
                    e.smartthings_device_id = None;
                }
            }
            Ok(())
        })??;
        for tv in self.cached_tvs() {
            tv.forget_cloud_cache();
            tv.event_bus().emit(Event::ConfigChanged);
        }
        self.changed();
        Ok(())
    }

    /// Unlink TV `id` from its SmartThings device; that device is never auto-linked to
    /// this TV again (until linked manually).
    pub fn unlink_smartthings(&self, id: &str) -> Result<()> {
        self.edit(id, |e| {
            if let Some(d) = e.smartthings_device_id.take() {
                if !e.smartthings_unlinked.contains(&d) {
                    e.smartthings_unlinked.push(d);
                }
            }
        })?;
        if let Ok(tv) = self.tv(id) {
            tv.forget_cloud_cache();
        }
        Ok(())
    }

    fn cached_tvs(&self) -> Vec<Tv> {
        self.inner.tvs.lock().map(|g| g.values().cloned().collect()).unwrap_or_default()
    }

    // ----- SmartThings (account) ---------------------------------------------------------

    /// True if a SmartThings token is configured.
    pub fn has_smartthings(&self) -> bool {
        self.config().smartthings_token.is_some_and(|t| !t.trim().is_empty())
    }

    /// SmartThings client for the account token.
    pub fn smartthings(&self) -> Result<SmartThings> {
        let tok = self
            .config()
            .smartthings_token
            .ok_or_else(|| Error::SmartThings("no SmartThings token configured".into()))?;
        SmartThings::new(&tok)
    }

    /// Set or clear the account's SmartThings token.
    pub fn set_smartthings_token(&self, token: Option<String>) -> Result<()> {
        let token = token.map(|t| t.trim().to_string()).filter(|t| !t.is_empty());
        self.inner.config.update_app(|a| a.smartthings_token = token)?;
        for tv in self.cached_tvs() {
            tv.forget_cloud_cache();
            tv.event_bus().emit(Event::ConfigChanged);
        }
        self.changed();
        Ok(())
    }

    // ----- detection ---------------------------------------------------------------------

    /// SSDP search, then `GET /api/v2/` on every Samsung TV that answered (in parallel).
    pub fn scan_lan(timeout: Duration) -> Result<Vec<LanTv>> {
        let found: Vec<DiscoveredDevice> =
            discovery::discover(timeout)?.into_iter().filter(DiscoveredDevice::is_samsung_tv).collect();
        let handles: Vec<_> = found
            .into_iter()
            .map(|d| {
                std::thread::spawn(move || {
                    let ip = d.ip.to_string();
                    let info = Rest::new(&ip).device_info().ok();
                    LanTv::new(Some(d), &ip, info)
                })
            })
            .collect();
        Ok(handles.into_iter().filter_map(|h| h.join().ok()).collect())
    }

    /// Probe one address over REST (for `tvs add <ip>`); `None` if it does not answer.
    pub fn probe_host(host: &str) -> Option<LanTv> {
        let ip: Option<IpAddr> = host.parse().ok();
        let info = Rest::new(host).device_info().ok()?;
        Some(LanTv { ip: ip.map(|i| i.to_string()).unwrap_or_else(|| host.to_string()), ssdp: None, info: Some(info) })
    }

    /// The TVs of the SmartThings account, with health and OCF info (the status is
    /// fetched only when the device list lacks the model number).
    pub fn scan_cloud(&self) -> Result<Vec<CloudTv>> {
        let st = self.smartthings()?;
        let devs = st.tvs()?;
        let handles: Vec<_> = devs
            .into_iter()
            .map(|mut d| {
                let st = st.clone();
                std::thread::spawn(move || {
                    let health = st.health(&d.device_id).ok();
                    if d.ocf.model_number.is_none() || d.ocf.name.is_none() {
                        if let Ok(v) = st.status_json(&d.device_id) {
                            d.ocf.fill_from_status(&v);
                        }
                    }
                    CloudTv { device: d, health }
                })
            })
            .collect();
        Ok(handles.into_iter().filter_map(|h| h.join().ok()).collect())
    }

    /// Detect TVs on the LAN and in SmartThings (in parallel), merge them, apply host
    /// changes / last-seen / auto-links to known TVs (saved), and cache the result for
    /// [`Fleet::last_scan`]. Blocks about `timeout`.
    pub fn scan(&self, timeout: Duration) -> ScanReport {
        let lan_h = std::thread::spawn(move || Self::scan_lan(timeout));
        let cloud_checked = self.has_smartthings();
        let cloud = if cloud_checked { Some(self.scan_cloud()) } else { None };
        let lan = lan_h.join().unwrap_or_else(|_| Err(Error::Config("LAN scan crashed".into())));
        let mut report = ScanReport { cloud_checked, ..Default::default() };
        let lan = lan.unwrap_or_else(|e| {
            report.lan_error = Some(e.to_string());
            Vec::new()
        });
        let cloud = match cloud {
            Some(Ok(c)) => c,
            Some(Err(e)) => {
                report.cloud_error = Some(e.to_string());
                Vec::new()
            }
            None => Vec::new(),
        };
        report.candidates = merge_candidates(&self.entries(), &lan, &cloud);
        let cands = report.candidates.clone();
        let before = self.config();
        if let Ok(ch) = self.inner.config.update_app(|a| apply_scan(a, &cands, unix_now())) {
            report.host_changes = ch;
        }
        // Re-label known-ness after auto-links.
        report.candidates = merge_candidates(&self.entries(), &lan, &cloud);
        if self.config().tvs != before.tvs {
            for tv in self.cached_tvs() {
                tv.forget_cloud_cache();
                tv.event_bus().emit(Event::ConfigChanged);
            }
            self.changed();
        }
        save_last_scan(&report);
        report
    }

    /// The candidates of the last [`Fleet::scan`] (from `~/.cache/tv-remote/last-scan.json`).
    pub fn last_scan() -> Option<ScanReport> {
        let text = std::fs::read_to_string(crate::config::cache_dir().join("last-scan.json")).ok()?;
        serde_json::from_str(&text).ok()
    }

    // ----- status ------------------------------------------------------------------------

    /// [`Tv::health`] of every TV (in parallel).
    pub fn health_all(&self) -> Vec<TvHealth> {
        let handles: Vec<_> = self
            .ids()
            .into_iter()
            .filter_map(|id| self.tv(&id).ok())
            .map(|tv| std::thread::spawn(move || tv.health()))
            .collect();
        handles.into_iter().filter_map(|h| h.join().ok()).collect()
    }

    /// Start status polling for every TV: REST power every `interval` (per-TV
    /// [`Event::Power`]), SmartThings health every `cloud_interval` ([`Event::Cloud`]).
    /// TVs added later are picked up. No WebSocket is opened.
    pub fn start_status_polling(&self, interval: Duration, cloud_interval: Duration) {
        self.stop_status_polling();
        let flag = Arc::new(AtomicBool::new(false));
        if let Ok(mut g) = self.inner.poll_stop.lock() {
            *g = Some(flag.clone());
        }
        let weak = Arc::downgrade(&self.inner);
        let _ = std::thread::Builder::new().name("tv-fleet-poll".into()).spawn(move || {
            let mut last_cloud: Option<Instant> = None;
            while !flag.load(Ordering::SeqCst) {
                let Some(inner) = weak.upgrade() else { return };
                let fleet = Fleet { inner };
                for id in fleet.ids() {
                    if let Ok(tv) = fleet.tv(&id) {
                        if !tv.is_polling() && !tv.host().trim().is_empty() {
                            tv.start_status_polling(interval);
                        }
                    }
                }
                if fleet.has_smartthings() && last_cloud.is_none_or(|t| t.elapsed() >= cloud_interval) {
                    last_cloud = Some(Instant::now());
                    for id in fleet.ids() {
                        if let Ok(tv) = fleet.tv(&id) {
                            let _ = std::thread::spawn(move || tv.cloud_state());
                        }
                    }
                }
                drop(fleet);
                let until = Instant::now() + Duration::from_secs(2);
                while Instant::now() < until && !flag.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        });
    }

    /// Stop fleet polling (and each TV's poller).
    pub fn stop_status_polling(&self) {
        if let Ok(mut g) = self.inner.poll_stop.lock() {
            if let Some(f) = g.take() {
                f.store(true, Ordering::SeqCst);
            }
        }
        for tv in self.cached_tvs() {
            tv.stop_status_polling();
        }
    }
}

fn save_last_scan(r: &ScanReport) {
    if let Ok(text) = serde_json::to_string(r) {
        let _ = std::fs::write(crate::config::cache_dir().join("last-scan.json"), text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn fixture(name: &str) -> Value {
        let text = match name {
            "rest_salon" => include_str!("../fixtures/rest_salon.json"),
            "rest_chambre" => include_str!("../fixtures/rest_chambre.json"),
            "st_devices" => include_str!("../fixtures/st_devices.json"),
            "st_status_chambre" => include_str!("../fixtures/st_status_chambre.json"),
            "st_health_online" => include_str!("../fixtures/st_health_online.json"),
            _ => unreachable!(),
        };
        serde_json::from_str(text).unwrap()
    }

    fn lan(rest: &str) -> LanTv {
        let ssdp = discovery::parse_ssdp_response(
            include_str!("../fixtures/ssdp_salon.txt"),
            "192.0.2.169".parse().unwrap(),
        );
        let info = DeviceInfo::from_json(fixture(rest));
        let ip = info.ip.clone().unwrap();
        LanTv { ip, ssdp, info: Some(info) }
    }

    fn cloud_tvs() -> Vec<CloudTv> {
        let devs: Vec<StDevice> = fixture("st_devices")["items"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(StDevice::from_json)
            .filter(StDevice::is_tv)
            .collect();
        devs.into_iter()
            .map(|mut d| {
                if d.ocf.model_number.is_none() {
                    d.ocf.fill_from_status(&fixture("st_status_chambre"));
                }
                CloudTv { device: d, health: Some(DeviceHealth::from_json(&fixture("st_health_online"))) }
            })
            .collect()
    }

    #[test]
    fn fixtures_parse() {
        let l = lan("rest_salon");
        assert_eq!(l.ip, "192.0.2.169");
        assert_eq!(l.model(), Some("QE85Q7FAAUXXC"));
        assert!(l.ssdp.as_ref().unwrap().is_samsung_tv());
        let c = cloud_tvs();
        assert_eq!(c.len(), 3, "the fridge is not a TV");
        assert_eq!(c[0].model(), Some("QE85Q7FAAUXXC"));
        assert_eq!(c[1].model(), Some("UE55TU7000"), "filled from the ocf capability");
        assert_eq!(c[1].device.ocf.di.as_deref(), Some("aaaaaaaa-0000-0000-0000-000000000002"));
        assert_eq!(c[1].online(), Some(true));
        assert_eq!(c[2].device.mac.as_deref(), Some("AA:BB:CC:DD:EE:03"));
        let st = crate::smartthings::TvStatus::from_status_json(&fixture("st_status_chambre"));
        assert_eq!(st.thing_status.as_deref(), Some("Idle"));
    }

    #[test]
    fn merge_matches_by_model_name_uuid_and_mac() {
        let lans = vec![lan("rest_salon"), lan("rest_chambre")];
        let clouds = cloud_tvs();
        let c = merge_candidates(&[], &lans, &clouds);
        assert_eq!(c.len(), 3);
        // Salon: same model and same name on both sides.
        assert_eq!(c[0].presence, Presence::Both);
        assert_eq!(c[0].matched_by, Some(MatchKey::ModelAndName));
        assert_eq!(c[0].name, "Salon", "the SmartThings label wins");
        assert_eq!(c[0].key, "lan:192.0.2.169");
        // Chambre: OCF di == REST duid.
        assert_eq!(c[1].presence, Presence::Both);
        assert_eq!(c[1].matched_by, Some(MatchKey::Uuid));
        // The third cloud TV (MAC that matches no LAN TV) stays cloud-only.
        assert_eq!(c[2].presence, Presence::CloudOnly);
        assert!(c[2].key.starts_with("st:"));
        assert!(c.iter().all(|x| x.known.is_none()));

        // MAC wins when both sides expose one.
        let mut l3 = lan("rest_chambre");
        if let Some(i) = l3.info.as_mut() {
            i.wifi_mac = Some("aa-bb-cc-dd-ee-03".into());
            i.duid = None;
            i.model_name = Some("OTHER".into());
        }
        let c = merge_candidates(&[], &[l3], &clouds);
        assert_eq!(c.iter().find(|x| x.presence == Presence::Both).unwrap().matched_by, Some(MatchKey::Mac));
    }

    #[test]
    fn model_only_needs_uniqueness() {
        let mut a = lan("rest_salon");
        let mut b = lan("rest_salon");
        b.ip = "192.0.2.170".into();
        for l in [&mut a, &mut b] {
            l.info.as_mut().unwrap().name = Some("Other".into());
        }
        let clouds = vec![cloud_tvs().remove(0)];
        let c = merge_candidates(&[], &[a.clone(), b], &clouds);
        assert!(c.iter().all(|x| x.presence != Presence::Both), "two LAN TVs of that model: no guess");
        let c = merge_candidates(&[], &[a], &clouds);
        assert_eq!(c[0].matched_by, Some(MatchKey::ModelOnly));
    }

    #[test]
    fn known_tvs_links_and_overrides() {
        let lans = vec![lan("rest_salon"), lan("rest_chambre")];
        let clouds = cloud_tvs();
        let salon_dev = clouds[0].device.device_id.clone();
        let mut e1 = TvEntry::new("tv1", "Salon", "192.0.2.50"); // old IP
        e1.mac = Some("AA:BB:CC:DD:EE:01".into());
        let mut e2 = TvEntry::new("tv2", "Chambre", "192.0.2.42");
        e2.smartthings_unlinked = vec![clouds[1].device.device_id.clone()];
        let mut app = AppConfig { tvs: vec![e1, e2], active: Some("tv1".into()), ..Default::default() };

        let c = merge_candidates(&app.tvs, &lans, &clouds);
        assert_eq!(c[0].known, Some(("tv1".into(), KnownBy::Mac)));
        assert_eq!(c[1].known, Some(("tv2".into(), KnownBy::Host)));
        assert_eq!(c[1].presence, Presence::LanOnly, "unlinked device is not re-paired");
        let changes = apply_scan(&mut app, &c, 1000);
        assert_eq!(changes, vec![HostChange { id: "tv1".into(), old: "192.0.2.50".into(), new: "192.0.2.169".into() }]);
        let tv1 = app.entry("tv1").unwrap();
        assert_eq!(tv1.host, "192.0.2.169");
        assert_eq!(tv1.smartthings_device_id.as_deref(), Some(salon_dev.as_str()), "auto-linked");
        assert_eq!(tv1.last_seen.lan, Some(1000));
        assert_eq!(tv1.last_seen.cloud, Some(1000));
        assert!(app.entry("tv2").unwrap().smartthings_device_id.is_none());

        // Manual link of tv2 to the third device beats every automatic key.
        app.entry_mut("tv2").unwrap().smartthings_device_id = Some(clouds[2].device.device_id.clone());
        let c = merge_candidates(&app.tvs, &lans, &clouds);
        assert_eq!(c[0].matched_by, Some(MatchKey::Linked));
        assert_eq!(c[1].matched_by, Some(MatchKey::Linked));
        assert_eq!(c[1].device_id(), Some(clouds[2].device.device_id.as_str()));
        assert_eq!(c.len(), 3);
        assert_eq!(c[2].presence, Presence::CloudOnly);

        // A new candidate becomes an entry.
        let e = entry_from_candidate(&c[2], "tv3");
        assert_eq!(e.smartthings_device_id.as_deref(), Some(clouds[1].device.device_id.as_str()));
        assert!(e.host.is_empty());
        assert!(c[2].matches(&format!("st:{}", clouds[1].device.device_id)));
    }

    #[test]
    fn route_planning() {
        use Reach::*;
        use Route::*;
        assert_eq!(plan_routes(Op::SetVolume, Yes, None), vec![Lan]);
        assert_eq!(plan_routes(Op::SetVolume, Yes, Some(Yes)), vec![Lan, Cloud]);
        assert_eq!(plan_routes(Op::SetVolume, Unknown, Some(Unknown)), vec![Lan, Cloud]);
        assert_eq!(plan_routes(Op::SetVolume, No, Some(Yes)), vec![Cloud, Lan]);
        assert_eq!(plan_routes(Op::GetMute, No, Some(Unknown)), vec![Cloud, Lan]);
        assert_eq!(plan_routes(Op::SetMute, No, Some(No)), vec![Lan]);
        assert_eq!(plan_routes(Op::PowerOn, No, Some(Yes)), vec![Cloud, Lan]);
        assert_eq!(plan_routes(Op::PowerOn, Yes, Some(Yes)), vec![Lan, Cloud]);
        assert_eq!(plan_routes(Op::PowerOff, No, Some(Yes)), vec![Cloud]);
        assert_eq!(plan_routes(Op::PowerOff, No, Some(No)), vec![Lan]);
        assert_eq!(plan_routes(Op::PowerOff, Yes, Some(Unknown)), vec![Lan, Cloud]);
    }

    #[test]
    fn normalisation() {
        assert_eq!(normalize_mac("00-00-5E-00-53-01").as_deref(), Some("00:00:5e:00:53:01"));
        assert!(normalize_mac("00:00:00:00:00:00").is_none());
        assert_eq!(normalize_name("[TV] Samsung Q7F 85&quot;"), "samsungq7f85");
        assert!(same_model("QE85Q7FAAUXXC", "qe85q7faauxxc"));
        assert!(same_model("25_PONTUSM_QTV|QE85Q7FAAUXXC", "QE85Q7FAAUXXC"));
        assert!(!same_model("QE85Q7FAAUXXC", "QE65Q7FAAUXXC"));
        assert_eq!(display_tv_name("[TV] Samsung Q7F"), "Samsung Q7F");
    }

    #[test]
    fn fleet_management_offline() {
        let mut e = TvEntry::new("tv1", "Salon", "192.0.2.1");
        e.token = Some("t1".into());
        let fleet = Fleet::new(AppConfig { tvs: vec![e], active: Some("tv1".into()), ..Default::default() });
        let rx = fleet.subscribe();
        let id2 = fleet.add_host("192.0.2.2", Some("Chambre")).unwrap();
        assert_eq!(id2, "tv2");
        assert_eq!(fleet.add_host("192.0.2.2", None).unwrap(), "tv2", "no duplicate");
        assert!(matches!(rx.try_recv().unwrap().event, Event::FleetChanged));
        assert_eq!(fleet.active().id(), "tv1");
        fleet.set_active("tv2").unwrap();
        assert_eq!(fleet.active().id(), "tv2");
        assert_eq!(fleet.resolve("salon").unwrap(), "tv1");
        fleet.rename("tv2", "Bureau").unwrap();
        assert_eq!(fleet.tv("tv2").unwrap().display_name(), "Bureau");
        // Each TV has its own token, and events are tagged.
        let tv2 = fleet.tv("tv2").unwrap();
        assert!(tv2.config().token.is_none());
        assert_eq!(fleet.tv("tv1").unwrap().config().token.as_deref(), Some("t1"));
        tv2.update_config(|c| c.token = Some("t2".into())).unwrap();
        assert_eq!(fleet.config().entry("tv2").unwrap().token.as_deref(), Some("t2"));
        assert_eq!(fleet.config().entry("tv1").unwrap().token.as_deref(), Some("t1"));
        let ev = std::iter::from_fn(|| rx.try_recv().ok()).find(|e| matches!(e.event, Event::ConfigChanged)).unwrap();
        assert_eq!(ev.tv, "tv2");
        // Link / unlink.
        fleet.link_smartthings("tv1", "dev-a").unwrap();
        fleet.link_smartthings("tv2", "dev-a").unwrap();
        assert!(fleet.config().entry("tv1").unwrap().smartthings_device_id.is_none(), "moved");
        fleet.unlink_smartthings("tv2").unwrap();
        assert_eq!(fleet.config().entry("tv2").unwrap().smartthings_unlinked, vec!["dev-a".to_string()]);
        // Remove.
        fleet.remove("tv2").unwrap();
        assert_eq!(fleet.active_id().as_deref(), Some("tv1"));
        assert!(fleet.tv("tv2").is_err());
        assert!(fleet.rename("tv9", "x").is_err());
    }

    #[test]
    fn empty_fleet_first_run() {
        let fleet = Fleet::new(AppConfig::default());
        let tv = fleet.active();
        assert!(tv.host().is_empty());
        assert!(fleet.is_placeholder(&tv));
        tv.update_config(|c| c.host = "192.0.2.9".into()).unwrap();
        assert_eq!(fleet.ids(), vec!["tv1".to_string()]);
        assert_eq!(fleet.active().id(), "tv1");
        assert!(!fleet.is_placeholder(&fleet.active()));
        assert!(fleet.active().ptr_eq(&fleet.tv("tv1").unwrap()));
    }
}
