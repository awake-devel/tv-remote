//! Run the app's scan from the host with a throw-away config (read-only on the network:
//! SSDP + `GET /api/v2/`): `cargo run --example scan`.

fn main() {
    let dir = std::env::temp_dir().join(format!("tvremote-ffi-scan-{}", std::process::id()));
    let core = tvremote_ffi::RemoteCore::new(
        dir.join("config.json").to_string_lossy().into_owned(),
        dir.join("cache").to_string_lossy().into_owned(),
        "scan example".into(),
    )
    .expect("core");
    let t = std::time::Instant::now();
    let r = core.scan(4000);
    println!("{:?} in {:?}", r, t.elapsed());
    let _ = std::fs::remove_dir_all(dir);
}
