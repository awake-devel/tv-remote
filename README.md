# tv-remote

Remote control for Samsung Tizen TVs on Linux: a GTK4/libadwaita app (`tv-remote`) and a terminal UI + CLI (`tv-remote-tui`), built on a shared Rust core.

## Features

- **Remote**: every remote key, text input (when the TV shows its own keyboard), touchpad with Wayland pointer capture.
- **Apps**: list installed apps, launch them, open YouTube/Netflix via DIAL.
- **Cast** local files or URLs over DLNA, with a media browser that flags what the TV can play, a "TV-friendly" mode (ffmpeg: keep the video, convert DTS audio to AC3, drop extra tracks, optional VAAPI re-encode), and a **network check** that compares the file's peak bitrate with the measured TV link before casting.
- **Power** on (Wake-on-LAN) / off, volume, mute.
- **SmartThings** (optional, personal access token): status, input source, picture/sound mode, cloud fallback for power and volume.
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

## Layout

- `core/` — `tvremote_core`: WebSocket remote, REST, DLNA, DIAL, Wake-on-LAN, casting, network check, SmartThings, multi-TV fleet. See `core/API.md`.
- `gui/` — GTK4 / libadwaita app.
- `tui/` — ratatui terminal UI and clap CLI.
