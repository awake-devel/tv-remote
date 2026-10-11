# tvremote_core: API guide for the GUI (B) and TUI/CLI (C)

`cargo doc -p tvremote_core --open` has the full reference. Every public item has a doc comment.
This page covers what you need to start.

## Rules

- **Every call blocks** (network, ffmpeg/ffprobe). Never call the library from the GTK main loop or
  the ratatui draw loop. Run calls on a worker thread (`std::thread::spawn`, or
  `gio::spawn_blocking`) and send results back over a channel (`async-channel` + `glib::spawn_future_local` in the GUI).
- `Tv` is `Clone + Send + Sync`, and all clones share state. Create it once and clone it into each worker.
- Errors come back as `tvremote_core::Error` (`thiserror`; its `Display` text is short enough for a toast).
  The library never panics on network or parse errors.
- Never print `config.token`, `config.smartthings_token` or the secrets of `smartthings_oauth` (client secret, access and
  refresh tokens). `Config`'s, `TvEntry`'s, `AppConfig`'s and `StOAuth`'s `Debug` output redacts them.

## Several TVs: `tvremote_core::Fleet`

The config file holds a list of TVs plus account settings (see `config.rs` for the format and
the migration rule). A `Fleet` owns it and hands out one `Tv` per entry.

```rust
use tvremote_core::{Fleet, TvEvent, Event};
let fleet = Fleet::open_default()?;            // same file rules as Tv::open_default (missing = empty; broken = Err, never overwritten)
let tv = fleet.active();                       // the active TV (first run: a placeholder; setting its host creates `tv1`)
let tv2 = fleet.tv("tv2")?;                    // lazily created, cached: clones share the remote channel
let tv3 = fleet.tv_by("chambre")?;             // id, display name (any case), host, or unique prefix
let rx = fleet.subscribe();                    // Receiver<TvEvent { tv: id, event }> for every TV; "" = fleet-wide
fleet.start_status_polling(5s, 60s);           // REST power of every TV (Event::Power) + SmartThings health (Event::Cloud). No WebSocket.
```

| Area | Methods |
|---|---|
| List | `entries() -> Vec<TvEntry>`, `ids()`, `active_id()`, `config() -> AppConfig`, `resolve(sel) -> id` |
| Manage | `set_active(id)`, `rename(id, name)`, `remove(id)` (stops its channel, deletes its token), `add_candidate(&Candidate) -> id`, `add_host(host, name)` |
| SmartThings link | `link_smartthings(id, device_id)` (manual; moves the device off any other TV), `unlink_smartthings(id)` (remembered in `smartthings_unlinked`: never auto-linked again), `set_smartthings_token(opt)` (legacy PAT), `has_smartthings()` (OAuth connected or PAT), `smartthings()` (OAuth client if connected, else PAT) |
| SmartThings login | `smartthings_auth() -> StAuth` (import, login, refresh, logout, status; see below), `smartthings_auth_changed()` (call after login/logout/import: drops cloud caches, emits `ConfigChanged` per TV + `FleetChanged`) |
| Detect | `scan(timeout) -> ScanReport` (LAN + cloud in parallel; applies host changes / last-seen / auto-links; cached for `Fleet::last_scan()`), `Fleet::scan_lan(timeout) -> Vec<LanTv>`, `scan_cloud() -> Vec<CloudTv>`, `Fleet::probe_host(ip)` |
| Status | `health_all() -> Vec<TvHealth>`; per TV `tv.health()` (REST + SmartThings health; power from SmartThings `switch` when the LAN is silent) |
| First run | `is_placeholder(&tv)`: switch to `fleet.tv(active)` once a TV exists |

**Detection and matching** (`fleet::merge_candidates`, pure, fixture-tested). `ScanReport.candidates` is one
list of `Candidate { key ("lan:<ip>" / "st:<deviceId>"), name, model, lan: Option<LanTv>, cloud: Option<CloudTv>,
presence: LanOnly | CloudOnly | Both, matched_by: Option<MatchKey>, known: Option<(id, KnownBy)> }`.
LAN = SSDP answers enriched with REST `/api/v2/` (`name`, `modelName`, `wifiMac`, `ip`, `duid`, `PowerState`).
Cloud = SmartThings devices where `is_tv()`, with `ocf` (`name`, `modelNumber`, `firmwareVersion`, `vendorId`;
completed from the `ocf` capability of the status: `n`, `mnmo`, `mnfv`, `vid`, `di`) and `/devices/{id}/health`.
Pairing, strongest first (`MatchKey`): `Linked` (already in the config) → `Mac` (only if SmartThings exposes one;
two different MACs veto the pair) → `Uuid` (OCF `di` = REST `duid`) → `ModelAndName` (`ocf.modelNumber` = REST
`modelName`, and `ocf.name` = REST `name`, normalised) → `ModelOnly` (only when that model is unique on both sides).
A known TV is recognised by MAC, else host (`KnownBy`); one recognised **by MAC** at a new IP gets its host updated
(`ScanReport.host_changes`).

**Fallback** (`fleet::plan_routes(op, lan: Reach, cloud: Option<Reach>) -> Vec<Route>`, pure). `Tv::volume`,
`set_volume`, `mute`, `set_mute`, `power_off` and `power_on` use it when a SmartThings device is linked: LAN first
(cloud as fallback) unless the last REST request (< 30 s) failed, then cloud first; a cloud known offline (< 2 min)
is skipped. `power_on` also sends SmartThings `switch.on` (at once if the LAN is silent, else after 4 s) and accepts
SmartThings reporting `switch: on` as success. `tv.lan_reach()`, `tv.cloud_reach()`, `tv.cloud_state()`
(health request, emits `Event::Cloud` on change), `tv.cached_cloud_state()`.

**Events.** Each `Tv` from a fleet has a tagged bus: `tv.subscribe()` still gives that TV's `Event`s only, and
every event is also sent to `fleet.subscribe()` as `TvEvent { tv, event }`. New variants: `Event::Cloud(CloudState)`
and `Event::FleetChanged` (list, names, links, or active TV changed).

**Pairing.** Each TV has its own token in its entry. A new TV has none: its first remote command opens the channel
without a token, the TV shows its approval prompt, and the token from `ms.channel.connect` is saved to that entry.

## Facade: `tvremote_core::Tv`

`Tv` is one TV. `Tv::open_default()` / `open(path)` still work: they bind to the TV that is active when the file is
opened. `tv.id()`, `tv.display_name()`, `tv.ptr_eq(&other)`. `tv.config()` is that TV's `Config` view (its entry plus
the account `name` and `smartthings_token`); `tv.update_config(|c| …)` writes each field back to the right place
(`c.display_name` renames).


```rust
use tvremote_core::{Tv, Key, Event, ConnectionState, PowerState};
let tv = Tv::open_default()?;          // ~/.config/samsung-tv/config.json; saves back (new token, settings)
// Missing file → Config::default() (empty host) bound to that path; the file is created on the first save.
// A file that exists but cannot be read/parsed → Err(Error::Config); it is never overwritten
// (fall back to Tv::new(Config::default()), in memory, if you want to carry on). First run: tv.host().is_empty().
// Tv::open(path) stays strict (missing file = error); Tv::open_or_default(path) is the lenient variant.
let events = tv.subscribe();           // std::sync::mpsc::Receiver<Event>; one per subscriber
tv.connect();                          // non-blocking: starts the remote WebSocket thread
tv.start_status_polling(Duration::from_secs(5)); // emits Event::Power on changes
```

| Area | Methods |
|---|---|
| Config | `open_default()`, `open_or_default(path)`, `open(path)`, `new(cfg)`; `config()`, `update_config(|c| ...)` (saves, emits `ConfigChanged`, reconnects if host/token changed), `host()` |
| Remote (WebSocket) | `connect()`, `disconnect()`, `connection_state()`, `send_key(&Key)`, `send_key_action(&Key, KeyAction)`, `hold_key(&Key, dur)`, `send_text(&str)`, `mouse_move(dx, dy)`, `mouse_click()`, `installed_apps() -> Vec<App>`, `launch_app(&App)`, `launch_app_id(id, LaunchType, meta)` |
| REST | `device_info() -> DeviceInfo`, `power_state() -> PowerState` (never errors; unreachable = `Off`), `cached_device_info()` / `tv_network() -> Option<TvNetwork>` (no I/O; filled by `device_info`, `power_state` and the status poller), `app_status(id)`, `close_app(id)` |
| Power | `power_on(timeout)` (WoL every 2 s, plus `KEY_POWER` if in network standby; polls until on), `power_off()` |
| Volume (DLNA) | `volume()`, `set_volume(u8)`, `mute()`, `set_mute(bool)` |
| DIAL | `youtube(id_or_url) -> YouTubeOpened` (`Video`, or `AppOnly` when the TV refuses the link and the app was launched over REST instead), `netflix()`; `dial().launch(app, body)` |
| Cast | `probe_media(path)`, `cast(CastRequest) -> CastSession`, `current_cast()`, `transport()` |
| Network check | `analyze_demand(&info, on_progress, on_done) -> DemandJob`, `network_verdict(&info, &mode, packets, tv.tv_network()) -> (Demand, Verdict)`, `measure_network(&cancel, &progress)` |
| SmartThings | `has_smartthings()`, `smartthings() -> SmartThings`, `smartthings_tv() -> StDevice` (linked device; else matched against this TV, or the single TV of a single-TV setup; saved as the link), `linked_smartthings_device()` (never auto-links), `smartthings_status()` |
| Status | `health() -> TvHealth { lan, cloud, power }`, `lan_reach()`, `cloud_reach()`, `cloud_state()` |
| Discovery | `rediscover(timeout) -> Option<ip>` (SSDP + REST; the TV whose `wifiMac` matches its MAC, else the only TV; updates the host) |

Lower-level clients are also public if you need them: `tv.rest()`, `tv.dlna()`, `tv.dial()`, and the
modules `remote`, `rest`, `dlna`, `server`, `cast`/`media`, `netcheck`, `dial`, `wol`, `smartthings`, `discovery`.

## Threading model and events

- **Remote channel:** one background thread owns the `wss://host:8002` socket. It accepts invalid TLS
  certificates and hostnames. Commands go in over a channel and each returns a `Result`. When the link
  drops it reconnects with back-off (1 s → 30 s), and it makes an immediate attempt when a command arrives
  while disconnected. On `ms.channel.connect` it saves any new token to the config and emits
  `Event::TokenUpdated`. With no token, the TV shows an approval prompt. The thread waits up to 30 s for it;
  during that time commands block and can time out after 12 s.
- **Event** (`Clone`), from `tv.subscribe()`:
  - `Connection(ConnectionState::{Disconnected, Connecting, Connected, Retrying(reason), Unauthorized})`
  - `Power(PowerState::{On, Standby, Off, Unknown})`
  - `TokenUpdated`, `ConfigChanged`
  - `RemoteEvent { name, data }`: unsolicited TV events, for example `ms.remote.imeStart` when a text field gets focus. Useful for popping up the keyboard entry.
  - `Cast(CastEvent::{Loading, State(TransportState), Position{..}, Throughput(sample), Stall{message, suggestion}, Ended, NeverStarted, Stopped, Error})`
- GUI: run a thread that does `for ev in rx { sender.send_blocking(ev) }` into an `async_channel`, and
  consume it with `glib::spawn_future_local`. TUI: `rx.try_recv()` on each tick.

## Typical sequences

**Connect and send a key**
```rust
tv.connect();                                   // optional; send_key connects on demand
tv.send_key(&Key::VolumeUp)?;                   // or "…".parse::<Key>()?, see below; Key::Raw("KEY_X".into())
tv.send_text("interstellar")?;                  // into the focused TV text field
```
`Key::ALL` lists the named keys; `key.code()` is the `KEY_*` code, `key.name()` the variant name
(`"VolumeUp"`), `key.label()` the button text (`"Vol +"`).
`Key::from_str` (`Err = Error::Parse`) accepts, case-insensitively and in this order: the code (`KEY_VOLUP`),
the bare code (`volup`), the variant name (`VolumeUp`, `volume_up`; `_ - space` ignored), the label
(`Vol +`, `ok`, `back`; ASCII `-` matches `−`). Unknown names starting with `KEY_` become `Key::Raw` (uppercased);
any other unknown name is an error.

**Apps**
```rust
let apps = tv.installed_apps()?;                // Vec<App { app_id, name, app_type, icon }>
tv.launch_app(&apps[0])?;
tv.youtube("https://youtu.be/dQw4w9WgXcQ")?;    // DIAL; falls back to launching the app
```

**Browse → probe → profile → netcheck → cast → monitor**
```rust
use tvremote_core::cast::*;                       // MediaInfo, check_compat, CastProfile, ...
let info = tv.probe_media(&path)?;                // ffprobe (< 1 s); cache it in the browser
let report = check_compat(&info);                 // report.badge() ✅/⚠/❌; report.issues (⚠ also = untested format)
let start_friendly = report.needs_tv_friendly();  // default mode: true only if a direct cast would not play
let vaapi = vaapi_available();                    // slow first call (~1 s), cached; worker thread!
let level = report.effective_level(vaapi);        // re-encode needed + no VAAPI → Unsupported

let mut profile = CastProfile::recommended(&info, vaapi); // never picks Reduce when vaapi is false
// selectors: info.audio_streams() / info.subtitle_streams() (StreamInfo::label(), is_dts(&codec), is_text_subtitle())
profile.audio = Some(8);                          // absolute stream index
profile.audio_handling = AudioHandling::Auto;     // DTS/TrueHD → AC3 640k, others copied
profile.subtitle = None;                          // text subs only, kept as SRT
profile.video = VideoMode::Copy;                  // or Reduce { target_mbps: 30.0 } (only if vaapi)
let est = estimated_bitrate(&info, &profile);     // live "resulting bitrate" (est.total_mbps())
let mode = CastMode::TvFriendly(profile);         // or CastMode::Direct (= direct_bitrate(&info))

// Network check: an instant provisional verdict, then the accurate one when packet analysis finishes.
// tv_network(): how the TV is connected (TvNetwork::Wireless/Wired) from cached device info; None = unknown.
let (_demand, verdict) = tv.network_verdict(&info, &mode, None, tv.tv_network());
let job = tv.analyze_demand(&info, |p| {/* progress 0..1 */}, move |res| {/* Ok(Arc<PacketProfile>) */});
//   on done: tv.network_verdict(&info, &mode, Some(&profile_arc), tv.tv_network()) (instant; re-run when the profile changes)
// Pure form: netcheck::verdict(&demand, cap, link, tv: Option<TvNetwork>); DeviceInfo::tv_network() parses networkType.
// verdict.level (Ok/Risky/Bad, .badge()), verdict.explanation, verdict.fixes: Vec<Fix { description,
//   resulting_mbps, resulting_level, requires_vaapi, kind }>. FixKind::DropExtraTracks{profile} /
//   ReduceVideo{target_mbps, profile} carry a ready CastProfile you can apply. Hide requires_vaapi fixes if !vaapi.
// Bad verdict → require an explicit "Cast anyway".
// job.cancel() if the user picks another file.

let session = tv.cast(CastRequest { source: CastSource::File(path), mode, title: None, media: Some(info) })?;
// or CastRequest::file(path) / CastRequest::url(url)
session.status();          // CastStatus { state, position, duration, seekable, tv_friendly, started, outcome, last_mbps, .. }
session.throughput();      // Vec<ThroughputSample { t_secs, bytes_per_sec }> for the graph (.mbps())
session.pause()?; session.resume()?; session.seek(dur)?;   // seek → Error::Unsupported in TV-friendly mode
session.stop()?;
session.wait(None);        // CLI: block until Ended / NeverStarted / Stopped / Lost
```
The session keeps running until playback ends or `stop()` is called, even if you drop the handle.
`tv.current_cast()` returns it again later. Playback counts as `Ended` only after `PLAYING` has been seen;
if the TV never reaches `PLAYING` within 30 s you get `NeverStarted` (the format is probably unsupported).
`CastEvent::Stall` is the live guard: it fires when the position freezes for 4 s, or when the TV goes back
to `TRANSITIONING` mid-play. Show it as a toast with its `suggestion`. TV-friendly ffmpeg stderr goes to `cast::ffmpeg_log_path()`.

**Network test**
```rust
let cancel = Arc::new(AtomicBool::new(false));     // keep a clone in the UI; cancel.store(true, SeqCst) aborts
let m = tv.measure_network(&cancel, &|p| match p { CapacityProgress::GeneratingClip(f) => .., CapacityProgress::Measuring{elapsed, mbps} => .., _ => .. })?;
// Cancelled → Err(Error::Cancelled) within ~0.5 s: ffmpeg killed (no partial clip), test cast stopped, nothing saved.
// (netcheck::measure_capacity(&dlna, &cancel, &progress) / ensure_probe_clip(&cancel, &progress) take the same flag.)
// The first run encodes a 20 s 150 Mbps noise clip (~375 MB in ~/.cache/tv-remote), which takes a minute or so.
// A noise test pattern shows on the TV for ~10 s.
netcheck::load_capacity();                         // cached CapacityMeasurement { mbps, measured_at, .. }
cap.is_stale(now_unix, netcheck::detect_link().as_ref()); // > 24 h or another Wi-Fi link
```

**Power**
```rust
tv.power_off()?;                                   // KEY_POWER when on
tv.power_on(Duration::from_secs(20))?;             // WoL (config.mac); may need "Power On with Mobile" on the TV
```

**Several TVs** (CLI: `--tv`, `tvs list|scan|add|rename|remove|use|link|unlink`)
```rust
let r = fleet.scan(Duration::from_secs(3));        // ~3 s; LAN + SmartThings
for c in &r.candidates { /* c.presence.label(), c.name, c.ip(), c.matched_by, c.known */ }
let id = fleet.add_candidate(&r.candidates[0])?;   // no I/O; first TV becomes active
fleet.set_active(&id)?;
let tv = fleet.tv(&id)?; tv.send_key(&Key::Home)?; // new TV: approval prompt on its screen, token saved to its entry
fleet.link_smartthings(&id, "device-uuid")?;       // or let scan / smartthings_tv() match it
tv.set_volume(12)?;                                // DLNA, or SmartThings when the LAN does not answer
```

**SmartThings login (OAuth)** (`tvremote_core::smartthings_auth`; account-level, one login for all TVs)

Personal access tokens created after 30 Dec 2024 expire after 24 h, so the long-lived path is an OAuth app the user
registers once with the SmartThings CLI (`npx @smartthings/cli apps:create -i docs/smartthings-app.json -o
~/.config/samsung-tv/st-app-created.json -j`; redirect URIs `http://localhost:47811/callback` and
`https://httpbin.org/get`, scopes `r:devices:* w:devices:* x:devices:* r:locations:*`). Config key `smartthings_oauth`
(`StOAuth { client_id, client_secret, app_id, redirect_uri, access_token, access_expires_at, refresh_token,
refreshed_at, scope }`, unix seconds). When connected it wins; otherwise `smartthings_token` (PAT) is used.
```rust
use tvremote_core::smartthings_auth::*;
let auth = fleet.smartthings_auth();                       // StAuth: Clone + Send + Sync, no I/O
auth.import_app(&default_app_file())?;                     // ~/.config/samsung-tv/st-app-created.json → client id/secret/app id
// Local flow (worker thread): one-shot listener on 127.0.0.1:47811 (GET /callback only, state checked, 5 min).
let state = random_state();
match CallbackListener::bind() {                           // Err (AddrInUse) → paste flow below
    Ok(l) => {
        let url = auth.authorize_url(&state, &l.redirect_uri())?;   // = LOCAL_REDIRECT
        if !open_in_browser(&url) { /* show the URL */ }
        auth.login_with_listener(&l, &state, LOGIN_TIMEOUT, &cancel)?;  // cancel: &AtomicBool → Error::Cancelled
    }
    Err(_) => {
        let url = auth.authorize_url(&state, PASTE_REDIRECT)?;           // https://httpbin.org/get
        // user pastes the redirected URL, httpbin's JSON, or just the code:
        auth.login_with_input(&pasted, Some(&state), PASTE_REDIRECT)?;
    }
}
fleet.smartthings_auth_changed();
let tok = auth.bearer()?;          // valid access token; refreshes first if it expires within 10 min
auth.refresh()?;                   // spend the refresh token now (daily timer: `tv-remote-tui smartthings refresh --quiet`)
auth.logout()?;                    // forget tokens, keep the app credentials
let s: AuthStatus = auth.status(); // method (OAuth/Pat/None), app_imported, connected, needs_login,
                                   // access_expires_in, refresh_age, pat_present, scope; s.summary() for a status row
```
- **Single-use refresh tokens.** Each refresh returns a new pair and invalidates the old refresh token *and* the current
  access token. Refreshes are serialized by a process-wide mutex plus an exclusive `flock` on `config.json.lock`
  (`SharedConfig::with_oauth_locked`); the config is re-read inside the lock (another process may have rotated the pair)
  and the new pair is saved before the access token is used. Every other config save also takes the lock and re-reads the
  whole file before applying its change (a read-modify-write), so a long-running GUI never writes back a stale pair, nor
  drops TVs or settings another process changed; a file that no longer parses is never overwritten (the save fails).
- Every SmartThings call in core goes through `bearer()`; on HTTP 401 the client calls `after_unauthorized(used)` (adopts a
  token another process stored, else refreshes) and retries once.
- Token endpoint: `POST https://api.smartthings.com/oauth/token`, form body, HTTP Basic `client_id:client_secret`. Pure,
  unit-tested builders: `build_authorize_url`, `exchange_request`, `refresh_request`, `parse_token_response`,
  `apply_token_response`, `needs_refresh`, `parse_app_file`, `code_from_input`, `parse_callback_request`.
- Errors: `Error::SmartThingsNotConnected`, `Error::SmartThingsLoginExpired` (`invalid_grant` on refresh: tokens are cleared,
  `refreshed_at` kept, `status().needs_login`; tell the user to log in again), `Error::Http` (network; nothing cleared),
  `Error::SmartThings` (anything else, e.g. `invalid_client` → re-import the app file). The CLI exits 4 on `SmartThingsLoginExpired`.

**SmartThings devices** (hide the page unless `tv.has_smartthings()`)
```rust
// Legacy PAT (used only when OAuth is not connected): tv.update_config(|c| c.smartthings_token = Some(pat))?;
let st = tv.smartthings()?; let dev = tv.smartthings_tv()?;   // finds the TV and saves its device id
dev.capabilities / dev.has("custom.picturemode")               // only offer what the device reports
let s = st.status(&dev.device_id)?;                            // TvStatus { picture_mode, picture_modes, input_source, input_sources, volume, .. }
st.set_picture_mode(&dev, "movie")?; st.set_input_source(&dev, "PlayStation")?;  // Error::Unsupported if the capability is missing
// Both take the id OR the display name: resolved against status.picture_modes / input_sources (id exact,
// then id any case, then name any case); picture mode is sent as the display name, input as the id.
// Unknown value → Error::Unsupported listing the supported ones; empty map → passed through.
// Each does one status GET; with a status in hand use set_picture_mode_from(&dev, &s.picture_modes, m) /
// set_input_source_from(&dev, &s.input_sources, src). Pure helpers: smartthings::{resolve_choice, picture_mode_arg, input_source_arg}.
```

## Thresholds (netcheck)
With a measured capacity: ✅ capacity ≥ 1.5 × the 10 s peak and ≥ the 2 s peak; ⚠ capacity ≥ the 10 s peak; ❌ below it.
Unmeasured, TV wireless or unknown (`tv` = `None`): ❌ above 40 Mbps, ⚠ above 20 Mbps. Unmeasured, TV wired:
❌ above 80 Mbps, ⚠ above 50 Mbps (many TVs have a 100 Mbit/s port). The wording only says the TV is on Wi-Fi
when it is known to be; unknown is hedged ("If the TV is on Wi-Fi too…"). The provisional peak is the average × 1.5;
reduced video counts at its target bitrate and AC3 at 640 kbps.
