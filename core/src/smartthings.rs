//! Optional SmartThings cloud control. Exposes what the local APIs cannot: picture
//! mode, input source, and power/volume/channel as a fallback.
//!
//! Authorization: an OAuth login ([`crate::smartthings_auth`], long-lived, refreshed
//! automatically) or, as a legacy fallback, a personal access token (24 h for tokens
//! created after 30 Dec 2024). Use [`crate::smartthings_auth::client_for`] (or
//! `tv.smartthings()` / `fleet.smartthings()`) to get the right client. With OAuth,
//! every request takes its token from [`StAuth::bearer`]; on HTTP 401 it refreshes once
//! and retries once.
//!
//! Only capabilities the device actually reports are offered: every command checks
//! [`StDevice::has`] first and returns [`Error::Unsupported`] otherwise.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::error::{Error, Result};
use crate::http::{status_and_body, tls_agent};
use crate::smartthings_auth::StAuth;

/// SmartThings REST base URL.
pub const API: &str = "https://api.smartthings.com/v1";

/// A SmartThings device with the capabilities of its `main` component.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StDevice {
    /// Device id.
    pub device_id: String,
    /// User label (e.g. "Salon TV").
    pub label: String,
    /// Device name.
    pub name: String,
    /// OCF type, `oic.d.tv` for TVs.
    pub ocf_device_type: Option<String>,
    /// Capability ids of the `main` component.
    pub capabilities: Vec<String>,
    /// OCF information (`ocf` block of the device, completed by the `ocf` capability of
    /// the status when fetched). Used to match the device with a TV on the LAN.
    #[serde(default)]
    pub ocf: OcfInfo,
    /// A MAC address found anywhere in the device JSON, if SmartThings exposes one.
    #[serde(default)]
    pub mac: Option<String>,
}

/// The identification fields SmartThings publishes for an OCF device (Samsung TVs).
///
/// From `GET /devices` (`ocf` block): `name`, `modelNumber`, `firmwareVersion`,
/// `vendorId`, `manufacturerName`. From the `ocf` capability of
/// `GET /devices/{id}/status`: `n`, `mnmo`, `mnfv`, `vid`, `mnmn`, `di`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OcfInfo {
    /// Device name as the TV reports it (`[TV] Samsung Q7F 85`), as REST `device.name`.
    pub name: Option<String>,
    /// Model number (`QE85Q7FAAUXXC`), as REST `device.modelName`.
    pub model_number: Option<String>,
    /// Firmware version (`T-PTMDEUC-…`).
    pub firmware: Option<String>,
    /// Vendor id (`VD-STV-2025`).
    pub vendor_id: Option<String>,
    /// Manufacturer name.
    pub manufacturer: Option<String>,
    /// OCF device id (`di`), a UUID.
    pub di: Option<String>,
}

impl OcfInfo {
    /// From the `ocf` block of a device JSON. Pure.
    pub fn from_device_json(v: &Value) -> OcfInfo {
        let o = &v["ocf"];
        let s = |k: &str| o[k].as_str().filter(|x| !x.trim().is_empty()).map(str::to_string);
        OcfInfo {
            name: s("name"),
            model_number: s("modelNumber"),
            firmware: s("firmwareVersion"),
            vendor_id: s("vendorId"),
            manufacturer: s("manufacturerName"),
            di: None,
        }
    }

    /// Fill missing fields from the `ocf` capability of a status document (whole
    /// document or `components.main`). Pure.
    pub fn fill_from_status(&mut self, status: &Value) {
        let main = if status["components"]["main"].is_object() { &status["components"]["main"] } else { status };
        let a = |k: &str| {
            main["ocf"][k]["value"]
                .as_str()
                .filter(|x| !x.trim().is_empty())
                .map(str::to_string)
        };
        self.name = self.name.take().or_else(|| a("n"));
        self.model_number = self.model_number.take().or_else(|| a("mnmo"));
        self.firmware = self.firmware.take().or_else(|| a("mnfv"));
        self.vendor_id = self.vendor_id.take().or_else(|| a("vid"));
        self.manufacturer = self.manufacturer.take().or_else(|| a("mnmn"));
        self.di = self.di.take().or_else(|| a("di"));
    }
}

/// Find a MAC-looking value under a key containing `mac` anywhere in `v`. Pure.
pub fn find_mac(v: &Value) -> Option<String> {
    match v {
        Value::Object(o) => {
            for (k, val) in o {
                if k.to_ascii_lowercase().contains("mac") {
                    let s = val.as_str().or_else(|| val["value"].as_str());
                    if let Some(s) = s.filter(|s| crate::wol::parse_mac(s).is_ok()) {
                        return Some(s.to_string());
                    }
                }
            }
            o.values().find_map(find_mac)
        }
        Value::Array(a) => a.iter().find_map(find_mac),
        _ => None,
    }
}

/// `GET /devices/{id}/health`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceHealth {
    /// `ONLINE`, `OFFLINE` or something else.
    pub state: String,
    /// `lastUpdatedDate`, if given.
    pub last_updated: Option<String>,
}

impl DeviceHealth {
    /// Parse the health JSON. Pure.
    pub fn from_json(v: &Value) -> DeviceHealth {
        DeviceHealth {
            state: v["state"].as_str().unwrap_or("UNKNOWN").to_string(),
            last_updated: v["lastUpdatedDate"].as_str().map(str::to_string),
        }
    }

    /// True if `state == ONLINE`.
    pub fn is_online(&self) -> bool {
        self.state.eq_ignore_ascii_case("ONLINE")
    }
}

impl StDevice {
    /// Does the device report this capability?
    pub fn has(&self, cap: &str) -> bool {
        self.capabilities.iter().any(|c| c == cap)
    }

    /// Heuristic: is it a TV?
    pub fn is_tv(&self) -> bool {
        self.ocf_device_type.as_deref() == Some("oic.d.tv")
            || self.has("tvChannel")
            || self.capabilities.iter().any(|c| c.starts_with("samsungvd."))
    }

    /// Parse one item of `GET /devices`.
    pub fn from_json(v: &Value) -> Option<StDevice> {
        let caps = v["components"]
            .as_array()
            .and_then(|cs| cs.iter().find(|c| c["id"] == "main").or(cs.first()))
            .and_then(|c| c["capabilities"].as_array())
            .map(|a| a.iter().filter_map(|c| c["id"].as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        Some(StDevice {
            device_id: v["deviceId"].as_str()?.to_string(),
            label: v["label"].as_str().unwrap_or_default().to_string(),
            name: v["name"].as_str().unwrap_or_default().to_string(),
            ocf_device_type: v["ocf"]["ocfDeviceType"]
                .as_str()
                .or(v["ocfDeviceType"].as_str())
                .map(str::to_string),
            capabilities: caps,
            ocf: OcfInfo::from_device_json(v),
            mac: find_mac(v),
        })
    }
}

/// Typed view of the TV's `main` component status.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct TvStatus {
    /// `switch.switch` == "on".
    pub power_on: Option<bool>,
    /// `audioVolume.volume`.
    pub volume: Option<u8>,
    /// `audioMute.mute` == "muted".
    pub muted: Option<bool>,
    /// Current input source (`mediaInputSource` or `samsungvd.mediaInputSource`).
    pub input_source: Option<String>,
    /// Supported input sources (id, display name).
    pub input_sources: Vec<(String, String)>,
    /// Current picture mode (`samsungvd.pictureMode`, or `custom.picturemode` on older TVs).
    pub picture_mode: Option<String>,
    /// Supported picture modes (id, display name).
    pub picture_modes: Vec<(String, String)>,
    /// Current sound mode (`samsungvd.soundMode`).
    pub sound_mode: Option<String>,
    /// Supported sound modes (id, display name).
    pub sound_modes: Vec<(String, String)>,
    /// Current channel (`tvChannel.tvChannel`).
    pub channel: Option<String>,
    /// `samsungvd.thingStatus.status` when the TV reports it (attribute not verified on
    /// a real TV; [`SmartThings::health`] is the reliable online signal).
    #[serde(default)]
    pub thing_status: Option<String>,
    /// The raw status JSON of the `main` component.
    pub raw: Value,
}

fn attr<'a>(main: &'a Value, cap: &str, a: &str) -> &'a Value {
    &main[cap][a]["value"]
}

/// Parse a list attribute that may be `["HDMI1", ...]`, `[{"id":..,"name":..}]`,
/// or `[{"modeName":..}]`.
fn pairs(v: &Value) -> Vec<(String, String)> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| match x {
                    Value::String(s) => Some((s.clone(), s.clone())),
                    Value::Object(o) => {
                        let id = o.get("id").or(o.get("modeName")).and_then(Value::as_str)?;
                        let name = o.get("name").or(o.get("modeName")).and_then(Value::as_str).unwrap_or(id);
                        Some((id.to_string(), name.to_string()))
                    }
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Picture-mode capabilities, newest first.
pub const PICTURE_CAPS: [&str; 2] = ["samsungvd.pictureMode", "custom.picturemode"];
/// Sound-mode capability.
pub const SOUND_CAP: &str = "samsungvd.soundMode";

/// The picture-mode capability this device reports, if any.
pub fn picture_cap(dev: &StDevice) -> Option<&'static str> {
    PICTURE_CAPS.into_iter().find(|c| dev.has(c))
}

impl TvStatus {
    /// Parse `GET /devices/{id}/status` (whole document or just `components.main`).
    pub fn from_status_json(v: &Value) -> TvStatus {
        let main = if v["components"]["main"].is_object() { &v["components"]["main"] } else { v };
        let input = attr(main, "samsungvd.mediaInputSource", "inputSource")
            .as_str()
            .or(attr(main, "mediaInputSource", "inputSource").as_str())
            .map(str::to_string);
        let mut inputs = pairs(attr(main, "samsungvd.mediaInputSource", "supportedInputSourcesMap"));
        if inputs.is_empty() {
            inputs = pairs(attr(main, "mediaInputSource", "supportedInputSources"));
        }
        // 2025 TVs report `samsungvd.pictureMode`; older ones `custom.picturemode`.
        let mut modes = Vec::new();
        let mut mode = None;
        for cap in PICTURE_CAPS {
            modes = pairs(attr(main, cap, "supportedPictureModesMap"));
            if modes.is_empty() {
                modes = pairs(attr(main, cap, "supportedPictureModes"));
            }
            mode = attr(main, cap, "pictureMode").as_str().map(str::to_string);
            if !modes.is_empty() || mode.is_some() {
                break;
            }
        }
        let mut sounds = pairs(attr(main, SOUND_CAP, "supportedSoundModesMap"));
        if sounds.is_empty() {
            sounds = pairs(attr(main, SOUND_CAP, "supportedSoundModes"));
        }
        TvStatus {
            power_on: attr(main, "switch", "switch").as_str().map(|s| s == "on"),
            volume: attr(main, "audioVolume", "volume").as_u64().map(|v| v.min(100) as u8),
            muted: attr(main, "audioMute", "mute").as_str().map(|s| s == "muted"),
            input_source: input,
            input_sources: inputs,
            picture_mode: mode,
            picture_modes: modes,
            sound_mode: attr(main, SOUND_CAP, "soundMode").as_str().map(str::to_string),
            sound_modes: sounds,
            channel: attr(main, "tvChannel", "tvChannel").as_str().map(str::to_string),
            thing_status: attr(main, "samsungvd.thingStatus", "status").as_str().map(str::to_string),
            raw: main.clone(),
        }
    }
}

/// Find `input` in a supported-values map of `(id, display name)` pairs (as in
/// [`TvStatus::picture_modes`] / [`TvStatus::input_sources`]). Pure.
///
/// Matches the id exactly first, then the id case-insensitively, then the display
/// name case-insensitively (surrounding spaces ignored).
pub fn resolve_choice<'a>(supported: &'a [(String, String)], input: &str) -> Option<&'a (String, String)> {
    let t = input.trim();
    supported
        .iter()
        .find(|(id, _)| id == t)
        .or_else(|| supported.iter().find(|(id, _)| id.eq_ignore_ascii_case(t)))
        .or_else(|| supported.iter().find(|(_, name)| name.trim().to_lowercase() == t.to_lowercase()))
}

fn resolve_arg(supported: &[(String, String)], input: &str, what: &str, use_name: bool) -> Result<String> {
    let t = input.trim();
    if t.is_empty() {
        return Err(Error::parse(format!("empty {what}")));
    }
    if supported.is_empty() {
        // The TV did not publish a list: pass the value through unchanged.
        return Ok(t.to_string());
    }
    match resolve_choice(supported, t) {
        Some((id, name)) => Ok(if use_name { name.clone() } else { id.clone() }),
        None => {
            let list: Vec<String> = supported
                .iter()
                .map(|(id, name)| if id == name { id.clone() } else { format!("{name} ({id})") })
                .collect();
            Err(Error::Unsupported(format!("unknown {what} '{t}' (the TV supports: {})", list.join(", "))))
        }
    }
}

/// The `setPictureMode` argument for `mode`, given as the mode id (`modeMovie`) or its
/// display name (`movie`): resolved with [`resolve_choice`] against `supported`
/// ([`TvStatus::picture_modes`]) and returned as the **display name**, which is what
/// SmartThings expects and what [`TvStatus::picture_mode`] reports. An empty
/// `supported` list passes `mode` through; an unknown mode is [`Error::Unsupported`]
/// (listing the supported ones). Pure.
pub fn picture_mode_arg(supported: &[(String, String)], mode: &str) -> Result<String> {
    resolve_arg(supported, mode, "picture mode", true)
}

/// The `setInputSource` argument for `source`, given as the source id (`HDMI1`) or its
/// display name (`PlayStation`): resolved with [`resolve_choice`] against `supported`
/// ([`TvStatus::input_sources`]) and returned as the **id**, which is what SmartThings
/// expects and what [`TvStatus::input_source`] reports. An empty `supported` list
/// passes `source` through; an unknown source is [`Error::Unsupported`]. Pure.
pub fn input_source_arg(supported: &[(String, String)], source: &str) -> Result<String> {
    resolve_arg(supported, source, "input source", false)
}

/// Build a `POST /devices/{id}/commands` body. Pure.
pub fn command_body(capability: &str, command: &str, args: &[Value]) -> Value {
    json!({"commands": [{"component": "main", "capability": capability, "command": command, "arguments": args}]})
}

/// Where a [`SmartThings`] client gets its bearer token.
#[derive(Clone)]
enum Auth {
    /// Personal access token (legacy).
    Pat(String),
    /// OAuth access token, refreshed as needed.
    OAuth(StAuth),
}

/// SmartThings API client.
#[derive(Clone)]
pub struct SmartThings {
    auth: Auth,
    agent: ureq::Agent,
}

impl std::fmt::Debug for SmartThings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kind = match self.auth {
            Auth::Pat(_) => "PAT",
            Auth::OAuth(_) => "OAuth",
        };
        f.debug_struct("SmartThings").field("auth", &kind).field("token", &"<redacted>").finish()
    }
}

impl SmartThings {
    /// New client with a personal access token (no traffic until used).
    pub fn new(token: &str) -> Result<SmartThings> {
        if token.trim().is_empty() {
            return Err(Error::SmartThingsNotConnected);
        }
        Ok(SmartThings {
            auth: Auth::Pat(token.trim().to_string()),
            agent: tls_agent(Duration::from_secs(10))?,
        })
    }

    /// New client using the OAuth login of `auth` (no traffic until used).
    pub fn with_oauth(auth: StAuth) -> Result<SmartThings> {
        Ok(SmartThings { auth: Auth::OAuth(auth), agent: tls_agent(Duration::from_secs(10))? })
    }

    /// True if this client uses OAuth (false: a personal access token).
    pub fn uses_oauth(&self) -> bool {
        matches!(self.auth, Auth::OAuth(_))
    }

    /// Run `req` with a bearer token; with OAuth, a 401 refreshes once and retries once.
    /// `req` sends the request with the given `Authorization` value and returns
    /// [`status_and_body`].
    fn call(&self, req: impl Fn(&str) -> Result<(u16, String)>) -> Result<(u16, String)> {
        match &self.auth {
            Auth::Pat(t) => req(&format!("Bearer {t}")),
            Auth::OAuth(a) => {
                let tok = a.bearer()?;
                let (code, body) = req(&format!("Bearer {tok}"))?;
                if code != 401 {
                    return Ok((code, body));
                }
                let tok2 = a.after_unauthorized(&tok)?;
                req(&format!("Bearer {tok2}"))
            }
        }
    }

    fn get(&self, path: &str) -> Result<Value> {
        let url = format!("{API}{path}");
        let (code, body) = self.call(|auth| status_and_body(self.agent.get(&url).set("Authorization", auth).call()))?;
        if code == 401 || code == 403 {
            return Err(Error::SmartThings(match self.auth {
                Auth::Pat(_) => "token rejected (expired? PATs last 24 h; check its scopes: r:devices:*, x:devices:*)".into(),
                Auth::OAuth(_) => format!("access refused (HTTP {code}): log in again if this persists"),
            }));
        }
        if code >= 400 {
            return Err(Error::SmartThings(format!("GET {path}: HTTP {code}")));
        }
        Ok(serde_json::from_str(&body)?)
    }

    /// All devices visible to the token.
    pub fn devices(&self) -> Result<Vec<StDevice>> {
        let v = self.get("/devices")?;
        Ok(v["items"].as_array().map(|a| a.iter().filter_map(StDevice::from_json).collect()).unwrap_or_default())
    }

    /// One device (with capabilities).
    pub fn device(&self, device_id: &str) -> Result<StDevice> {
        StDevice::from_json(&self.get(&format!("/devices/{device_id}"))?)
            .ok_or_else(|| Error::SmartThings("malformed device".into()))
    }

    /// The first TV among the devices.
    pub fn find_tv(&self) -> Result<Option<StDevice>> {
        Ok(self.devices()?.into_iter().find(StDevice::is_tv))
    }

    /// The raw status document of a device (all components).
    pub fn status_json(&self, device_id: &str) -> Result<Value> {
        self.get(&format!("/devices/{device_id}/status"))
    }

    /// Device health (`ONLINE` / `OFFLINE`).
    pub fn health(&self, device_id: &str) -> Result<DeviceHealth> {
        Ok(DeviceHealth::from_json(&self.get(&format!("/devices/{device_id}/health"))?))
    }

    /// All TVs of the account ([`StDevice::is_tv`]).
    pub fn tvs(&self) -> Result<Vec<StDevice>> {
        Ok(self.devices()?.into_iter().filter(StDevice::is_tv).collect())
    }

    /// Status of the device's main component.
    pub fn status(&self, device_id: &str) -> Result<TvStatus> {
        Ok(TvStatus::from_status_json(&self.get(&format!("/devices/{device_id}/status"))?))
    }

    /// Send any command (no capability check).
    pub fn command(&self, device_id: &str, capability: &str, command: &str, args: &[Value]) -> Result<()> {
        let url = format!("{API}/devices/{device_id}/commands");
        let (code, body) = self.call(|auth| {
            status_and_body(
                self.agent
                    .post(&url)
                    .set("Authorization", auth)
                    .send_json(command_body(capability, command, args)),
            )
        })?;
        if code >= 400 {
            let msg = serde_json::from_str::<Value>(&body)
                .ok()
                .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
                .unwrap_or_else(|| format!("HTTP {code}"));
            return Err(Error::SmartThings(format!("{capability}.{command}: {msg}")));
        }
        Ok(())
    }

    fn checked(&self, dev: &StDevice, cap: &str, command: &str, args: &[Value]) -> Result<()> {
        if !dev.has(cap) {
            return Err(Error::Unsupported(format!("the TV does not report the {cap} capability")));
        }
        self.command(&dev.device_id, cap, command, args)
    }

    /// Power on/off (`switch`).
    pub fn set_power(&self, dev: &StDevice, on: bool) -> Result<()> {
        self.checked(dev, "switch", if on { "on" } else { "off" }, &[])
    }

    /// Volume 0-100 (`audioVolume`).
    pub fn set_volume(&self, dev: &StDevice, v: u8) -> Result<()> {
        self.checked(dev, "audioVolume", "setVolume", &[json!(v.min(100))])
    }

    /// Mute (`audioMute`).
    pub fn set_mute(&self, dev: &StDevice, mute: bool) -> Result<()> {
        self.checked(dev, "audioMute", if mute { "mute" } else { "unmute" }, &[])
    }

    fn input_cap(dev: &StDevice) -> &'static str {
        if dev.has("samsungvd.mediaInputSource") {
            "samsungvd.mediaInputSource"
        } else {
            "mediaInputSource"
        }
    }

    /// Switch the input source, given as its id (`HDMI1`) or display name
    /// (`PlayStation`, case-insensitive), via `samsungvd.mediaInputSource` or
    /// `mediaInputSource`. Fetches the status once to resolve the name (see
    /// [`input_source_arg`]); use [`SmartThings::set_input_source_from`] if you already
    /// have [`TvStatus::input_sources`].
    pub fn set_input_source(&self, dev: &StDevice, source: &str) -> Result<()> {
        if !dev.has(Self::input_cap(dev)) {
            return Err(Error::Unsupported("the TV does not report an input source capability".into()));
        }
        let st = self.status(&dev.device_id)?;
        self.set_input_source_from(dev, &st.input_sources, source)
    }

    /// Like [`SmartThings::set_input_source`], resolving against a supported-sources
    /// map you already have (no status request).
    pub fn set_input_source_from(&self, dev: &StDevice, supported: &[(String, String)], source: &str) -> Result<()> {
        let arg = input_source_arg(supported, source)?;
        self.checked(dev, Self::input_cap(dev), "setInputSource", &[json!(arg)])
    }

    /// Set the picture mode (`custom.picturemode`), given as its id (`modeMovie`) or
    /// display name (`Movie`, `filmmaker mode`, case-insensitive). Fetches the status
    /// once to resolve it (see [`picture_mode_arg`]); use
    /// [`SmartThings::set_picture_mode_from`] if you already have
    /// [`TvStatus::picture_modes`].
    pub fn set_picture_mode(&self, dev: &StDevice, mode: &str) -> Result<()> {
        if picture_cap(dev).is_none() {
            return Err(Error::Unsupported("the TV does not report a picture-mode capability".into()));
        }
        let st = self.status(&dev.device_id)?;
        self.set_picture_mode_from(dev, &st.picture_modes, mode)
    }

    /// Like [`SmartThings::set_picture_mode`], resolving against a supported-modes map
    /// you already have (no status request).
    pub fn set_picture_mode_from(&self, dev: &StDevice, supported: &[(String, String)], mode: &str) -> Result<()> {
        let arg = picture_mode_arg(supported, mode)?;
        let cap = picture_cap(dev)
            .ok_or_else(|| Error::Unsupported("the TV does not report a picture-mode capability".into()))?;
        self.checked(dev, cap, "setPictureMode", &[json!(arg)])
    }

    /// Set the sound mode (`samsungvd.soundMode`), given as its id (`modeStandard`) or
    /// display name (`Standard`). Fetches the status once to resolve it.
    pub fn set_sound_mode(&self, dev: &StDevice, mode: &str) -> Result<()> {
        if !dev.has(SOUND_CAP) {
            return Err(Error::Unsupported("the TV does not report samsungvd.soundMode".into()));
        }
        let st = self.status(&dev.device_id)?;
        self.set_sound_mode_from(dev, &st.sound_modes, mode)
    }

    /// Like [`SmartThings::set_sound_mode`], resolving against a supported-modes map you
    /// already have ([`TvStatus::sound_modes`]).
    pub fn set_sound_mode_from(&self, dev: &StDevice, supported: &[(String, String)], mode: &str) -> Result<()> {
        // Same convention as picture modes: SmartThings takes the display name.
        let arg = picture_mode_arg(supported, mode)?;
        self.checked(dev, SOUND_CAP, "setSoundMode", &[json!(arg)])
    }

    /// Channel up.
    pub fn channel_up(&self, dev: &StDevice) -> Result<()> {
        self.checked(dev, "tvChannel", "channelUp", &[])
    }

    /// Channel down.
    pub fn channel_down(&self, dev: &StDevice) -> Result<()> {
        self.checked(dev, "tvChannel", "channelDown", &[])
    }

    /// Set channel number.
    pub fn set_channel(&self, dev: &StDevice, channel: &str) -> Result<()> {
        self.checked(dev, "tvChannel", "setTvChannel", &[json!(channel)])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_device_and_status() {
        let d = StDevice::from_json(&json!({
            "deviceId": "abc", "label": "Salon", "name": "Samsung TV",
            "ocf": {"ocfDeviceType": "oic.d.tv"},
            "components": [{"id": "main", "capabilities": [{"id": "switch"}, {"id": "custom.picturemode"}, {"id": "samsungvd.mediaInputSource"}]}]
        }))
        .unwrap();
        assert!(d.is_tv());
        assert!(d.has("custom.picturemode"));
        assert!(!d.has("tvChannel"));

        let st = TvStatus::from_status_json(&json!({"components": {"main": {
            "switch": {"switch": {"value": "on"}},
            "audioVolume": {"volume": {"value": 12, "unit": "%"}},
            "audioMute": {"mute": {"value": "unmuted"}},
            "custom.picturemode": {"pictureMode": {"value": "Movie"},
                "supportedPictureModesMap": {"value": [{"id": "modeMovie", "name": "Movie"}, {"id": "modeStandard", "name": "Standard"}]}},
            "samsungvd.mediaInputSource": {"inputSource": {"value": "HDMI2"},
                "supportedInputSourcesMap": {"value": [{"id": "dtv", "name": "TV"}, {"id": "HDMI2", "name": "PlayStation"}]}}
        }}}));
        assert_eq!(st.power_on, Some(true));
        assert_eq!(st.volume, Some(12));
        assert_eq!(st.muted, Some(false));
        assert_eq!(st.picture_mode.as_deref(), Some("Movie"));
        assert_eq!(st.picture_modes.len(), 2);
        assert_eq!(st.input_source.as_deref(), Some("HDMI2"));
        assert_eq!(st.input_sources[1], ("HDMI2".into(), "PlayStation".into()));
        let b = command_body("audioVolume", "setVolume", &[json!(10)]);
        assert_eq!(b["commands"][0]["component"], "main");
    }

    fn map(v: &[(&str, &str)]) -> Vec<(String, String)> {
        v.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect()
    }

    #[test]
    fn resolve_picture_modes_and_inputs() {
        let modes = map(&[("modeStandard", "Standard"), ("modeMovie", "Movie"), ("modeFilmmaker", "FILMMAKER MODE")]);
        // Id (exact or any case) or display name (any case) → display name.
        assert_eq!(picture_mode_arg(&modes, "modeMovie").unwrap(), "Movie");
        assert_eq!(picture_mode_arg(&modes, "MODEMOVIE").unwrap(), "Movie");
        assert_eq!(picture_mode_arg(&modes, "movie").unwrap(), "Movie");
        assert_eq!(picture_mode_arg(&modes, " filmmaker mode ").unwrap(), "FILMMAKER MODE");
        assert!(matches!(picture_mode_arg(&modes, "Vivid"), Err(Error::Unsupported(m)) if m.contains("Movie (modeMovie)")));
        assert!(picture_mode_arg(&modes, "  ").is_err());
        // No published list: passed through.
        assert_eq!(picture_mode_arg(&[], "Dynamic").unwrap(), "Dynamic");

        let inputs = map(&[("dtv", "TV"), ("HDMI1", "HDMI1"), ("HDMI2", "PlayStation")]);
        // Id or display name → id.
        assert_eq!(input_source_arg(&inputs, "HDMI2").unwrap(), "HDMI2");
        assert_eq!(input_source_arg(&inputs, "hdmi1").unwrap(), "HDMI1");
        assert_eq!(input_source_arg(&inputs, "playstation").unwrap(), "HDMI2");
        assert_eq!(input_source_arg(&inputs, "tv").unwrap(), "dtv");
        assert!(input_source_arg(&inputs, "HDMI4").is_err());
        assert_eq!(input_source_arg(&[], "HDMI3").unwrap(), "HDMI3");

        // An id wins over another entry's display name.
        let tricky = map(&[("a", "B"), ("B", "c")]);
        assert_eq!(resolve_choice(&tricky, "B").unwrap().0, "B");
        assert_eq!(resolve_choice(&tricky, "b").unwrap().0, "B");
        assert_eq!(resolve_choice(&tricky, "C").unwrap().0, "B");
        assert!(resolve_choice(&tricky, "x").is_none());
    }
}
