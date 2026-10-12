//! SSDP discovery (M-SEARCH) to find the TV again if its IP changes.

use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::error::Result;

/// Search target answered by Samsung TVs' remote-control receiver.
pub const ST_SAMSUNG_REMOTE: &str = "urn:samsung.com:device:RemoteControlReceiver:1";
/// Generic UPnP media renderer.
pub const ST_MEDIA_RENDERER: &str = "urn:schemas-upnp-org:device:MediaRenderer:1";

/// One SSDP response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveredDevice {
    /// Sender IP.
    pub ip: IpAddr,
    /// `LOCATION` header (device description URL).
    pub location: Option<String>,
    /// `SERVER` header.
    pub server: Option<String>,
    /// `ST` header.
    pub st: Option<String>,
    /// `USN` header.
    pub usn: Option<String>,
}

impl DiscoveredDevice {
    /// Looks like a Samsung TV.
    pub fn is_samsung_tv(&self) -> bool {
        let s = |o: &Option<String>| o.as_deref().unwrap_or("").to_ascii_lowercase();
        s(&self.st).contains("samsung.com") || s(&self.server).contains("samsung") || s(&self.usn).contains("samsung")
    }
}

/// The M-SEARCH request text for a search target. Pure.
pub fn msearch_request(st: &str, mx: u32) -> String {
    format!(
        "M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: \"ssdp:discover\"\r\nMX: {mx}\r\nST: {st}\r\n\r\n"
    )
}

/// Parse an SSDP response (headers are case-insensitive). Pure.
pub fn parse_ssdp_response(text: &str, ip: IpAddr) -> Option<DiscoveredDevice> {
    let mut lines = text.lines();
    let first = lines.next()?;
    if !first.starts_with("HTTP/1.1 200") && !first.starts_with("NOTIFY") {
        return None;
    }
    let mut h = BTreeMap::new();
    for l in lines {
        if let Some((k, v)) = l.split_once(':') {
            h.insert(k.trim().to_ascii_uppercase(), v.trim().to_string());
        }
    }
    Some(DiscoveredDevice {
        ip,
        location: h.remove("LOCATION"),
        server: h.remove("SERVER"),
        st: h.remove("ST").or_else(|| h.remove("NT")),
        usn: h.remove("USN"),
    })
}

/// Multicast M-SEARCH for Samsung remote receivers and media renderers; collects
/// answers for `timeout`. Blocking. De-duplicated by IP.
pub fn discover(timeout: Duration) -> Result<Vec<DiscoveredDevice>> {
    let sock = UdpSocket::bind("0.0.0.0:0")?;
    sock.set_read_timeout(Some(Duration::from_millis(200)))?;
    let _ = sock.set_multicast_ttl_v4(2);
    let target: SocketAddr = "239.255.255.250:1900".parse().map_err(crate::error::Error::parse)?;
    let mx = timeout.as_secs().clamp(1, 5) as u32;
    let mut sent = 0;
    let mut send_err = None;
    for st in [ST_SAMSUNG_REMOTE, ST_MEDIA_RENDERER] {
        match sock.send_to(msearch_request(st, mx).as_bytes(), target) {
            Ok(_) => sent += 1,
            Err(e) => send_err = Some(e),
        }
    }
    // Nothing went out (no network, multicast refused): say so instead of "found nothing".
    if sent == 0 {
        if let Some(e) = send_err {
            return Err(crate::error::Error::Io(e));
        }
    }
    let mut found: Vec<DiscoveredDevice> = Vec::new();
    let deadline = Instant::now() + timeout;
    let mut buf = [0u8; 4096];
    while Instant::now() < deadline {
        match sock.recv_from(&mut buf) {
            Ok((n, from)) => {
                if let Some(d) = parse_ssdp_response(&String::from_utf8_lossy(&buf[..n]), from.ip()) {
                    match found.iter_mut().find(|x| x.ip == d.ip) {
                        // Prefer the Samsung-specific answer.
                        Some(x) if d.is_samsung_tv() && !x.is_samsung_tv() => *x = d,
                        Some(_) => {}
                        None => found.push(d),
                    }
                }
            }
            Err(_) => continue,
        }
    }
    Ok(found)
}

/// First Samsung TV found within `timeout`, if any.
pub fn find_tv(timeout: Duration) -> Result<Option<DiscoveredDevice>> {
    Ok(discover(timeout)?.into_iter().find(DiscoveredDevice::is_samsung_tv))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_response() {
        let r = "HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age=1800\r\nLocation: http://192.0.2.9:9197/dmr\r\n\
                 SERVER: SHP, UPnP/1.0, Samsung UPnP SDK/1.0\r\nST: urn:samsung.com:device:RemoteControlReceiver:1\r\nUSN: uuid:x\r\n\r\n";
        let ip: IpAddr = "192.0.2.9".parse().unwrap();
        let d = parse_ssdp_response(r, ip).unwrap();
        assert_eq!(d.location.as_deref(), Some("http://192.0.2.9:9197/dmr"));
        assert!(d.is_samsung_tv());
        assert!(parse_ssdp_response("garbage", ip).is_none());
        assert!(msearch_request(ST_MEDIA_RENDERER, 2).contains("MX: 2\r\n"));
    }
}
