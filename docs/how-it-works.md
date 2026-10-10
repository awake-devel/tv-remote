# How it works

This page is for contributors and the curious. It describes the code layout, the protocols, the config file, and how to run the tests.

## Architecture

The repository is a Cargo workspace with three crates:

| Crate | Path | What it is |
|---|---|---|
| `tvremote_core` | `core/` | library: every protocol, casting, the network check, SmartThings, the multi-TV fleet |
| `tv-remote-gui` | `gui/` | the `tv-remote` desktop app (GTK 4, libadwaita) |
| `tv-remote-tui` | `tui/` | the `tv-remote-tui` terminal UI (ratatui) and command line (clap) |

The GUI and the TUI contain no protocol code. They call the core and display its events.

Key ideas in the core:

- **Blocking calls, background threads.** Every core call blocks (network, ffmpeg). The GUI and TUI run them on worker threads and get results back over channels.
- **`Fleet` and `Tv`.** A `Fleet` owns the config file and hands out one `Tv` per configured TV. A `Tv` is cheap to clone, and clones share state.
- **Events.** A `Tv` publishes events (connection, power, cast progress, SmartThings state, TV events such as "keyboard opened"). The fleet forwards every TV's events to one bus.
- **Remote channel.** One background thread per TV owns the WebSocket. It reconnects with back-off (1 s up to 30 s) and saves a new pairing token when the TV sends one.
- **Pure decision functions.** Matching LAN and cloud TVs, choosing LAN or cloud routes, compatibility checks and network verdicts are pure functions tested with fixtures.

[`core/API.md`](../core/API.md) is the guide to the core API for the GUI and TUI. `cargo doc -p tvremote_core --open` has the full reference.

## Protocols and ports

| What | Protocol | Port | Direction |
|---|---|---|---|
| Device info, power state, app status | HTTP REST (`/api/v2/`) | TCP 8001 on the TV | PC → TV |
| Remote keys, text, pointer, app launch | WebSocket over TLS (self-signed certificate) | TCP 8002 on the TV | PC → TV |
| Casting control, volume, mute | DLNA / UPnP AVTransport and RenderingControl | TCP 9197 on the TV | PC → TV |
| Launch YouTube and Netflix | DIAL | TCP 8080 on the TV | PC → TV |
| Cast stream | HTTP (with range requests in direct mode) | random TCP port on the PC | TV → PC |
| Discovery | SSDP (multicast `239.255.255.250:1900`) | UDP 1900 | PC → network, answers back |
| Power on | Wake-on-LAN magic packet | UDP 9, broadcast | PC → network |
| SmartThings | HTTPS REST, OAuth 2 | TCP 443 on `api.smartthings.com` | PC → internet |
| SmartThings login | local redirect page | TCP 47811 on `localhost`, during `smartthings login` only | browser → PC |

Notes:

- **Pairing.** The first WebSocket connection without a token makes the TV show its approval prompt. When you accept, the TV sends a token in `ms.channel.connect`, and TV Remote saves it to that TV's entry. Later connections send the token and don't prompt.
- **App list.** Recent firmware ignores `ed.installedApp.get` on the WebSocket. The core asks REST about each id in `KNOWN_APPS` (`core/src/lib.rs`) in parallel instead, and only falls back to the WebSocket when none is found.
- **Wake-on-LAN** is sent to `255.255.255.255:9` and to each local subnet broadcast address, every 2 seconds until the TV answers or the timeout runs out. If the TV is in network standby, `KEY_POWER` is also sent.
- **Casting.** TV Remote starts a small HTTP server on a random port, then tells the TV's DLNA renderer to play its URL. In TV-friendly mode, ffmpeg remuxes (or, with VAAPI, re-encodes) the file into a live Matroska or MPEG-TS stream.

## Config file

Location: `~/.config/samsung-tv/config.json` (or `$XDG_CONFIG_HOME/samsung-tv/config.json`). It's written atomically, with mode 0600, because it holds tokens. If it exists but can't be parsed, TV Remote never overwrites it.

Format (version 2), with placeholder values:

```json
{
  "config_version": 2,
  "name": "My PC",
  "smartthings_oauth": {
    "client_id": "…", "client_secret": "…", "app_id": "…",
    "redirect_uri": "http://localhost:47811/callback",
    "access_token": "…", "access_expires_at": 1760086400,
    "refresh_token": "…", "refreshed_at": 1760000000,
    "scope": "r:devices:* w:devices:* x:devices:* r:locations:*"
  },
  "active": "tv1",
  "tvs": [
    {
      "id": "tv1",
      "name": "Living room",
      "host": "192.0.2.10",
      "port": 8002,
      "mac": "00:00:5E:00:53:01",
      "model": "QE65Q7FAAU",
      "token": "…",
      "smartthings_device_id": "00000000-0000-4000-8000-000000000001",
      "smartthings_unlinked": [],
      "last_seen": { "lan": 1760000000, "cloud": null, "ip": "192.0.2.10" }
    }
  ],
  "host": "192.0.2.10", "port": 8002, "token": "…", "model": "QE65Q7FAAU", "mac": "00:00:5E:00:53:01"
}
```

| Key | Meaning |
|---|---|
| `name` | client name shown on the TVs' approval prompt and device list |
| `smartthings_oauth` | the SmartThings app you registered and its tokens (optional) |
| `smartthings_token` | legacy SmartThings personal access token (optional; ignored when `smartthings_oauth` is set) |
| `active` | id of the active TV |
| `tvs` | one entry per TV, each with its own pairing `token` |
| `smartthings_unlinked` | SmartThings devices you unlinked from this TV; never linked automatically again |
| `last_seen` | when and where the TV was last seen, on the LAN and in the cloud |
| trailing `host`, `port`, `token`, … | a copy of the active TV, for older scripts that read the flat format; ignored on load |

A file in the old flat format (version 1, a single TV, no `tvs` key) is migrated on load: its TV becomes `tv1` and keeps its token, so the TV doesn't prompt again. Unknown keys are kept.

The SmartThings refresh token is single-use, and the GUI, the CLI and the daily timer share the file. So every save takes a lock on `config.json.lock` and re-reads the SmartThings section first. A process never writes back a token another process already replaced.

Other files:

| Path | What |
|---|---|
| `~/.config/samsung-tv/st-app-created.json` | output of `apps:create` (contains the client secret), read by `smartthings app-import` |
| `~/.cache/tv-remote/demand-*.json` | packet analysis of each file (keyed by path, size and modification time) |
| `~/.cache/tv-remote/capacity.json` | the last network test result |
| `~/.cache/tv-remote/last-scan.json` | the last `tvs scan`, used by `tvs add <n>` |
| `~/.cache/tv-remote/ffmpeg-last.log` | ffmpeg's log for the last TV-friendly cast |

## Privacy

- Pairing tokens, SmartThings credentials and tokens stay on your PC in `~/.config/samsung-tv/`, readable only by you.
- `Debug` output of the config types redacts tokens. Never log or print them in new code.
- Without SmartThings, nothing leaves your local network.
- With SmartThings, the core talks only to the official SmartThings API.

When you share logs or output in an issue, remove IP and MAC addresses, device ids and tokens.

## Building and testing

```sh
cargo build --workspace            # debug build of everything
cargo test --workspace             # all unit tests
cargo test -p tvremote_core        # core only
cargo clippy --workspace
```

The tests don't need a TV or a network. They use the JSON and SSDP samples in `core/fixtures/` and temporary config files. When you change behaviour that depends on a real TV, describe what you tested and on which model.

Run the programs from the build directory without installing:

```sh
cargo run -p tv-remote-tui -- --help
cargo run -p tv-remote-gui
```

Use `--config <path>` with a scratch config file to experiment without touching your real one.

## Documentation

- User documentation: this `docs/` folder. Use documentation-only example values: IP addresses from `192.0.2.0/24`, MAC addresses from `00:00:5E:00:53:xx`, and made-up UUIDs.
- Core API: [`core/API.md`](../core/API.md) and the doc comments.
