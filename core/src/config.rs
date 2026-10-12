//! Configuration file `~/.config/samsung-tv/config.json`.
//!
//! # Format (version 2, several TVs)
//!
//! ```json
//! {
//!   "config_version": 2,
//!   "name": "tv-remote",                   // client name shown on every TV (account-level)
//!   "smartthings_token": "…",              // optional SmartThings PAT (account-level, legacy fallback)
//!   "smartthings_oauth": {                 // optional SmartThings OAuth app + tokens (account-level; wins over the PAT)
//!     "client_id": "…", "client_secret": "…", "app_id": "…",
//!     "redirect_uri": "http://localhost:47811/callback",
//!     "access_token": "…", "access_expires_at": 1760086400,
//!     "refresh_token": "…", "refreshed_at": 1760000000,
//!     "scope": "r:devices:* w:devices:* x:devices:* r:locations:*"
//!   },
//!   "active": "tv1",                       // id of the active TV
//!   "tvs": [
//!     { "id": "tv1", "name": "TV du salon", "host": "192.0.2.10", "port": 8002,
//!       "mac": "…", "model": "…", "token": "…", "smartthings_device_id": "…",
//!       "smartthings_unlinked": [], "last_seen": { "lan": 1760000000, "cloud": null, "ip": "…" } }
//!   ],
//!   "host": "192.0.2.10", "port": 8002, "token": "…", "model": "…", "mac": "…"
//! }
//! ```
//!
//! The trailing flat keys are a **legacy mirror** of the active TV, rewritten on every
//! save so that older scripts sharing this file (`tv-cast` reads `host`) keep working.
//! They are ignored on load once `tvs` exists.
//!
//! # Migration from the flat format (version 1)
//!
//! A file without a `tvs` key is the old single-TV format (`host`, `port`, `name`,
//! `token`, `model`, `mac`, `smartthings_token`, `smartthings_device_id`). It is migrated
//! in memory on load ([`AppConfig::from_json`]): `name`, `smartthings_token` and
//! `smartthings_oauth` (if present) become account settings; `host`, `port`, `token`, `model`, `mac` and `smartthings_device_id`
//! become the first TV (id `tv1`), which is active. The pairing token moves unchanged,
//! so the TV does not ask for approval again. Unknown keys are kept. The file is only
//! rewritten in the new format on the next save.
//!
//! The file is written atomically with mode 0600 (it holds tokens). A file that exists
//! but cannot be parsed is never overwritten (loading fails with [`Error::Config`]).
//!
//! # SmartThings OAuth tokens and concurrent processes
//!
//! The OAuth refresh token is single-use, and several processes (the GUI, the CLI, the
//! daily refresh timer) share this file. So the file, not memory, is authoritative:
//! every save through [`SharedConfig`] takes an exclusive `flock` on `config.json.lock`
//! (see [`lock_path`]) and first re-reads the whole file, then applies its change and
//! saves (a read-modify-write transaction). A process therefore never writes back
//! tokens another process already rotated, nor drops a TV another process added. Token
//! refreshes run under the same lock ([`SharedConfig::with_oauth_locked`]). If the file
//! became unparseable after it was loaded, saves fail instead of overwriting it.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{Error, Result};

/// Current config file version (written as `config_version`).
pub const CONFIG_VERSION: u32 = 2;

/// Keys of the flat (version 1) format that the legacy mirror rewrites; they are never
/// kept in [`AppConfig::extra`].
const LEGACY_KEYS: [&str; 6] = ["host", "port", "token", "model", "mac", "smartthings_device_id"];

fn default_port() -> u16 {
    8002
}
fn default_name() -> String {
    "tv-remote".to_string()
}

/// When and where a TV was last seen (Unix seconds).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastSeen {
    /// Last time it answered on the LAN (SSDP or REST).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lan: Option<u64>,
    /// Last time SmartThings reported it online.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloud: Option<u64>,
    /// The IP it had when last seen on the LAN.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ip: Option<String>,
}

/// One known TV in the config file.
#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct TvEntry {
    /// Stable local id (`tv1`, `tv2`, …). Never changes, even on rename.
    pub id: String,
    /// Display name (e.g. "TV du salon").
    #[serde(default)]
    pub name: String,
    /// IP address or host name; empty for a cloud-only TV.
    #[serde(default)]
    pub host: String,
    /// Remote-control WebSocket port (8002 = wss).
    #[serde(default = "default_port")]
    pub port: u16,
    /// MAC address for Wake-on-LAN and for matching (`aa:bb:cc:dd:ee:ff`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mac: Option<String>,
    /// Model code, informational and used for matching.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Pairing token issued by this TV. Never log this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    /// SmartThings device id linked to this TV (manual link or auto-match).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub smartthings_device_id: Option<String>,
    /// SmartThings device ids the user unlinked from this TV: never auto-linked again.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub smartthings_unlinked: Vec<String>,
    /// Last-seen information.
    #[serde(default, skip_serializing_if = "is_default_seen")]
    pub last_seen: LastSeen,
    /// Unknown keys of this entry, preserved verbatim.
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

fn is_default_seen(s: &LastSeen) -> bool {
    *s == LastSeen::default()
}

impl TvEntry {
    /// New entry with defaults.
    pub fn new(id: &str, name: &str, host: &str) -> TvEntry {
        TvEntry {
            id: id.to_string(),
            name: name.to_string(),
            host: host.to_string(),
            port: default_port(),
            mac: None,
            model: None,
            token: None,
            smartthings_device_id: None,
            smartthings_unlinked: Vec::new(),
            last_seen: LastSeen::default(),
            extra: BTreeMap::new(),
        }
    }

    /// Display name, falling back to the model, the host, then the id.
    pub fn display_name(&self) -> String {
        [Some(&self.name), self.model.as_ref(), Some(&self.host)]
            .into_iter()
            .flatten()
            .find(|s| !s.trim().is_empty())
            .cloned()
            .unwrap_or_else(|| self.id.clone())
    }
}

impl std::fmt::Debug for TvEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TvEntry")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("mac", &self.mac)
            .field("model", &self.model)
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .field("smartthings_device_id", &self.smartthings_device_id)
            .field("smartthings_unlinked", &self.smartthings_unlinked)
            .field("last_seen", &self.last_seen)
            .finish()
    }
}

/// SmartThings OAuth app credentials and tokens (account-level `smartthings_oauth`).
///
/// Written by [`crate::smartthings_auth`]: `client_id`, `client_secret` and `app_id`
/// come from the SmartThings CLI output (`apps:create`), the tokens from the login and
/// each refresh. `Debug` redacts the secret and the tokens. Never log them.
#[derive(Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct StOAuth {
    /// OAuth client id of the registered app.
    #[serde(default)]
    pub client_id: String,
    /// OAuth client secret. Never log this.
    #[serde(default)]
    pub client_secret: String,
    /// SmartThings app id (informational).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_id: Option<String>,
    /// Redirect URI used for the last login (one of those registered with the app).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redirect_uri: Option<String>,
    /// Current access token (24 h). Never log this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_token: Option<String>,
    /// When the access token expires (Unix seconds).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_expires_at: Option<u64>,
    /// Current refresh token (single-use, ~30 days). Never log this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// When the current refresh token was issued (login or last refresh, Unix seconds).
    /// Kept after a rejected refresh (tokens cleared) to tell "login expired" from
    /// "never logged in".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refreshed_at: Option<u64>,
    /// Scopes granted by the last token response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// Unknown keys, preserved verbatim.
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl StOAuth {
    /// True if the app credentials are present.
    pub fn has_app(&self) -> bool {
        !self.client_id.trim().is_empty() && !self.client_secret.trim().is_empty()
    }

    /// True if logged in (app credentials and a refresh token).
    pub fn is_connected(&self) -> bool {
        self.has_app() && self.refresh_token.as_deref().is_some_and(|t| !t.trim().is_empty())
    }

    /// Forget the tokens, keep the app credentials. `keep_history` keeps
    /// `refreshed_at` (a rejected refresh: "login expired") instead of clearing it
    /// (logout: "not connected").
    pub fn clear_tokens(&mut self, keep_history: bool) {
        self.access_token = None;
        self.access_expires_at = None;
        self.refresh_token = None;
        self.scope = None;
        if !keep_history {
            self.refreshed_at = None;
        }
    }
}

impl std::fmt::Debug for StOAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let red = |o: Option<&str>| o.filter(|s| !s.is_empty()).map(|_| "<redacted>");
        f.debug_struct("StOAuth")
            .field("client_id", &self.client_id)
            .field("client_secret", &red(Some(&self.client_secret)))
            .field("app_id", &self.app_id)
            .field("redirect_uri", &self.redirect_uri)
            .field("access_token", &red(self.access_token.as_deref()))
            .field("access_expires_at", &self.access_expires_at)
            .field("refresh_token", &red(self.refresh_token.as_deref()))
            .field("refreshed_at", &self.refreshed_at)
            .field("scope", &self.scope)
            .finish()
    }
}

/// The whole config file: account settings plus the list of TVs.
#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct AppConfig {
    /// Format version ([`CONFIG_VERSION`]).
    #[serde(default)]
    pub config_version: u32,
    /// Client name shown on the TVs' device lists (sent base64-encoded).
    #[serde(default = "default_name")]
    pub name: String,
    /// Optional SmartThings personal access token. Never log this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub smartthings_token: Option<String>,
    /// Optional SmartThings OAuth app and tokens; when connected it wins over
    /// `smartthings_token`. Never log the secrets (`Debug` redacts them).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub smartthings_oauth: Option<StOAuth>,
    /// Id of the active TV.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<String>,
    /// Known TVs, in display order.
    #[serde(default)]
    pub tvs: Vec<TvEntry>,
    /// Any other top-level keys, preserved verbatim.
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl Default for AppConfig {
    fn default() -> Self {
        AppConfig {
            config_version: CONFIG_VERSION,
            name: default_name(),
            smartthings_token: None,
            smartthings_oauth: None,
            active: None,
            tvs: Vec::new(),
            extra: BTreeMap::new(),
        }
    }
}

impl std::fmt::Debug for AppConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppConfig")
            .field("name", &self.name)
            .field("smartthings_token", &self.smartthings_token.as_ref().map(|_| "<redacted>"))
            .field("smartthings_oauth", &self.smartthings_oauth)
            .field("active", &self.active)
            .field("tvs", &self.tvs)
            .field("extra_keys", &self.extra.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl AppConfig {
    /// Parse the file contents, migrating the flat single-TV format (see the module
    /// docs). Pure.
    pub fn from_json(v: Value) -> Result<AppConfig> {
        let Value::Object(mut obj) = v else {
            return Err(Error::Config("the config is not a JSON object".into()));
        };
        if obj.contains_key("tvs") {
            // New format; drop the legacy mirror (rewritten on save).
            for k in LEGACY_KEYS {
                obj.remove(k);
            }
            let mut cfg: AppConfig = serde_json::from_value(Value::Object(obj))?;
            cfg.config_version = CONFIG_VERSION;
            cfg.fix_active();
            return Ok(cfg);
        }
        // Flat (version 1) format.
        let take_str = |obj: &mut serde_json::Map<String, Value>, k: &str| -> Option<String> {
            match obj.remove(k) {
                Some(Value::String(s)) => Some(s),
                Some(Value::Number(n)) => Some(n.to_string()),
                _ => None,
            }
        };
        let host = take_str(&mut obj, "host");
        let port = obj.remove("port").and_then(|p| p.as_u64()).and_then(|p| u16::try_from(p).ok());
        let token = take_str(&mut obj, "token");
        let model = take_str(&mut obj, "model");
        let mac = take_str(&mut obj, "mac");
        let st_dev = take_str(&mut obj, "smartthings_device_id");
        let name = take_str(&mut obj, "name").unwrap_or_else(default_name);
        let st_tok = take_str(&mut obj, "smartthings_token");
        let st_oauth = match obj.remove("smartthings_oauth") {
            Some(Value::Null) | None => None,
            Some(v) => Some(serde_json::from_value::<StOAuth>(v)?),
        };
        obj.remove("config_version");
        let mut cfg = AppConfig {
            config_version: CONFIG_VERSION,
            name,
            smartthings_token: st_tok,
            smartthings_oauth: st_oauth,
            active: None,
            tvs: Vec::new(),
            extra: obj.into_iter().collect(),
        };
        let has_tv = host.as_deref().is_some_and(|h| !h.trim().is_empty())
            || token.is_some()
            || mac.is_some()
            || st_dev.is_some();
        if has_tv {
            let mut e = TvEntry::new("tv1", "", host.as_deref().unwrap_or(""));
            e.name = match &model {
                Some(m) if !m.trim().is_empty() => format!("Samsung {m}"),
                _ => "Samsung TV".into(),
            };
            e.port = port.unwrap_or_else(default_port);
            e.token = token;
            e.model = model;
            e.mac = mac;
            e.smartthings_device_id = st_dev;
            cfg.tvs.push(e);
            cfg.active = Some("tv1".into());
        }
        Ok(cfg)
    }

    /// Serialise for the file: the new format plus the legacy mirror of the active TV.
    pub fn to_json(&self) -> Result<Value> {
        let mut v = serde_json::to_value(self)?;
        if let (Value::Object(obj), Some(tv)) = (&mut v, self.active_entry()) {
            obj.insert("host".into(), Value::String(tv.host.clone()));
            obj.insert("port".into(), Value::from(tv.port));
            for (k, val) in [("token", &tv.token), ("model", &tv.model), ("mac", &tv.mac)] {
                if let Some(s) = val {
                    obj.insert(k.into(), Value::String(s.clone()));
                }
            }
        }
        Ok(v)
    }

    /// Make sure `active` names an existing TV (first one otherwise, `None` if empty).
    pub fn fix_active(&mut self) {
        let ok = self.active.as_ref().is_some_and(|a| self.tvs.iter().any(|t| &t.id == a));
        if !ok {
            self.active = self.tvs.first().map(|t| t.id.clone());
        }
    }

    /// `~/.config/samsung-tv/config.json` (honours `$XDG_CONFIG_HOME`).
    pub fn default_path() -> PathBuf {
        Config::default_path()
    }

    /// Load and migrate a config file.
    pub fn load_from(path: &Path) -> Result<AppConfig> {
        let text = fs::read_to_string(path)
            .map_err(|e| Error::Config(format!("cannot read {}: {e}", path.display())))?;
        let v: Value = serde_json::from_str(&text)
            .map_err(|e| Error::Config(format!("invalid {}: {e}", path.display())))?;
        Self::from_json(v).map_err(|e| Error::Config(format!("invalid {}: {e}", path.display())))
    }

    /// Like [`AppConfig::load_from`], but a missing file gives the defaults.
    pub fn load_or_default(path: &Path) -> Result<AppConfig> {
        match fs::metadata(path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(AppConfig::default()),
            _ => Self::load_from(path),
        }
    }

    /// Save atomically (temp file + rename) with permissions 0600. A symlinked config
    /// (e.g. kept in a dotfiles repository) is written through: its target is replaced,
    /// not the link. A missing directory is created with mode 0700.
    pub fn save_to(&self, path: &Path) -> Result<()> {
        use std::os::unix::fs::OpenOptionsExt;
        let target = resolve_symlinks(path);
        let ctx = |e: std::io::Error| Error::Config(format!("cannot save {}: {e}", target.display()));
        if let Some(dir) = target.parent().filter(|d| !d.as_os_str().is_empty()) {
            create_private_dir(dir).map_err(ctx)?;
        }
        let tmp = target.with_extension("json.tmp");
        let text = serde_json::to_string_pretty(&self.to_json()?)?;
        let write = || -> std::io::Result<()> {
            {
                let mut f = fs::OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .mode(0o600)
                    .open(&tmp)?;
                f.write_all(text.as_bytes())?;
                f.write_all(b"\n")?;
                f.sync_all()?;
            }
            // mode() only applies on creation; enforce it if the temp file pre-existed.
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
            fs::rename(&tmp, &target)
        };
        write().map_err(ctx)
    }

    /// True if SmartThings is usable: OAuth connected, or a personal access token.
    pub fn has_smartthings(&self) -> bool {
        self.oauth_connected() || self.smartthings_token.as_deref().is_some_and(|t| !t.trim().is_empty())
    }

    /// True if the OAuth login is connected (it then wins over the PAT).
    pub fn oauth_connected(&self) -> bool {
        self.smartthings_oauth.as_ref().is_some_and(StOAuth::is_connected)
    }

    /// The active TV entry, if any.
    pub fn active_entry(&self) -> Option<&TvEntry> {
        let id = self.active.as_ref()?;
        self.tvs.iter().find(|t| &t.id == id)
    }

    /// Entry by id.
    pub fn entry(&self, id: &str) -> Option<&TvEntry> {
        self.tvs.iter().find(|t| t.id == id)
    }

    /// Mutable entry by id.
    pub fn entry_mut(&mut self, id: &str) -> Option<&mut TvEntry> {
        self.tvs.iter_mut().find(|t| t.id == id)
    }

    /// The next free id (`tv1`, `tv2`, …).
    pub fn next_id(&self) -> String {
        (1..)
            .map(|n| format!("tv{n}"))
            .find(|id| self.entry(id).is_none())
            .unwrap_or_else(|| "tv".into())
    }

    /// Find a TV by selector: exact id, then display name (any case), then host, then a
    /// unique case-insensitive prefix of the id or name. Pure.
    pub fn resolve(&self, sel: &str) -> Result<&TvEntry> {
        let s = sel.trim();
        if s.is_empty() {
            return Err(Error::Config("empty TV name".into()));
        }
        let lower = s.to_lowercase();
        if let Some(e) = self.tvs.iter().find(|t| t.id == s) {
            return Ok(e);
        }
        let by_name: Vec<_> = self.tvs.iter().filter(|t| t.display_name().to_lowercase() == lower).collect();
        if by_name.len() == 1 {
            return Ok(by_name[0]);
        }
        if let Some(e) = self.tvs.iter().find(|t| !t.host.is_empty() && t.host == s) {
            return Ok(e);
        }
        let pre: Vec<_> = self
            .tvs
            .iter()
            .filter(|t| t.id.to_lowercase().starts_with(&lower) || t.display_name().to_lowercase().starts_with(&lower))
            .collect();
        match pre.len() {
            1 => Ok(pre[0]),
            0 => Err(Error::Config(format!("no TV named '{s}' (see `tvs list`)"))),
            _ => Err(Error::Config(format!(
                "'{s}' matches several TVs: {}",
                pre.iter().map(|t| format!("{} ({})", t.display_name(), t.id)).collect::<Vec<_>>().join(", ")
            ))),
        }
    }

    /// The per-TV [`Config`] view of TV `id` (or of the active TV if `id` is `None`);
    /// a default view carrying the account settings if there is no such TV.
    pub fn view(&self, id: Option<&str>) -> Config {
        let entry = match id {
            Some(id) => self.entry(id),
            None => self.active_entry(),
        };
        let mut c = Config {
            name: self.name.clone(),
            smartthings_token: self.smartthings_token.clone(),
            ..Config::default()
        };
        match entry {
            Some(e) => {
                c.id = e.id.clone();
                c.display_name = e.display_name();
                c.host = e.host.clone();
                c.port = e.port;
                c.token = e.token.clone();
                c.model = e.model.clone();
                c.mac = e.mac.clone();
                c.smartthings_device_id = e.smartthings_device_id.clone();
                c.extra = e.extra.clone();
            }
            None => c.id = id.unwrap_or_default().to_string(),
        }
        c
    }

    /// Write a modified [`Config`] view back: account fields to the account, the rest to
    /// TV `id` (or the view's id, or the active TV). A view with no TV behind it creates
    /// one if it now has a host, token, MAC or SmartThings device (the first TV becomes
    /// active). Returns the id written to, if any.
    pub fn apply_view(&mut self, id: Option<&str>, before: &Config, after: &Config) -> Option<String> {
        self.name = after.name.clone();
        self.smartthings_token = after.smartthings_token.clone();
        let target = id
            .map(str::to_string)
            .or_else(|| (!after.id.is_empty()).then(|| after.id.clone()))
            .or_else(|| self.active.clone());
        let exists = target.as_deref().is_some_and(|t| self.entry(t).is_some());
        if !exists {
            let wants = !after.host.trim().is_empty()
                || after.token.is_some()
                || after.mac.is_some()
                || after.smartthings_device_id.is_some();
            if !wants {
                return None;
            }
            let new_id = target.filter(|t| !t.is_empty()).unwrap_or_else(|| self.next_id());
            let mut e = TvEntry::new(&new_id, "", "");
            if after.display_name != before.display_name {
                e.name = after.display_name.clone();
            }
            self.tvs.push(e);
            if self.active.is_none() {
                self.active = Some(new_id.clone());
            }
            return self.write_entry(&new_id, before, after);
        }
        let t = target?;
        self.write_entry(&t, before, after)
    }

    fn write_entry(&mut self, id: &str, before: &Config, after: &Config) -> Option<String> {
        let e = self.entry_mut(id)?;
        e.host = after.host.clone();
        e.port = after.port;
        e.token = after.token.clone();
        e.model = after.model.clone();
        e.mac = after.mac.clone();
        e.smartthings_device_id = after.smartthings_device_id.clone();
        e.extra = after.extra.clone();
        if after.display_name != before.display_name {
            e.name = after.display_name.clone();
        }
        Some(id.to_string())
    }
}

/// Settings of **one** TV as the single-TV API sees them: that TV's entry plus the
/// account settings (`name`, `smartthings_token`). Build it with [`AppConfig::view`];
/// [`crate::Tv::update_config`] writes changes back to the right place.
#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct Config {
    /// Local id of the TV (empty if the view has no TV behind it yet).
    #[serde(default)]
    pub id: String,
    /// Display name of the TV.
    #[serde(default)]
    pub display_name: String,
    /// TV IP address or host name.
    #[serde(default)]
    pub host: String,
    /// Remote-control WebSocket port (8002 = wss).
    #[serde(default = "default_port")]
    pub port: u16,
    /// Client name shown on the TV's device list (account-level).
    #[serde(default = "default_name")]
    pub name: String,
    /// Pairing token issued by this TV. Never log this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    /// TV model code, informational.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// TV MAC address for Wake-on-LAN (`aa:bb:cc:dd:ee:ff`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mac: Option<String>,
    /// Optional SmartThings personal access token (account-level). Never log this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub smartthings_token: Option<String>,
    /// SmartThings device id linked to this TV.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub smartthings_device_id: Option<String>,
    /// Unknown keys of the TV's entry, preserved verbatim.
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            id: String::new(),
            display_name: String::new(),
            host: String::new(),
            port: default_port(),
            name: default_name(),
            token: None,
            model: None,
            mac: None,
            smartthings_token: None,
            smartthings_device_id: None,
            extra: BTreeMap::new(),
        }
    }
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let red = |o: &Option<String>| o.as_ref().map(|_| "<redacted>");
        f.debug_struct("Config")
            .field("id", &self.id)
            .field("display_name", &self.display_name)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("name", &self.name)
            .field("token", &red(&self.token))
            .field("model", &self.model)
            .field("mac", &self.mac)
            .field("smartthings_token", &red(&self.smartthings_token))
            .field("smartthings_device_id", &self.smartthings_device_id)
            .field("extra_keys", &self.extra.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl Config {
    /// `~/.config/samsung-tv/config.json` (honours `$XDG_CONFIG_HOME`).
    pub fn default_path() -> PathBuf {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home_dir().join(".config"));
        base.join("samsung-tv").join("config.json")
    }

    /// The active TV's view of [`Config::default_path`].
    pub fn load() -> Result<Config> {
        Self::load_from(&Self::default_path())
    }

    /// The active TV's view of a config file (either format).
    pub fn load_from(path: &Path) -> Result<Config> {
        Ok(AppConfig::load_from(path)?.view(None))
    }
}

/// `$HOME` (falls back to `/`).
pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// Set by [`set_cache_dir`].
static CACHE_DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Use `dir` instead of `~/.cache/tv-remote/` for this process, for platforms without a
/// usable `$HOME` (the Android app passes its `cacheDir`). The first call wins; returns
/// `false` if a directory was already set.
pub fn set_cache_dir(dir: PathBuf) -> bool {
    CACHE_DIR.set(dir).is_ok()
}

/// `~/.cache/tv-remote/` (honours `$XDG_CACHE_HOME`), or the [`set_cache_dir`] override;
/// created on demand.
pub fn cache_dir() -> PathBuf {
    if let Some(dir) = CACHE_DIR.get() {
        let _ = fs::create_dir_all(dir);
        return dir.clone();
    }
    let base = std::env::var_os("XDG_CACHE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".cache"));
    let dir = base.join("tv-remote");
    let _ = fs::create_dir_all(&dir);
    dir
}

/// An [`AppConfig`] shared between threads, with an optional backing file, seen
/// through a TV scope.
///
/// Background threads (e.g. a remote channel persisting a new token) and the UI read
/// and update the same instance. Clones made with [`SharedConfig::scoped`] share the
/// data but [`SharedConfig::get`] / [`SharedConfig::update`] act on one TV's
/// [`Config`] view; an unscoped handle follows the active TV.
#[derive(Clone, Debug)]
pub struct SharedConfig {
    inner: Arc<RwLock<AppConfig>>,
    path: Option<PathBuf>,
    scope: Option<String>,
}

impl SharedConfig {
    /// Wrap a single-TV view (`path` = where updates are persisted, `None` = memory
    /// only). The view becomes the only TV; the handle is scoped to it.
    pub fn new(cfg: Config, path: Option<PathBuf>) -> Self {
        let mut app = AppConfig::default();
        let before = Config::default();
        let id = app.apply_view(None, &before, &cfg);
        let mut s = Self::from_app(app, path);
        s.scope = id;
        s
    }

    /// Wrap a whole config (unscoped: follows the active TV).
    pub fn from_app(cfg: AppConfig, path: Option<PathBuf>) -> Self {
        SharedConfig {
            inner: Arc::new(RwLock::new(cfg)),
            path,
            scope: None,
        }
    }

    /// A handle on the same data, scoped to TV `id`.
    pub fn scoped(&self, id: &str) -> SharedConfig {
        SharedConfig {
            inner: self.inner.clone(),
            path: self.path.clone(),
            scope: Some(id.to_string()),
        }
    }

    /// A handle on the same data that follows the active TV.
    pub fn unscoped(&self) -> SharedConfig {
        SharedConfig {
            inner: self.inner.clone(),
            path: self.path.clone(),
            scope: None,
        }
    }

    /// The TV this handle is scoped to (`None` = follows the active TV).
    pub fn scope(&self) -> Option<&str> {
        self.scope.as_deref()
    }

    /// The TV id this handle currently resolves to (its scope, else the active TV).
    pub fn tv_id(&self) -> Option<String> {
        self.scope.clone().or_else(|| self.app().active)
    }

    /// A snapshot of this handle's TV view.
    pub fn get(&self) -> Config {
        self.inner
            .read()
            .map(|c| c.view(self.scope.as_deref()))
            .unwrap_or_default()
    }

    /// A snapshot of the whole config.
    pub fn app(&self) -> AppConfig {
        self.inner.read().map(|c| c.clone()).unwrap_or_default()
    }

    /// Backing file, if any.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Mutate this handle's TV view and persist (see [`AppConfig::apply_view`]).
    pub fn update(&self, f: impl FnOnce(&mut Config)) -> Result<()> {
        let scope = self.scope.clone();
        self.update_app(move |app| {
            let before = app.view(scope.as_deref());
            let mut after = before.clone();
            f(&mut after);
            if after != before {
                app.apply_view(scope.as_deref(), &before, &after);
            }
        })
    }

    /// Mutate the whole config and persist it to the backing file (if any).
    ///
    /// With a backing file this is a read-modify-write transaction: it holds the file's
    /// `flock` (see [`lock_path`]), first adopts the file as it is now (another process,
    /// e.g. the CLI next to a running GUI, may have changed TVs or rotated the
    /// SmartThings tokens), then applies `f` and saves. A file that exists but cannot
    /// be parsed any more is never overwritten: the update fails instead.
    /// Never call it from inside [`SharedConfig::with_oauth_locked`].
    pub fn update_app<R>(&self, f: impl FnOnce(&mut AppConfig) -> R) -> Result<R> {
        let _lock = match &self.path {
            Some(p) => Some(FileLock::acquire(p)?),
            None => None,
        };
        self.adopt_disk_locked()?;
        let (snapshot, r) = {
            let mut guard = self
                .inner
                .write()
                .map_err(|_| Error::Config("config lock poisoned".into()))?;
            let r = f(&mut guard);
            guard.fix_active();
            (guard.clone(), r)
        };
        if let Some(p) = &self.path {
            snapshot.save_to(p)?;
        }
        Ok(r)
    }

    /// Replace memory with the backing file's current contents. Call it with the file
    /// lock held. No file (or none yet) keeps memory; a file that cannot be read or
    /// parsed is an error, so that it is never overwritten.
    fn adopt_disk_locked(&self) -> Result<()> {
        let Some(p) = &self.path else { return Ok(()) };
        let text = match fs::read_to_string(p) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(Error::Config(format!("cannot read {}: {e}; not saving", p.display()))),
        };
        let disk = serde_json::from_str::<Value>(&text)
            .map_err(|e| e.to_string())
            .and_then(|v| AppConfig::from_json(v).map_err(|e| e.to_string()))
            .map_err(|e| {
                Error::Config(format!(
                    "{} is not a valid config any more ({e}); it is not overwritten: fix or move it",
                    p.display()
                ))
            })?;
        let mut g = self.inner.write().map_err(|_| Error::Config("config lock poisoned".into()))?;
        *g = disk;
        Ok(())
    }

    /// The `smartthings_oauth` section as the backing file has it now (adopted into
    /// memory), or the in-memory one without a file. Takes no lock: the file is
    /// replaced atomically, so the read is consistent.
    pub fn oauth_from_disk(&self) -> Option<StOAuth> {
        if let Some(d) = self.path.as_deref().and_then(disk_oauth) {
            if let Ok(mut g) = self.inner.write() {
                if g.smartthings_oauth != d {
                    g.smartthings_oauth = d;
                }
            }
        }
        self.app().smartthings_oauth
    }

    /// Run `f` on the `smartthings_oauth` section while holding the process-wide
    /// SmartThings mutex and the config file's `flock`, so that exactly one thread of
    /// one process at a time can spend the single-use refresh token. The whole file is
    /// re-read first (another process may have rotated the tokens or changed TVs); if
    /// `f` changes the section, the change is saved **immediately** (before `f`'s result
    /// is used). If the file cannot be parsed any more, `f` is not run at all, so a
    /// refresh token is never spent without a place to save its successor.
    /// `f` may do network I/O; it must not call [`SharedConfig::update_app`].
    pub fn with_oauth_locked<R>(&self, f: impl FnOnce(&mut Option<StOAuth>) -> Result<R>) -> Result<R> {
        static PROCESS_LOCK: Mutex<()> = Mutex::new(());
        let _process = PROCESS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _file = match &self.path {
            Some(p) => Some(FileLock::acquire(p)?),
            None => None,
        };
        self.adopt_disk_locked()?;
        let mut cur = self.app().smartthings_oauth;
        let before = cur.clone();
        let res = f(&mut cur);
        if cur != before {
            let snapshot = {
                let mut g = self
                    .inner
                    .write()
                    .map_err(|_| Error::Config("config lock poisoned".into()))?;
                g.smartthings_oauth = cur;
                g.clone()
            };
            if let Some(p) = &self.path {
                snapshot.save_to(p)?;
            }
        }
        res
    }
}

/// `path` with its symlinks followed (up to 16 levels; relative targets resolve against
/// the link's directory). Dangling links resolve to their target, so the first save
/// creates it. A path that is not a symlink is returned unchanged.
fn resolve_symlinks(path: &Path) -> PathBuf {
    let mut p = path.to_path_buf();
    for _ in 0..16 {
        match fs::read_link(&p) {
            Ok(t) if t.is_absolute() => p = t,
            Ok(t) => p = p.parent().map(|d| d.join(&t)).unwrap_or(t),
            Err(_) => break,
        }
    }
    p
}

/// `create_dir_all`, with mode 0700 for the directories it creates (they hold tokens).
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)
}

/// The lock file guarding the config file `path`: `config.json.lock` next to it.
pub fn lock_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().map(|n| n.to_os_string()).unwrap_or_else(|| "config.json".into());
    name.push(".lock");
    path.with_file_name(name)
}

/// The `smartthings_oauth` section of the file at `path`; `None` if the file is missing
/// or unreadable (memory is then kept).
fn disk_oauth(path: &Path) -> Option<Option<StOAuth>> {
    let text = fs::read_to_string(path).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    match v.get("smartthings_oauth") {
        None | Some(Value::Null) => Some(None),
        Some(o) => serde_json::from_value::<StOAuth>(o.clone()).ok().map(Some),
    }
}

/// An exclusive `flock` on [`lock_path`], released on drop.
struct FileLock {
    file: fs::File,
}

impl FileLock {
    /// Wait up to 60 s for the lock (a holder only keeps it for one token request).
    fn acquire(config_path: &Path) -> Result<FileLock> {
        use std::os::unix::fs::OpenOptionsExt;
        let lp = lock_path(config_path);
        let ctx = |e: std::io::Error| Error::Config(format!("cannot save {}: {e}", lp.display()));
        if let Some(dir) = lp.parent().filter(|d| !d.as_os_str().is_empty()) {
            create_private_dir(dir).map_err(ctx)?;
        }
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&lp)
            .map_err(ctx)?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(FileLock { file }),
                Err(fs::TryLockError::WouldBlock) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                Err(fs::TryLockError::WouldBlock) => {
                    return Err(Error::Config(format!("{} is locked by another process", lp.display())))
                }
                Err(fs::TryLockError::Error(e)) => return Err(Error::Io(e)),
            }
        }
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tvremote-cfg-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The exact flat format written by the single-TV versions (fake values).
    const FLAT_V1: &str = r#"{
  "host": "192.0.2.169",
  "port": 8002,
  "name": "Ryoku",
  "token": "11111111",
  "model": "QE85Q7FAAUXXC",
  "mac": "AA:BB:CC:DD:EE:01",
  "smartthings_token": "fake-pat",
  "smartthings_device_id": "00000000-aaaa-bbbb-cccc-000000000001",
  "future": {"x": 1}
}"#;

    #[test]
    fn migrates_the_flat_format() {
        let dir = temp("migrate");
        let path = dir.join("config.json");
        fs::write(&path, FLAT_V1).unwrap();
        let app = AppConfig::load_from(&path).unwrap();
        assert_eq!(app.name, "Ryoku");
        assert_eq!(app.smartthings_token.as_deref(), Some("fake-pat"));
        assert_eq!(app.active.as_deref(), Some("tv1"));
        assert_eq!(app.tvs.len(), 1);
        let tv = &app.tvs[0];
        assert_eq!(tv.host, "192.0.2.169");
        assert_eq!(tv.port, 8002);
        assert_eq!(tv.token.as_deref(), Some("11111111"), "the pairing token moves unchanged");
        assert_eq!(tv.mac.as_deref(), Some("AA:BB:CC:DD:EE:01"));
        assert_eq!(tv.model.as_deref(), Some("QE85Q7FAAUXXC"));
        assert_eq!(tv.smartthings_device_id.as_deref(), Some("00000000-aaaa-bbbb-cccc-000000000001"));
        assert_eq!(app.extra.get("future"), Some(&serde_json::json!({"x": 1})));
        assert!(!app.extra.contains_key("host"));
        // The view the single-TV API sees is the same as before.
        let v = app.view(None);
        assert_eq!((v.host.as_str(), v.token.as_deref(), v.name.as_str()), ("192.0.2.169", Some("11111111"), "Ryoku"));
        assert!(!format!("{app:?}").contains("11111111"));
        assert!(!format!("{v:?}").contains("fake-pat"));
        // Loading does not touch the file.
        assert_eq!(fs::read_to_string(&path).unwrap(), FLAT_V1);

        // Save → new format + legacy mirror, 0600, round-trips.
        app.save_to(&path).unwrap();
        let raw: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(raw["config_version"], 2);
        assert_eq!(raw["tvs"][0]["token"], "11111111");
        assert_eq!(raw["host"], "192.0.2.169", "legacy mirror for tv-cast");
        assert_eq!(raw["future"]["x"], 1);
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        let back = AppConfig::load_from(&path).unwrap();
        assert_eq!(back, app);
        let _ = fs::remove_dir_all(&dir);
    }

    const FAKE_OAUTH: &str = r#"{"client_id": "fake-client", "client_secret": "fake-secret",
        "app_id": "fake-app", "access_token": "fake-access", "access_expires_at": 2000,
        "refresh_token": "fake-refresh", "refreshed_at": 1000, "scope": "r:devices:*", "later": 1}"#;

    #[test]
    fn oauth_section_round_trips_migrates_and_is_redacted() {
        let oauth: Value = serde_json::from_str(FAKE_OAUTH).unwrap();
        // New format.
        let app = AppConfig::from_json(serde_json::json!({"tvs": [], "smartthings_oauth": oauth.clone()})).unwrap();
        let o = app.smartthings_oauth.clone().unwrap();
        assert!(o.is_connected() && app.oauth_connected() && app.has_smartthings());
        assert_eq!(o.extra.get("later"), Some(&Value::from(1)), "unknown keys kept");
        let dbg = format!("{app:?}");
        for secret in ["fake-secret", "fake-access", "fake-refresh"] {
            assert!(!dbg.contains(secret), "{secret} leaked in Debug");
        }
        let back = AppConfig::from_json(app.to_json().unwrap()).unwrap();
        assert_eq!(back, app);
        // Flat (v1) format: becomes an account setting next to the PAT.
        let flat = AppConfig::from_json(serde_json::json!({"host": "h", "smartthings_token": "pat", "smartthings_oauth": oauth})).unwrap();
        assert_eq!(flat.smartthings_oauth.as_ref().unwrap().client_id, "fake-client");
        assert!(!flat.extra.contains_key("smartthings_oauth"));
        assert_eq!(flat.smartthings_token.as_deref(), Some("pat"));
        // App only / PAT only / nothing.
        let mut only_app = o.clone();
        only_app.clear_tokens(true);
        assert!(only_app.has_app() && !only_app.is_connected() && only_app.refreshed_at.is_some());
        only_app.clear_tokens(false);
        assert!(only_app.refreshed_at.is_none());
        let pat = AppConfig { smartthings_token: Some("p".into()), ..Default::default() };
        assert!(pat.has_smartthings() && !pat.oauth_connected());
        assert!(!AppConfig::default().has_smartthings());
    }

    #[test]
    fn saves_adopt_oauth_tokens_rotated_by_another_process() {
        let dir = temp("rotate");
        let path = dir.join("config.json");
        let app = AppConfig { smartthings_oauth: Some(serde_json::from_str(FAKE_OAUTH).unwrap()), ..Default::default() };
        app.save_to(&path).unwrap();
        let shared = SharedConfig::from_app(AppConfig::load_from(&path).unwrap(), Some(path.clone()));
        // Another process rotates the pair on disk.
        let mut other = AppConfig::load_from(&path).unwrap();
        other.smartthings_oauth.as_mut().unwrap().refresh_token = Some("rotated-refresh".into());
        other.save_to(&path).unwrap();
        // An unrelated save in this process must not write the stale pair back.
        shared.update_app(|a| a.name = "Renamed".into()).unwrap();
        let disk = AppConfig::load_from(&path).unwrap();
        assert_eq!(disk.name, "Renamed");
        assert_eq!(disk.smartthings_oauth.unwrap().refresh_token.as_deref(), Some("rotated-refresh"));
        assert!(lock_path(&path).ends_with("config.json.lock"));
        // with_oauth_locked saves at once and memory follows.
        shared
            .with_oauth_locked(|o| {
                o.as_mut().unwrap().access_token = Some("new-access".into());
                Ok(())
            })
            .unwrap();
        assert_eq!(AppConfig::load_from(&path).unwrap().smartthings_oauth.unwrap().access_token.as_deref(), Some("new-access"));
        assert_eq!(shared.app().smartthings_oauth.unwrap().access_token.as_deref(), Some("new-access"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn concurrent_processes_do_not_lose_each_others_changes() {
        let dir = temp("lost-update");
        let path = dir.join("config.json");
        let mut app = AppConfig::default();
        app.tvs.push(TvEntry::new("tv1", "Salon", "192.0.2.1"));
        app.save_to(&path).unwrap();
        // Two processes load the same file (e.g. the GUI, then the CLI).
        let gui = SharedConfig::from_app(AppConfig::load_from(&path).unwrap(), Some(path.clone()));
        let cli = SharedConfig::from_app(AppConfig::load_from(&path).unwrap(), Some(path.clone()));
        cli.update_app(|a| {
            let id = a.next_id();
            a.tvs.push(TvEntry::new(&id, "Chambre", "192.0.2.2"));
        })
        .unwrap();
        // The GUI's next save (stale memory) must keep the TV the CLI added.
        gui.update_app(|a| a.entry_mut("tv1").unwrap().name = "Living room".into()).unwrap();
        let disk = AppConfig::load_from(&path).unwrap();
        assert_eq!(disk.tvs.len(), 2, "the CLI's TV survived the GUI's save");
        assert_eq!(disk.entry("tv1").unwrap().name, "Living room");
        assert_eq!(gui.app().tvs.len(), 2, "memory follows the file");

        // Many writers at once, each with its own stale copy: no add is lost, ids are unique.
        let handles: Vec<_> = (0..12)
            .map(|i| {
                let p = path.clone();
                let shared = SharedConfig::from_app(AppConfig::load_from(&p).unwrap(), Some(p));
                std::thread::spawn(move || {
                    shared
                        .update_app(|a| {
                            let id = a.next_id();
                            a.tvs.push(TvEntry::new(&id, &format!("TV {i}"), &format!("192.0.2.{}", 10 + i)));
                        })
                        .unwrap()
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let disk = AppConfig::load_from(&path).unwrap();
        assert_eq!(disk.tvs.len(), 14);
        let mut ids: Vec<_> = disk.tvs.iter().map(|t| t.id.clone()).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 14, "unique ids");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_broken_after_loading_is_never_overwritten() {
        let dir = temp("broken-later");
        let path = dir.join("config.json");
        let app = AppConfig { smartthings_oauth: Some(serde_json::from_str(FAKE_OAUTH).unwrap()), ..Default::default() };
        app.save_to(&path).unwrap();
        let shared = SharedConfig::from_app(AppConfig::load_from(&path).unwrap(), Some(path.clone()));
        // The user edits the file by hand and leaves a typo.
        let broken = "{\"tvs\": [ oops";
        fs::write(&path, broken).unwrap();
        let err = shared.update_app(|a| a.name = "x".into()).unwrap_err();
        assert!(err.to_string().contains("not overwritten"), "{err}");
        assert_eq!(fs::read_to_string(&path).unwrap(), broken);
        // A token refresh is not even attempted (its successor could not be saved).
        let mut ran = false;
        assert!(shared
            .with_oauth_locked(|_| {
                ran = true;
                Ok(())
            })
            .is_err());
        assert!(!ran);
        assert_eq!(fs::read_to_string(&path).unwrap(), broken);
        // Once the file is gone, saving recreates it.
        fs::remove_file(&path).unwrap();
        shared.update_app(|a| a.name = "y".into()).unwrap();
        assert_eq!(AppConfig::load_from(&path).unwrap().name, "y");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn saves_write_through_symlinks_and_create_private_dirs() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp("symlink");
        let real = dir.join("dotfiles").join("config.json");
        let link_dir = dir.join("xdg").join("samsung-tv");
        fs::create_dir_all(&link_dir).unwrap();
        let link = link_dir.join("config.json");
        std::os::unix::fs::symlink("../../dotfiles/config.json", &link).unwrap();
        let app = AppConfig { name: "linked".into(), ..Default::default() };
        app.save_to(&link).unwrap();
        assert!(fs::symlink_metadata(&link).unwrap().file_type().is_symlink(), "the link is kept");
        assert_eq!(AppConfig::load_from(&real).unwrap().name, "linked");
        assert_eq!(fs::metadata(dir.join("dotfiles")).unwrap().permissions().mode() & 0o777, 0o700);
        // An unwritable directory gives an error that names the file.
        let ro = dir.join("ro");
        fs::create_dir_all(&ro).unwrap();
        fs::set_permissions(&ro, fs::Permissions::from_mode(0o500)).unwrap();
        let res = app.save_to(&ro.join("config.json"));
        fs::set_permissions(&ro, fs::Permissions::from_mode(0o700)).unwrap();
        // (root ignores directory permissions, so the save may succeed there)
        if let Err(e) = res {
            assert!(e.to_string().contains("cannot save") && e.to_string().contains("ro/config.json"), "{e}");
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn minimal_and_empty_flat_files() {
        let app = AppConfig::from_json(serde_json::json!({"host": "h"})).unwrap();
        assert_eq!(app.tvs[0].port, 8002);
        assert_eq!(app.name, "tv-remote");
        assert!(app.tvs[0].token.is_none());
        let empty = AppConfig::from_json(serde_json::json!({})).unwrap();
        assert!(empty.tvs.is_empty() && empty.active.is_none());
        assert!(AppConfig::from_json(serde_json::json!([1])).is_err());
        let json = serde_json::to_string(&empty.to_json().unwrap()).unwrap();
        assert!(!json.contains("smartthings"));
    }

    #[test]
    fn new_format_ignores_stale_mirror_and_fixes_active() {
        let app = AppConfig::from_json(serde_json::json!({
            "tvs": [{"id": "tv1", "host": "10.0.0.1"}, {"id": "tv2", "name": "Chambre", "host": "10.0.0.2", "zz": true}],
            "active": "gone", "host": "1.2.3.4"
        }))
        .unwrap();
        assert_eq!(app.active.as_deref(), Some("tv1"));
        assert!(!app.extra.contains_key("host"));
        assert_eq!(app.tvs[1].extra.get("zz"), Some(&Value::Bool(true)));
        assert_eq!(app.resolve("chambre").unwrap().id, "tv2");
        assert_eq!(app.resolve("tv2").unwrap().id, "tv2");
        assert_eq!(app.resolve("10.0.0.1").unwrap().id, "tv1");
        assert!(app.resolve("tv").is_err(), "ambiguous prefix");
        assert!(app.resolve("nope").is_err());
        assert_eq!(app.next_id(), "tv3");
    }

    #[test]
    fn scoped_updates_write_to_the_right_tv() {
        let mut app = AppConfig::default();
        app.tvs.push(TvEntry::new("tv1", "Salon", "10.0.0.1"));
        app.tvs.push(TvEntry::new("tv2", "Chambre", "10.0.0.2"));
        app.active = Some("tv1".into());
        let shared = SharedConfig::from_app(app, None);
        let tv2 = shared.scoped("tv2");
        tv2.update(|c| {
            c.token = Some("t2".into());
            c.smartthings_token = Some("pat".into());
        })
        .unwrap();
        let a = shared.app();
        assert_eq!(a.entry("tv2").unwrap().token.as_deref(), Some("t2"));
        assert!(a.entry("tv1").unwrap().token.is_none());
        assert_eq!(a.smartthings_token.as_deref(), Some("pat"), "account-level");
        assert_eq!(shared.get().id, "tv1", "unscoped follows active");
        assert_eq!(tv2.get().display_name, "Chambre");
    }

    #[test]
    fn first_run_creates_a_tv_on_update() {
        let shared = SharedConfig::from_app(AppConfig::default(), None);
        assert!(shared.get().host.is_empty());
        shared.update(|c| c.name = "Client".into()).unwrap();
        assert!(shared.app().tvs.is_empty(), "no TV without a host");
        shared.update(|c| c.host = "192.0.2.7".into()).unwrap();
        let a = shared.app();
        assert_eq!(a.tvs.len(), 1);
        assert_eq!(a.active.as_deref(), Some("tv1"));
        assert_eq!(a.name, "Client");
        let s = SharedConfig::new(Config { host: "h".into(), ..Default::default() }, None);
        assert_eq!(s.scope(), Some("tv1"));
        assert_eq!(s.get().host, "h");
    }
}
