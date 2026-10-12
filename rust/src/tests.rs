//! Offline tests: no TV is contacted (the only address used is 127.0.0.1, where nothing
//! listens on the TV ports).

use std::sync::mpsc::{channel, Sender};

use super::*;

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let p = std::env::temp_dir().join(format!("tvremote-ffi-{tag}-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&p).unwrap();
        TempDir(p)
    }
    fn file(&self, name: &str) -> String {
        self.0.join(name).to_string_lossy().into_owned()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn core(dir: &TempDir) -> Arc<RemoteCore> {
    RemoteCore::new(dir.file("config.json"), dir.file("cache"), "TV Remote (Android)".into()).unwrap()
}

#[test]
fn every_core_key_is_exposed_in_order() {
    assert_eq!(RemoteKey::ALL.len(), Key::ALL.len());
    for (f, k) in RemoteKey::ALL.iter().zip(Key::ALL) {
        assert_eq!(&f.core(), k);
    }
    assert_eq!(key_code(RemoteKey::VolumeUp), "KEY_VOLUP");
    assert_eq!(key_code(RemoteKey::Blue), "KEY_CYAN");
    assert_eq!(key_label(RemoteKey::Enter), "OK");
}

#[test]
fn errors_map_to_kinds() {
    assert!(matches!(RemoteError::from(Error::Unauthorized), RemoteError::NotAuthorized(_)));
    assert!(matches!(RemoteError::from(Error::Timeout("x".into())), RemoteError::Timeout(_)));
    assert!(matches!(RemoteError::from(Error::NotConnected), RemoteError::Unreachable(_)));
    assert!(matches!(RemoteError::from(Error::Config("x".into())), RemoteError::Invalid(_)));
    let e = RemoteError::from(Error::Unauthorized);
    assert_eq!(e.to_string(), Error::Unauthorized.to_string());
}

#[test]
fn events_map() {
    let ime = |n: &str| Event::RemoteEvent { name: n.into(), data: serde_json::json!({}) };
    assert_eq!(event(&ime("ms.remote.imeStart")), Some(CoreEvent::KeyboardShown));
    assert_eq!(event(&ime("ms.remote.imeEnd")), Some(CoreEvent::KeyboardHidden));
    assert!(matches!(event(&ime("ms.remote.touchEnable")), Some(CoreEvent::Remote { .. })));
    assert_eq!(event(&Event::TokenUpdated), Some(CoreEvent::Paired));
    assert_eq!(event(&Event::Power(PowerState::Standby)), Some(CoreEvent::Power { power: TvPower::Standby }));
    assert_eq!(
        event(&Event::Connection(ConnectionState::Retrying("off".into()))),
        Some(CoreEvent::Connection { state: Connection::Retrying { reason: "off".into() } })
    );
    assert_eq!(event(&Event::FleetChanged), Some(CoreEvent::TvsChanged));
}

#[test]
fn tv_list_round_trip() {
    let dir = TempDir::new("list");
    let c = core(&dir);
    assert!(c.tvs().is_empty());
    assert_eq!(c.active_id(), None);

    // Nothing answers on 127.0.0.1: added by address, without MAC.
    let id = c.add_host("127.0.0.1".into(), Some("Salon".into())).unwrap();
    let tvs = c.tvs();
    assert_eq!(tvs.len(), 1);
    assert_eq!(tvs[0].id, id);
    assert_eq!(tvs[0].name, "Salon");
    assert_eq!(tvs[0].host, "127.0.0.1");
    assert!(tvs[0].active && !tvs[0].paired && tvs[0].mac.is_none());
    // Same host again: same TV.
    assert_eq!(c.add_host(" 127.0.0.1 ".into(), None).unwrap(), id);
    assert!(matches!(c.add_host("  ".into(), None), Err(RemoteError::Invalid(_))));

    c.rename(id.clone(), "Chambre".into()).unwrap();
    assert_eq!(c.tv(id.clone()).unwrap().name(), "Chambre");

    // Saved to the file, with the client name of a new config.
    let saved: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.file("config.json")).unwrap()).unwrap();
    assert_eq!(saved["name"], "TV Remote (Android)");
    drop(c);
    let again = core(&dir);
    assert_eq!(again.tvs().len(), 1);
    assert_eq!(again.tvs()[0].name, "Chambre");

    again.remove(id.clone()).unwrap();
    assert!(again.tvs().is_empty());
    assert!(again.tv(id).is_err());
}

#[test]
fn existing_config_keeps_its_client_name() {
    let dir = TempDir::new("name");
    std::fs::write(dir.file("config.json"), r#"{"name":"Ryoku","tvs":[]}"#).unwrap();
    let c = core(&dir);
    c.add_host("127.0.0.1".into(), None).unwrap();
    let saved: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.file("config.json")).unwrap()).unwrap();
    assert_eq!(saved["name"], "Ryoku");
}

#[test]
fn add_found_needs_a_scan_result() {
    let dir = TempDir::new("found");
    let c = core(&dir);
    assert!(matches!(c.add_found("lan:192.0.2.9".into()), Err(RemoteError::Invalid(_))));
}

#[test]
fn unreachable_tv_reports_off_and_errors() {
    let dir = TempDir::new("off");
    let c = core(&dir);
    let tv = c.tv(c.add_host("127.0.0.1".into(), None).unwrap()).unwrap();
    assert_eq!(tv.power_state(), TvPower::Off);
    assert_eq!(tv.connection(), Connection::Disconnected);
    assert!(matches!(tv.volume(), Err(RemoteError::Unreachable(_))));
    assert!(tv.device_info().is_err());
    // No MAC and not in standby: nothing can wake it.
    assert!(matches!(tv.power_on(1000), Err(RemoteError::Invalid(_))));
    assert!(matches!(tv.send_key_code("not a key".into()), Err(RemoteError::Invalid(_))));
}

struct Collect(Mutex<Sender<(String, CoreEvent)>>);

impl EventListener for Collect {
    fn on_event(&self, tv_id: String, event: CoreEvent) {
        let _ = self.0.lock().unwrap().send((tv_id, event));
    }
}

#[test]
fn listener_gets_list_changes_and_can_be_replaced() {
    let dir = TempDir::new("events");
    let c = core(&dir);
    let (tx, rx) = channel();
    c.set_listener(Box::new(Collect(Mutex::new(tx))));
    let id = c.add_host("127.0.0.1".into(), None).unwrap();
    let got = rx.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(got, (String::new(), CoreEvent::TvsChanged));

    c.clear_listener();
    std::thread::sleep(Duration::from_millis(400));
    while rx.try_recv().is_ok() {}
    c.rename(id, "Other".into()).unwrap();
    std::thread::sleep(Duration::from_millis(400));
    assert!(rx.try_recv().is_err());
}

#[test]
fn wol_broadcasts_accept_garbage() {
    set_wol_broadcasts(vec!["192.168.0.255".into(), "nope".into()]);
    set_wol_broadcasts(Vec::new());
    assert_eq!(tls_backend(), "rustls");
}
