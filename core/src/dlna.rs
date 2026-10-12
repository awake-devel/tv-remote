//! DLNA / UPnP media renderer control (port 9197), ported from the tested
//! `~/.local/bin/tv-cast` script: SOAP envelope, fault parsing, DIDL-Lite metadata
//! with DLNA flags, and `Play` retried while the TV says "Transport is locked".

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::http::lan_agent;

/// DLNA renderer port.
pub const DLNA_PORT: u16 = 9197;
/// AVTransport service type.
pub const AVT: &str = "urn:schemas-upnp-org:service:AVTransport:1";
/// RenderingControl service type.
pub const RC: &str = "urn:schemas-upnp-org:service:RenderingControl:1";
/// ConnectionManager service type.
pub const CM: &str = "urn:schemas-upnp-org:service:ConnectionManager:1";
/// DLNA flags used in DIDL `protocolInfo` (as in tv-cast).
pub const DLNA_FLAGS: &str = "DLNA.ORG_OP=01;DLNA.ORG_CI=0;DLNA.ORG_FLAGS=01700000000000000000000000000000";

/// Number of `Play` attempts while the transport is locked (×0.5 s).
pub const PLAY_RETRIES: u32 = 20;

/// Escape like Python's `html.escape(s, quote=True)`.
pub fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#x27;"),
            c => out.push(c),
        }
    }
    out
}

/// Reverse of [`xml_escape`], plus numeric character references (like `html.unescape`
/// for the entities the TV actually uses).
pub fn xml_unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        if let Some(end) = tail.find(';').filter(|&e| e <= 12) {
            let ent = &tail[1..end];
            let rep = match ent {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                _ if ent.starts_with("#x") || ent.starts_with("#X") => {
                    u32::from_str_radix(&ent[2..], 16).ok().and_then(char::from_u32)
                }
                _ if ent.starts_with('#') => ent[1..].parse().ok().and_then(char::from_u32),
                _ => None,
            };
            if let Some(c) = rep {
                out.push(c);
                rest = &tail[end + 1..];
                continue;
            }
        }
        out.push('&');
        rest = &tail[1..];
    }
    out.push_str(rest);
    out
}

/// Build the SOAP envelope exactly as tv-cast does (`InstanceID` 0 first, then `args`).
pub fn soap_envelope(service: &str, action: &str, args: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?><s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
         s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\"><s:Body><u:{action} xmlns:u=\"{service}\">\
         <InstanceID>0</InstanceID>{args}</u:{action}></s:Body></s:Envelope>"
    )
}

/// First `<name>…</name>` text in `xml`, unescaped; empty string when absent.
pub fn field(xml: &str, name: &str) -> String {
    let open = format!("<{name}>");
    match xml.find(&open) {
        Some(i) => {
            let start = i + open.len();
            let end = xml[start..].find('<').map(|e| start + e).unwrap_or(xml.len());
            xml_unescape(&xml[start..end])
        }
        None => String::new(),
    }
}

/// Is this SOAP fault a possibly temporary "not now" (UPnP 701 *Transition not
/// available*, 705 *Transport is locked*)? Right after a `Seek` the 2025 TV briefly
/// refuses `Pause` / another `Seek` with 701; measured on the LAN it clears within
/// ~0.2 s. But 701 is also the TV's permanent answer to an impossible transition (Pause
/// while already paused or stopped, Seek in a live stream), so retries are bounded by
/// [`settle_delay`]. Pure.
pub fn is_transient_refusal(err: &Error) -> bool {
    match err {
        Error::Soap { detail, .. } => {
            let d = detail.to_ascii_lowercase();
            d.contains("transition not available") || d.contains("transport is locked")
        }
        _ => false,
    }
}

/// How many times [`Dlna::pause`] and [`Dlna::seek`] try while the TV refuses
/// transiently (×0.2 s: at most ~1.4 s of waiting, so a permanent 701 fails fast).
pub const SETTLE_RETRIES: u32 = 8;

/// Wait before retry number `attempt + 1` of a transient refusal, `None` once
/// [`SETTLE_RETRIES`] tries were made. Pure.
pub fn settle_delay(attempt: u32) -> Option<Duration> {
    (attempt + 1 < SETTLE_RETRIES).then_some(Duration::from_millis(200))
}

/// A SOAP refusal reworded for people (`TV refused Seek: Illegal seek target` → "the TV
/// can't jump there …"); other errors are returned as they are. Pure.
pub fn friendly(err: Error) -> Error {
    let Error::Soap { action, detail } = &err else { return err };
    let d = detail.to_ascii_lowercase();
    let text = if d.contains("illegal seek target") || d.contains("711") {
        "the TV can't jump to that point (it is at or past the end)"
    } else if d.contains("transition not available") || d.contains("701") {
        match action.as_str() {
            "Seek" => "the TV can't seek right now (a live stream, or it is still buffering)",
            "Pause" => "the TV can't pause right now (already paused or stopped?)",
            _ => "the TV can't do that right now: try again in a moment",
        }
    } else if d.contains("transport is locked") || d.contains("705") {
        "the TV is busy loading: try again in a moment"
    } else {
        return err;
    };
    Error::Playback(text.into())
}

/// `errorDescription` from a SOAP fault body, if present.
pub fn parse_fault(body: &str) -> Option<String> {
    let d = field(body, "errorDescription");
    (!d.is_empty()).then_some(d)
}

/// UPnP class for a MIME type: `videoItem`, `audioItem` or `imageItem.photo`.
pub fn upnp_class(mime: &str) -> &'static str {
    if mime.starts_with("video") {
        "videoItem"
    } else if mime.starts_with("audio") {
        "audioItem"
    } else {
        "imageItem.photo"
    }
}

/// DIDL-Lite metadata for `SetAVTransportURI`, as in tv-cast.
///
/// `duration` (optional, `H:MM:SS.mmm`) adds a `duration` attribute on `<res>`; tv-cast
/// never sets it, and direct casts pass `None` to stay byte-identical with it. Live
/// TV-friendly streams pass the source duration so the TV can show a progress bar.
pub fn didl_metadata(uri: &str, title: &str, mime: &str, duration: Option<&str>) -> String {
    let kind = upnp_class(mime);
    let dur = duration
        .map(|d| format!(" duration=\"{}\"", xml_escape(d)))
        .unwrap_or_default();
    format!(
        "<DIDL-Lite xmlns=\"urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/\" xmlns:dc=\"http://purl.org/dc/elements/1.1/\" \
         xmlns:upnp=\"urn:schemas-upnp-org:metadata-1-0/upnp/\"><item id=\"0\" parentID=\"-1\" restricted=\"1\">\
         <dc:title>{}</dc:title><upnp:class>object.item.{kind}</upnp:class>\
         <res protocolInfo=\"http-get:*:{mime}:{DLNA_FLAGS}\"{dur}>{}</res></item></DIDL-Lite>",
        xml_escape(title),
        xml_escape(uri)
    )
}

/// `SetAVTransportURI` arguments (URI and DIDL, both escaped once more).
pub fn set_uri_args(uri: &str, didl: &str) -> String {
    format!(
        "<CurrentURI>{}</CurrentURI><CurrentURIMetaData>{}</CurrentURIMetaData>",
        xml_escape(uri),
        xml_escape(didl)
    )
}

/// Format a duration as UPnP `H:MM:SS`.
pub fn format_hms(d: Duration) -> String {
    let s = d.as_secs();
    format!("{}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
}

/// Parse UPnP `H:MM:SS[.fff]` (returns `None` for `NOT_IMPLEMENTED`, empty, etc.).
pub fn parse_hms(s: &str) -> Option<Duration> {
    let mut parts = s.trim().split(':');
    let h: f64 = parts.next()?.parse().ok()?;
    let m: f64 = parts.next()?.parse().ok()?;
    let sec: f64 = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    let total = h * 3600.0 + m * 60.0 + sec;
    (total.is_finite() && total >= 0.0).then(|| Duration::from_secs_f64(total))
}

/// AVTransport `CurrentTransportState`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TransportState {
    /// `PLAYING`.
    Playing,
    /// `PAUSED_PLAYBACK`.
    Paused,
    /// `STOPPED`.
    Stopped,
    /// `TRANSITIONING` (loading / buffering).
    Transitioning,
    /// `NO_MEDIA_PRESENT`.
    NoMedia,
    /// Anything else.
    Other(String),
}

impl TransportState {
    /// Parse the wire value.
    pub fn parse(s: &str) -> TransportState {
        match s {
            "PLAYING" => TransportState::Playing,
            "PAUSED_PLAYBACK" => TransportState::Paused,
            "STOPPED" => TransportState::Stopped,
            "TRANSITIONING" => TransportState::Transitioning,
            "NO_MEDIA_PRESENT" => TransportState::NoMedia,
            o => TransportState::Other(o.to_string()),
        }
    }

    /// Stopped or no media.
    pub fn is_idle(&self) -> bool {
        matches!(self, TransportState::Stopped | TransportState::NoMedia)
    }
}

/// `GetTransportInfo` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransportInfo {
    /// Current state.
    pub state: TransportState,
    /// `CurrentTransportStatus` (`OK` / `ERROR_OCCURRED`).
    pub status: String,
}

/// `GetPositionInfo` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PositionInfo {
    /// Elapsed time (`RelTime`).
    pub position: Option<Duration>,
    /// Track length (`TrackDuration`).
    pub duration: Option<Duration>,
    /// URI being played.
    pub track_uri: String,
    /// DIDL-Lite of the track (unescaped once), if the TV reports it.
    pub track_metadata: String,
}

/// DLNA renderer client for one TV. Cheap to clone; `Send + Sync`.
#[derive(Clone)]
pub struct Dlna {
    host: String,
    agent: ureq::Agent,
}

impl std::fmt::Debug for Dlna {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Dlna").field("host", &self.host).finish()
    }
}

impl Dlna {
    /// New client (no traffic until a method is called).
    pub fn new(host: &str) -> Dlna {
        Dlna {
            host: host.to_string(),
            agent: lan_agent(Duration::from_secs(10)),
        }
    }

    /// TV host.
    pub fn host(&self) -> &str {
        &self.host
    }

    /// Send one SOAP action; returns the response body. Faults become [`Error::Soap`].
    pub fn soap(&self, service: &str, action: &str, args: &str) -> Result<String> {
        let path = match service {
            AVT => "AVTransport1",
            CM => "ConnectionManager1",
            _ => "RenderingControl1",
        };
        let url = format!("http://{}:{DLNA_PORT}/upnp/control/{path}", self.host);
        let body = soap_envelope(service, action, args);
        let r = self
            .agent
            .post(&url)
            .set("Content-Type", "text/xml; charset=\"utf-8\"")
            .set("SOAPACTION", &format!("\"{service}#{action}\""))
            .send_string(&body);
        match r {
            Ok(resp) => Ok(resp.into_string()?),
            Err(ureq::Error::Status(code, resp)) => {
                let text = resp.into_string().unwrap_or_default();
                Err(Error::Soap {
                    action: action.to_string(),
                    detail: parse_fault(&text).unwrap_or_else(|| format!("HTTP {code}")),
                })
            }
            Err(e) => Err(e.into()),
        }
    }

    /// `SetAVTransportURI` only (no Stop/Play).
    pub fn set_uri(&self, uri: &str, title: &str, mime: &str, duration: Option<&str>) -> Result<()> {
        let didl = didl_metadata(uri, title, mime, duration);
        self.soap(AVT, "SetAVTransportURI", &set_uri_args(uri, &didl))?;
        Ok(())
    }

    /// tv-cast's `play_uri`: Stop (errors ignored), SetAVTransportURI, then Play retried
    /// up to [`PLAY_RETRIES`] times 0.5 s apart while the TV refuses (transport locked).
    pub fn play_uri(&self, uri: &str, title: &str, mime: &str, duration: Option<&str>) -> Result<()> {
        let _ = self.stop(); // nothing loaded yet is fine
        self.set_uri(uri, title, mime, duration)?;
        let mut attempt = 0;
        loop {
            match self.play() {
                Ok(()) => return Ok(()),
                Err(e @ Error::Soap { .. }) => {
                    attempt += 1;
                    if attempt >= PLAY_RETRIES {
                        return Err(e);
                    }
                    std::thread::sleep(Duration::from_millis(500));
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// `Play` (speed 1).
    pub fn play(&self) -> Result<()> {
        self.soap(AVT, "Play", "<Speed>1</Speed>").map(|_| ())
    }

    /// An AVTransport action, retried (see [`settle_delay`]) while the TV answers with a
    /// possibly transient refusal ([`is_transient_refusal`]).
    fn settled(&self, action: &str, args: &str) -> Result<()> {
        let mut attempt = 0;
        loop {
            match self.soap(AVT, action, args) {
                Ok(_) => return Ok(()),
                Err(e) if is_transient_refusal(&e) => match settle_delay(attempt) {
                    Some(d) => {
                        attempt += 1;
                        std::thread::sleep(d);
                    }
                    None => return Err(e),
                },
                Err(e) => return Err(e),
            }
        }
    }

    /// `Pause` (retried while the TV is still settling, e.g. right after a seek).
    pub fn pause(&self) -> Result<()> {
        self.settled("Pause", "")
    }

    /// `Stop`.
    pub fn stop(&self) -> Result<()> {
        self.soap(AVT, "Stop", "").map(|_| ())
    }

    /// `Seek` to an absolute position (`REL_TIME`). Not possible for live streams.
    pub fn seek(&self, to: Duration) -> Result<()> {
        self.settled("Seek", &format!("<Unit>REL_TIME</Unit><Target>{}</Target>", format_hms(to)))
    }

    /// `GetTransportInfo`.
    pub fn transport_info(&self) -> Result<TransportInfo> {
        let x = self.soap(AVT, "GetTransportInfo", "")?;
        Ok(TransportInfo {
            state: TransportState::parse(&field(&x, "CurrentTransportState")),
            status: field(&x, "CurrentTransportStatus"),
        })
    }

    /// `GetPositionInfo`.
    pub fn position_info(&self) -> Result<PositionInfo> {
        let x = self.soap(AVT, "GetPositionInfo", "")?;
        Ok(PositionInfo {
            position: parse_hms(&field(&x, "RelTime")),
            duration: parse_hms(&field(&x, "TrackDuration")),
            track_uri: field(&x, "TrackURI"),
            track_metadata: field(&x, "TrackMetaData"),
        })
    }

    /// `GetVolume` (Master), 0-100.
    pub fn volume(&self) -> Result<u8> {
        let x = self.soap(RC, "GetVolume", "<Channel>Master</Channel>")?;
        field(&x, "CurrentVolume")
            .trim()
            .parse()
            .map_err(|_| Error::parse("CurrentVolume"))
    }

    /// `SetVolume` (Master), clamped to 0-100.
    pub fn set_volume(&self, v: u8) -> Result<()> {
        self.soap(
            RC,
            "SetVolume",
            &format!("<Channel>Master</Channel><DesiredVolume>{}</DesiredVolume>", v.min(100)),
        )
        .map(|_| ())
    }

    /// `GetMute` (Master).
    pub fn mute(&self) -> Result<bool> {
        let x = self.soap(RC, "GetMute", "<Channel>Master</Channel>")?;
        Ok(matches!(field(&x, "CurrentMute").trim(), "1" | "true"))
    }

    /// `SetMute` (Master).
    pub fn set_mute(&self, mute: bool) -> Result<()> {
        self.soap(
            RC,
            "SetMute",
            &format!(
                "<Channel>Master</Channel><DesiredMute>{}</DesiredMute>",
                if mute { 1 } else { 0 }
            ),
        )
        .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_matches_tv_cast() {
        let e = soap_envelope(AVT, "Play", "<Speed>1</Speed>");
        assert_eq!(
            e,
            "<?xml version=\"1.0\" encoding=\"utf-8\"?><s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
             s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\"><s:Body><u:Play xmlns:u=\"urn:schemas-upnp-org:service:AVTransport:1\">\
             <InstanceID>0</InstanceID><Speed>1</Speed></u:Play></s:Body></s:Envelope>"
        );
    }

    #[test]
    fn didl_and_double_escaping() {
        let uri = "http://10.0.0.5:4000/a%20b.mkv?x=1&y=2";
        let didl = didl_metadata(uri, "Tom & Jerry's <cut>", "video/x-mkv", None);
        assert!(didl.contains("<dc:title>Tom &amp; Jerry&#x27;s &lt;cut&gt;</dc:title>"));
        assert!(didl.contains("<upnp:class>object.item.videoItem</upnp:class>"));
        assert!(didl.contains(&format!("protocolInfo=\"http-get:*:video/x-mkv:{DLNA_FLAGS}\">")));
        assert!(didl.contains("x=1&amp;y=2</res>"));
        let args = set_uri_args(uri, &didl);
        assert!(args.starts_with("<CurrentURI>http://10.0.0.5:4000/a%20b.mkv?x=1&amp;y=2</CurrentURI>"));
        assert!(args.contains("&lt;DIDL-Lite"));
        assert!(args.contains("x=1&amp;amp;y=2"));
        assert_eq!(upnp_class("audio/x-flac"), "audioItem");
        assert_eq!(upnp_class("image/jpeg"), "imageItem.photo");
        assert!(didl_metadata(uri, "t", "video/mp4", Some("0:01:00.000")).contains("duration=\"0:01:00.000\""));
    }

    #[test]
    fn fault_and_fields() {
        let fault = "<s:Envelope><s:Body><s:Fault><detail><UPnPError><errorCode>705</errorCode>\
                     <errorDescription>Transport is locked</errorDescription></UPnPError></detail></s:Fault></s:Body></s:Envelope>";
        assert_eq!(parse_fault(fault).as_deref(), Some("Transport is locked"));
        assert_eq!(parse_fault("<x/>"), None);
        let soap = |d: &str| Error::Soap { action: "Pause".into(), detail: d.into() };
        assert!(is_transient_refusal(&soap("Transition not available")), "live 2025 TV, Pause after Seek");
        assert!(is_transient_refusal(&soap("Transport is locked")));
        assert!(!is_transient_refusal(&soap("Invalid InstanceID")));
        assert!(!is_transient_refusal(&Error::Http("timeout".into())));
        let r = "<CurrentTransportState>PLAYING</CurrentTransportState><TrackURI>http://a/b?c=1&amp;d=2</TrackURI>";
        assert_eq!(TransportState::parse(&field(r, "CurrentTransportState")), TransportState::Playing);
        assert_eq!(field(r, "TrackURI"), "http://a/b?c=1&d=2");
        assert_eq!(field(r, "Missing"), "");
    }

    #[test]
    fn transient_refusals_are_retried_briefly() {
        let total: Duration = (0..).map_while(settle_delay).sum();
        assert!(total <= Duration::from_secs(2), "a permanent 701 must fail fast ({total:?})");
        assert!(total >= Duration::from_millis(400), "a 701 right after a seek clears in ~0.2 s");
        assert_eq!(settle_delay(SETTLE_RETRIES), None);
    }

    #[test]
    fn refusals_are_reworded() {
        let soap = |a: &str, d: &str| Error::Soap { action: a.into(), detail: d.into() };
        let msg = |e: Error| friendly(e).to_string();
        assert!(msg(soap("Seek", "Illegal seek target")).contains("can't jump to that point"));
        assert!(msg(soap("Pause", "Transition not available")).contains("already paused"));
        assert!(msg(soap("Seek", "Transition not available")).contains("live stream"));
        assert!(msg(soap("Play", "Transport is locked")).contains("busy loading"));
        assert!(matches!(friendly(soap("Play", "Invalid InstanceID")), Error::Soap { .. }));
        assert!(matches!(friendly(Error::NotCasting), Error::NotCasting));
        // Reads cleanly after the CLI's "error: " (no "unsupported: " in between).
        assert_eq!(
            friendly(soap("Seek", "Illegal seek target")).to_string(),
            "the TV can't jump to that point (it is at or past the end)"
        );
    }

    #[test]
    fn hms() {
        assert_eq!(format_hms(Duration::from_secs(3723)), "1:02:03");
        assert_eq!(parse_hms("1:02:03"), Some(Duration::from_secs(3723)));
        assert_eq!(parse_hms("0:00:01.500"), Some(Duration::from_millis(1500)));
        assert_eq!(parse_hms("NOT_IMPLEMENTED"), None);
        assert_eq!(xml_unescape("&#39;a&#x27;&amp;b&unknown;"), "'a'&b&unknown;");
    }
}
