# TV Remote documentation

TV Remote controls a Samsung Tizen TV from a Linux PC. It comes as two programs built on one shared Rust library:

- `tv-remote`: a desktop app (GTK 4 and libadwaita).
- `tv-remote-tui`: a terminal UI, and a command-line tool for scripts.

With it you can:

- use every remote key, a touchpad and a keyboard;
- launch apps, and open YouTube videos or Netflix;
- cast video, audio and image files from your PC, with a check that tells you whether the TV can play the file and whether your network can carry it;
- turn the TV on and off, and set the volume;
- optionally use SmartThings for picture mode, input source, and power and volume when the local network can't reach the TV;
- control several TVs.

Everything except SmartThings runs on your local network. Nothing is sent to a server.

## Quick start

1. [Install](installation.md) the dependencies, then run `./install.sh`.
2. [Connect to your TV](first-connection.md): `tv-remote-tui tvs scan`, then `tv-remote-tui tvs add 1`, then accept the prompt on the TV.
3. Open **TV Remote** from your app launcher.

## Pages

| Page | What it covers |
|---|---|
| [Installation](installation.md) | Requirements for Arch, Debian/Ubuntu and Fedora, installing, updating, uninstalling |
| [First connection](first-connection.md) | Finding the TV, pairing, TV settings to change once |
| [Using the app](using-the-app.md) | The desktop app page by page, and the terminal UI keys |
| [Casting](casting.md) | Direct and TV-friendly modes, what the TV plays, the network check |
| [SmartThings](smartthings.md) | Optional cloud features and their one-time setup |
| [Several TVs](multiple-tvs.md) | Adding, naming and switching TVs, LAN and cloud fallback |
| [Command-line reference](cli-reference.md) | Every command, with examples and exit codes |
| [Troubleshooting](troubleshooting.md) | Common problems and their fixes |
| [FAQ](faq.md) | Short answers, including what Samsung allows |
| [How it works](how-it-works.md) | For contributors: architecture, protocols, config file, tests |

## Supported TVs

TV Remote targets Samsung TVs running Tizen (2016 and later) that are on the same network as your PC. It was developed and tested on a 2025 QLED (Q7F series). Older or newer models use the same local interfaces, but some details can differ: see [Troubleshooting](troubleshooting.md).

TV Remote is not affiliated with or endorsed by Samsung.
