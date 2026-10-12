//! Wake-on-LAN: magic packet to 255.255.255.255:9 and to each local subnet broadcast.

use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
use std::process::Command;
use std::sync::RwLock;

use crate::error::{Error, Result};

/// Parse `aa:bb:cc:dd:ee:ff` / `aa-bb-…` / `aabbccddeeff`.
pub fn parse_mac(s: &str) -> Result<[u8; 6]> {
    let hex: String = s.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    let seps_ok = s.chars().all(|c| c.is_ascii_hexdigit() || matches!(c, ':' | '-' | '.'));
    if hex.len() != 12 || !seps_ok {
        return Err(Error::parse(format!("invalid MAC address: {s}")));
    }
    let mut mac = [0u8; 6];
    for (i, b) in mac.iter_mut().enumerate() {
        *b = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).map_err(Error::parse)?;
    }
    Ok(mac)
}

/// The 102-byte magic packet: 6 × 0xFF then the MAC 16 times. Pure.
pub fn magic_packet(mac: [u8; 6]) -> [u8; 102] {
    let mut p = [0xFFu8; 102];
    for i in 0..16 {
        p[6 + i * 6..12 + i * 6].copy_from_slice(&mac);
    }
    p
}

/// Parse `ip -o -4 addr show` output into broadcast addresses (`brd` fields). Pure.
pub fn parse_broadcasts(ip_output: &str) -> Vec<Ipv4Addr> {
    let mut out = Vec::new();
    for line in ip_output.lines() {
        let toks: Vec<&str> = line.split_whitespace().collect();
        if let Some(i) = toks.iter().position(|t| *t == "brd") {
            if let Some(Ok(a)) = toks.get(i + 1).map(|t| t.parse::<Ipv4Addr>()) {
                if !out.contains(&a) {
                    out.push(a);
                }
            }
        }
    }
    out
}

/// Subnet broadcast addresses of this machine (from `ip -o -4 addr show`; local only).
pub fn local_broadcasts() -> Vec<Ipv4Addr> {
    Command::new("ip")
        .args(["-o", "-4", "addr", "show"])
        .output()
        .map(|o| parse_broadcasts(&String::from_utf8_lossy(&o.stdout)))
        .unwrap_or_default()
}

/// Set by [`set_broadcast_hints`].
static HINTS: RwLock<Vec<Ipv4Addr>> = RwLock::new(Vec::new());

/// Subnet broadcast addresses to add to every magic packet from now on (replaces the
/// previous hints). For platforms where `ip` cannot list them: the Android app passes
/// its Wi-Fi network's broadcast address.
pub fn set_broadcast_hints(addrs: Vec<Ipv4Addr>) {
    if let Ok(mut g) = HINTS.write() {
        *g = addrs;
    }
}

/// 255.255.255.255 then each address of the lists, without duplicates. Pure.
pub fn wol_targets(lists: &[&[Ipv4Addr]]) -> Vec<Ipv4Addr> {
    let mut out = vec![Ipv4Addr::BROADCAST];
    for a in lists.iter().flat_map(|l| l.iter()) {
        if !out.contains(a) {
            out.push(*a);
        }
    }
    out
}

/// Send the magic packet for `mac` to 255.255.255.255:9 and every subnet broadcast
/// (from `ip`, the [`set_broadcast_hints`], and `extra`). Succeeds if at least one send
/// worked.
pub fn send_magic_packet(mac: &str, extra: &[Ipv4Addr]) -> Result<()> {
    let pkt = magic_packet(parse_mac(mac)?);
    let sock = UdpSocket::bind("0.0.0.0:0")?;
    sock.set_broadcast(true)?;
    let hints = HINTS.read().map(|g| g.clone()).unwrap_or_default();
    let targets = wol_targets(&[&local_broadcasts(), &hints, extra]);
    let mut ok = false;
    let mut last_err = None;
    for t in targets {
        match sock.send_to(&pkt, SocketAddrV4::new(t, 9)) {
            Ok(_) => ok = true,
            Err(e) => last_err = Some(e),
        }
    }
    if ok {
        Ok(())
    } else {
        Err(last_err.map(Error::Io).unwrap_or_else(|| Error::Io(std::io::Error::other("no target"))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packet() {
        let mac = parse_mac("00:00:5E:00:53:01").unwrap();
        assert_eq!(mac, [0x00, 0x00, 0x5E, 0x00, 0x53, 0x01]);
        let p = magic_packet(mac);
        assert_eq!(&p[..6], &[0xFF; 6]);
        assert_eq!(&p[6..12], &mac);
        assert_eq!(&p[96..102], &mac);
        assert!(parse_mac("00:00:5E:00:53").is_err());
        assert_eq!(parse_mac("00005e005301").unwrap(), mac);
    }

    #[test]
    fn broadcasts() {
        let out = "1: lo    inet 127.0.0.1/8 scope host lo\n3: wlan0    inet 192.168.0.42/24 brd 192.168.0.255 scope global dynamic wlan0\n";
        assert_eq!(parse_broadcasts(out), vec![Ipv4Addr::new(192, 168, 0, 255)]);
    }

    #[test]
    fn targets() {
        let a = Ipv4Addr::new(192, 168, 0, 255);
        let b = Ipv4Addr::new(10, 0, 0, 255);
        assert_eq!(wol_targets(&[&[a], &[a, Ipv4Addr::BROADCAST], &[b]]), vec![Ipv4Addr::BROADCAST, a, b]);
        assert_eq!(wol_targets(&[]), vec![Ipv4Addr::BROADCAST]);
    }
}
