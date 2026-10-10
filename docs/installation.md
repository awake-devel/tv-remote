# Installation

TV Remote is built from source with Cargo and installed for your user only. You don't need root.

## Requirements

| What | Why | Minimum |
|---|---|---|
| Rust and Cargo | to build | a recent stable release (use [rustup](https://rustup.rs) if your distro's is too old) |
| GTK 4 | desktop app | 4.22 |
| libadwaita | desktop app | 1.8 |
| pkg-config | finds GTK and libadwaita while building | any |
| C compiler and linker | building some dependencies | any |
| OpenSSL (development files) | TLS to the TV and SmartThings | any |
| ffmpeg (with `ffprobe`) | casting: reading files, TV-friendly mode, network test | any recent |
| VAAPI driver | optional: "reduce bitrate" re-encoding while casting | — |
| Node.js (`npx`) | optional: one-time [SmartThings](smartthings.md) setup | — |

The desktop app needs recent GTK and libadwaita versions. Check what your system has:

```sh
pkg-config --modversion gtk4 libadwaita-1
```

If they are older than 4.22 and 1.8, you can still build and use the terminal UI and command line: see [Terminal UI only](#terminal-ui-only).

### Arch Linux

```sh
sudo pacman -S --needed base-devel rust openssl gtk4 libadwaita pkgconf ffmpeg
```

Optional VAAPI: `sudo pacman -S --needed libva-utils` plus your GPU's driver (`intel-media-driver` for Intel; AMD's driver ships with `mesa`).

### Debian and Ubuntu

```sh
sudo apt install build-essential cargo rustc libssl-dev libgtk-4-dev libadwaita-1-dev pkg-config ffmpeg
```

Optional VAAPI: `sudo apt install vainfo` plus `intel-media-va-driver` (Intel) or `mesa-va-drivers` (AMD).

Only recent releases ship GTK 4.22 and libadwaita 1.8. If `cargo` or `rustc` from apt is too old, remove them and install Rust with [rustup](https://rustup.rs).

### Fedora

```sh
sudo dnf install gcc cargo rust openssl-devel gtk4-devel libadwaita-devel pkgconf-pkg-config ffmpeg-free
```

`ffmpeg-free` lacks some codecs. For full codec support, install `ffmpeg` from RPM Fusion instead.

Optional VAAPI: `sudo dnf install libva-utils` plus `intel-media-driver` (Intel, RPM Fusion) or `mesa-va-drivers` (AMD).

### Checking VAAPI

VAAPI is only used to re-encode video to a lower bitrate while casting. Everything else works without it. To check:

```sh
vainfo | grep -i hevc
```

If you see an `HEVC` line with `VAEntrypointEncSlice`, re-encoding is available.

## Install

1. Get the source:

   ```sh
   git clone https://github.com/awake-devel/tv-remote.git
   cd tv-remote
   ```

2. Build and install:

   ```sh
   ./install.sh
   ```

   The first build takes a few minutes. Run it as your normal user, not with `sudo`.

3. Check that `~/.local/bin` is in your `PATH`. The script prints a note if it isn't. In that case, add this line to your shell's startup file (`~/.bashrc`, `~/.zshrc`, …) and open a new terminal:

   ```sh
   export PATH="$HOME/.local/bin:$PATH"
   ```

Next step: [connect to your TV](first-connection.md).

## What gets installed where

| Path | What |
|---|---|
| `~/.local/bin/tv-remote` | desktop app |
| `~/.local/bin/tv-remote-tui` | terminal UI and command line |
| `~/.local/share/applications/tv-remote.desktop` | "TV Remote" launcher entry |
| `~/.config/systemd/user/tv-remote-st-refresh.{service,timer}` | daily [SmartThings](smartthings.md) login refresh, enabled by the script; it does nothing until you set up SmartThings |

TV Remote creates these when you use it:

| Path | What |
|---|---|
| `~/.config/samsung-tv/config.json` | your TVs, their pairing tokens, settings (mode 0600) |
| `~/.config/samsung-tv/` (other files) | SmartThings app registration and tokens, if you set it up |
| `~/.cache/tv-remote/` | file analysis cache, network test clip (about 375 MB), last scan, ffmpeg log |

If you use [SmartThings](smartthings.md), the systemd user timer `tv-remote-st-refresh.timer` keeps its login alive.

`$XDG_CONFIG_HOME`, `$XDG_DATA_HOME` and `$XDG_CACHE_HOME` are respected if you set them.

## Update

```sh
cd tv-remote
git pull
./install.sh
```

The script rebuilds and overwrites the installed copies. Your config and pairing tokens are kept, so the TV doesn't ask for approval again.

## Uninstall

1. Remove the programs, the launcher entry and the refresh timer:

   ```sh
   cd tv-remote
   ./install.sh --uninstall
   ```

   Without the source folder, do the same by hand:

   ```sh
   systemctl --user disable --now tv-remote-st-refresh.timer
   rm ~/.config/systemd/user/tv-remote-st-refresh.service ~/.config/systemd/user/tv-remote-st-refresh.timer
   rm ~/.local/bin/tv-remote ~/.local/bin/tv-remote-tui
   rm ~/.local/share/applications/tv-remote.desktop
   ```

2. Optional: delete your settings, tokens and cache:

   ```sh
   rm -r ~/.config/samsung-tv ~/.cache/tv-remote
   ```

3. Optional: on the TV, remove the PC from the list of allowed devices (see [First connection](first-connection.md#4-ask-only-the-first-time)). If you used SmartThings, you can also delete the app you registered: `npx --yes @smartthings/cli apps` lists it, and `apps:delete` removes it.

## Terminal UI only

If your GTK or libadwaita is too old for the desktop app, build only the terminal UI and command line:

```sh
cargo build --release -p tv-remote-tui
install -Dm755 target/release/tv-remote-tui ~/.local/bin/tv-remote-tui
```

This needs Rust, a C compiler, pkg-config, the OpenSSL development files and ffmpeg, but not GTK.
