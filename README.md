# tv-remote

Remote control for Samsung Tizen TVs on Linux: a GTK4/libadwaita app (`tv-remote`) and a terminal UI + CLI (`tv-remote-tui`), built on a shared Rust core.

## Features

- **Remote**: every remote key, text input (when the TV shows its own keyboard), touchpad with Wayland pointer capture.
- **Apps**: list installed apps, launch them, open YouTube/Netflix via DIAL.
- **Cast** local files or URLs over DLNA, with a media browser that flags what the TV can play, a "TV-friendly" mode (ffmpeg: keep the video, convert DTS audio to AC3, drop extra tracks, optional VAAPI re-encode), and a **network check** that compares the file's peak bitrate with the measured TV link before casting.
- **Power** on (Wake-on-LAN) / off, volume, mute.
- **SmartThings** (optional, OAuth login refreshed automatically; personal access token as a fallback): status, input source, picture/sound mode, cloud fallback for power and volume.
- **Several TVs**: LAN (SSDP + REST) and SmartThings discovery, matched into one list; per-TV pairing tokens; `--tv` on the CLI, a switcher in the GUI and TUI.

## Install

Requires Rust, GTK 4 and libadwaita, and ffmpeg/ffprobe for casting.

```sh
./install.sh   # builds in release mode, installs to ~/.local/bin and adds the "TV Remote" launcher
```

## Usage

```sh
tv-remote                     # GUI
tv-remote-tui                 # terminal UI
tv-remote-tui tvs scan        # find TVs on the LAN and in SmartThings
tv-remote-tui key volup
tv-remote-tui cast movie.mkv --audio 8
tv-remote-tui netcheck movie.mkv
tv-remote-tui --help
```

The first connection to a TV shows an approval prompt on the TV; accept it once and the token is saved in `~/.config/samsung-tv/config.json` (mode 0600).

`./install.sh --uninstall` removes the binaries, the launcher entry and the refresh timer (the config stays).

## SmartThings (OAuth)

SmartThings personal access tokens now expire after 24 h, so TV Remote logs in with your own OAuth app instead:

1. Register the app once with the SmartThings CLI (edit `appName` in `docs/smartthings-app.json` first; it must be unique):
   ```sh
   npx @smartthings/cli apps:create -i docs/smartthings-app.json -o ~/.config/samsung-tv/st-app-created.json -j
   ```
2. Import it and log in: **Device page → SmartThings → Import app file… → Connect SmartThings**, or
   ```sh
   tv-remote-tui smartthings app-import       # default: ~/.config/samsung-tv/st-app-created.json
   tv-remote-tui smartthings login            # browser login, then paste the httpbin.org address it lands on
   tv-remote-tui smartthings auth-status
   ```

Access tokens last 24 h and refresh tokens about 30 days; each refresh token works once. They refresh automatically when used,
the GUI refreshes them while it runs, and `install.sh` adds a daily systemd user timer (`tv-remote-st-refresh.timer`, running
`tv-remote-tui smartthings refresh --quiet`). If the login expires anyway, connect again. `smartthings logout` forgets the
tokens; a personal access token (Device page → *Personal access token (legacy)*) is still used when OAuth is not connected.

## Android

[`android/`](android/README.md) is an Android remote (Jetpack Compose, package `fr.ryoku.tvremote`) on the same core,
through UniFFI bindings. Milestone 1, "remote first", is done: discovery and pairing, every key, text, touchpad, power
on/off (Wake-on-LAN), volume and mute, the app list with launching, YouTube/Netflix, "Share → Play on TV" for YouTube
links, several TVs. Not yet: casting, SmartThings. Tested on a Pixel 8a (Android 17) against a 2025 Q7F.

It builds with a toolchain kept under `~/Android` (JDK, Android SDK and NDK, a separate rustup with the Android targets and
cargo-ndk); `~/Android/env.sh` sets the environment. From fish:

```sh
cd android
bash -c 'source ~/Android/env.sh && ./gradlew assembleDebug'     # Rust .so files + Kotlin bindings + APK
adb install -r app/build/outputs/apk/debug/app-debug.apk          # or copy the APK to the phone and open it
```

On Android 17, allow "devices on your local network" when asked. Details, tests and notes: [`android/README.md`](android/README.md).
The MIT license of this repository covers it.

## Documentation

The user guide is in [`docs/`](docs/README.md):

- [Installation](docs/installation.md): requirements per distro, install, update, uninstall
- [First connection](docs/first-connection.md): find the TV, pair it, TV settings to change once
- [Using the app](docs/using-the-app.md): the desktop app page by page, and the terminal UI keys
- [Casting](docs/casting.md): direct and TV-friendly modes, supported formats, the network check
- [SmartThings](docs/smartthings.md): optional cloud features and their one-time setup
- [Several TVs](docs/multiple-tvs.md): adding, switching, LAN and cloud fallback
- [Command-line reference](docs/cli-reference.md), [Troubleshooting](docs/troubleshooting.md), [FAQ](docs/faq.md)
- [How it works](docs/how-it-works.md): architecture, protocols and ports, config file, tests

## Layout

- `core/` — `tvremote_core`: WebSocket remote, REST, DLNA, DIAL, Wake-on-LAN, casting, network check, SmartThings, multi-TV fleet. See `core/API.md`.
- `gui/` — GTK4 / libadwaita app.
- `tui/` — ratatui terminal UI and clap CLI.
- `android/` — Android app: `rust/` (UniFFI bindings crate, outside the Cargo workspace) and the Gradle project.

Not affiliated with or endorsed by Samsung.
