//! The TV's local REST API on port 8001: device info, power state, and app
//! status/launch/close (`/api/v2/applications/<appId>`).
//!
//! All functions are blocking with short timeouts.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::http::{lan_agent, status_and_body};

/// REST port.
pub const REST_PORT: u16 = 8001;

/// TV power state as far as the network can tell.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PowerState {
    /// Screen on (`device.PowerState == "on"`).
    On,
    /// Network standby: REST answers but the screen is off (`"standby"`).
    Standby,
    /// The REST API does not answer: TV fully off or unreachable.
    Off,
    /// Some other value reported by the TV.
    Unknown(String),
}

impl PowerState {
    /// True when the screen is on.
    pub fn is_on(&self) -> bool {
        matches!(self, PowerState::On)
    }

    fn from_str(s: &str) -> PowerState {
        match s.to_ascii_lowercase().as_str() {
            "on" => PowerState::On,
            "standby" => PowerState::Standby,
            "off" => PowerState::Off,
            other => PowerState::Unknown(other.to_string()),
        }
    }
}

/// Parsed subset of `GET http://<host>:8001/api/v2/` plus the raw JSON.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceInfo {
    /// Friendly name (e.g. `[TV] Samsung Q7F 85`).
    pub name: Option<String>,
    /// Model name (`device.modelName`).
    pub model_name: Option<String>,
    /// Model code (`device.model`).
    pub model: Option<String>,
    /// Power state (`device.PowerState`); `None` on older firmware.
    pub power_state: Option<PowerState>,
    /// Panel resolution, e.g. `3840x2160`.
    pub resolution: Option<String>,
    /// `wireless` or `wired`.
    pub network_type: Option<String>,
    /// Wi-Fi MAC address.
    pub wifi_mac: Option<String>,
    /// IP the TV reports for itself.
    pub ip: Option<String>,
    /// OS string, e.g. `Tizen`.
    pub os: Option<String>,
    /// Firmware / API version.
    pub version: Option<String>,
    /// Device unique id (`device.duid`, else `device.id` / top-level `id`), `uuid:…`.
    #[serde(default)]
    pub duid: Option<String>,
    /// The whole JSON document as returned by the TV.
    pub raw: serde_json::Value,
}

impl DeviceInfo {
    /// Parse the JSON returned by `/api/v2/`.
    pub fn from_json(v: serde_json::Value) -> DeviceInfo {
        let d = &v["device"];
        let s = |val: &serde_json::Value| val.as_str().map(str::to_string);
        DeviceInfo {
            // The TV HTML-escapes its name (`85&quot; QLED`).
            name: s(&d["name"])
                .or_else(|| s(&v["name"]))
                .map(|n| crate::dlna::xml_unescape(&n)),
            model_name: s(&d["modelName"]),
            model: s(&d["model"]),
            power_state: d["PowerState"].as_str().map(PowerState::from_str),
            resolution: s(&d["resolution"]),
            network_type: s(&d["networkType"]),
            wifi_mac: s(&d["wifiMac"]),
            ip: s(&d["ip"]),
            os: s(&d["OS"]),
            version: s(&v["version"]),
            duid: s(&d["duid"]).or_else(|| s(&d["id"])).or_else(|| s(&v["id"])),
            raw: v,
        }
    }

    /// True when the TV reports `networkType: wireless`.
    pub fn is_wireless(&self) -> bool {
        self.tv_network() == Some(crate::netcheck::TvNetwork::Wireless)
    }

    /// How the TV is connected (`None` if it does not say), for
    /// [`crate::netcheck::verdict`] and [`crate::Tv::network_verdict`].
    pub fn tv_network(&self) -> Option<crate::netcheck::TvNetwork> {
        self.network_type.as_deref().and_then(crate::netcheck::TvNetwork::from_network_type)
    }
}

/// Status of one app from `GET /api/v2/applications/<appId>`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AppStatus {
    /// App id.
    pub id: String,
    /// App name.
    pub name: Option<String>,
    /// Whether it is running.
    pub running: bool,
    /// Whether it is in the foreground.
    pub visible: bool,
    /// App version.
    pub version: Option<String>,
}

/// Client for the REST API of one TV.
#[derive(Clone)]
pub struct Rest {
    host: String,
    agent: ureq::Agent,
}

impl std::fmt::Debug for Rest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Rest").field("host", &self.host).finish()
    }
}

impl Rest {
    /// New client (no network traffic until a method is called).
    pub fn new(host: &str) -> Rest {
        Rest {
            host: host.to_string(),
            agent: lan_agent(Duration::from_secs(4)),
        }
    }

    fn base(&self) -> String {
        format!("http://{}:{}/api/v2/", self.host, REST_PORT)
    }

    /// `GET /api/v2/`.
    pub fn device_info(&self) -> Result<DeviceInfo> {
        let v: serde_json::Value = self
            .agent
            .get(&self.base())
            .call()?
            .into_json()
            .map_err(|e| Error::Parse(format!("device info: {e}")))?;
        Ok(DeviceInfo::from_json(v))
    }

    /// Power state; an unreachable TV is reported as [`PowerState::Off`] (never errors).
    pub fn power_state(&self) -> PowerState {
        match self.device_info() {
            Ok(info) => info.power_state.unwrap_or(PowerState::On),
            Err(_) => PowerState::Off,
        }
    }

    fn app_url(&self, app_id: &str) -> String {
        format!("{}applications/{}", self.base(), app_id)
    }

    /// `GET /api/v2/applications/<appId>`.
    pub fn app_status(&self, app_id: &str) -> Result<AppStatus> {
        let (code, body) = status_and_body(self.agent.get(&self.app_url(app_id)).call())?;
        if code >= 400 {
            return Err(Error::Http(format!("app status {app_id}: HTTP {code}")));
        }
        let v: serde_json::Value = serde_json::from_str(&body)?;
        Ok(AppStatus {
            id: v["id"].as_str().unwrap_or(app_id).to_string(),
            name: v["name"].as_str().map(str::to_string),
            running: v["running"].as_bool().unwrap_or(false),
            visible: v["visible"].as_bool().unwrap_or(false),
            version: v["version"].as_str().map(str::to_string),
        })
    }

    /// `POST /api/v2/applications/<appId>` (launch).
    pub fn launch_app(&self, app_id: &str) -> Result<()> {
        let (code, _) = status_and_body(self.agent.post(&self.app_url(app_id)).send_bytes(&[]))?;
        if code >= 400 {
            return Err(Error::Http(format!("launch {app_id}: HTTP {code}")));
        }
        Ok(())
    }

    /// `DELETE /api/v2/applications/<appId>` (close).
    pub fn close_app(&self, app_id: &str) -> Result<()> {
        let (code, _) = status_and_body(self.agent.delete(&self.app_url(app_id)).call())?;
        if code >= 400 {
            return Err(Error::Http(format!("close {app_id}: HTTP {code}")));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_device_info() {
        let v = serde_json::json!({
            "device": {"PowerState": "standby", "modelName": "QE85Q7F", "model": "25_PONTUSM",
                       "resolution": "3840x2160", "networkType": "wireless", "wifiMac": "00:00:5e:00:53:01",
                       "name": "[TV] Samsung", "OS": "Tizen", "ip": "192.0.2.5"},
            "version": "2.0.25"
        });
        let d = DeviceInfo::from_json(v);
        assert_eq!(d.power_state, Some(PowerState::Standby));
        assert!(d.is_wireless());
        assert_eq!(d.resolution.as_deref(), Some("3840x2160"));
        assert_eq!(d.version.as_deref(), Some("2.0.25"));
    }
}
