# Command-line reference

`tv-remote-tui` is both the terminal UI and a command-line tool. Without a command, it starts the terminal UI. With a command, it runs it and exits.

```sh
tv-remote-tui [OPTIONS] [COMMAND]
```

Run `tv-remote-tui --help`, or `tv-remote-tui <command> --help`, for the built-in help.

## Global options

These work with every command.

| Option | What it does |
|---|---|
| `--tv <NAME\|ID>` | control this TV instead of the active one: id, name, IP address or unique prefix (see `tvs list`) |
| `--json` | JSON output, for `info`, `status`, `apps`, `netcheck`, `discover`, `smartthings` and `tvs` |
| `--config <PATH>` | use another config file (default `~/.config/samsung-tv/config.json`); like the default, a missing file starts empty and is created on the first change |
| `-h`, `--help` | help |
| `-V`, `--version` | version |

## Remote control

| Command | Description | Example |
|---|---|---|
| `key <KEY>` | send a remote key | `tv-remote-tui key home` |
| `key <KEY> --hold [SECS]` | long-press a key (default 1 s, max 30 s) | `tv-remote-tui key power --hold 3` |
| `text <TEXT>...` | type text into the TV's focused text field | `tv-remote-tui text hello world` |
| `volume` | show the volume | `tv-remote-tui volume` |
| `volume <0-100>` | set the volume | `tv-remote-tui volume 20` |
| `mute` | show the mute state | `tv-remote-tui mute` |
| `mute on\|off\|toggle` | set the mute state | `tv-remote-tui mute toggle` |
| `on [--timeout SECS]` | turn the TV on (Wake-on-LAN, plus SmartThings if linked); default timeout 25 s | `tv-remote-tui on` |
| `off` | turn the TV off | `tv-remote-tui off` |

Key names are case-insensitive. You can use the code (`KEY_VOLUP`), the short code (`volup`), the name (`VolumeUp`, `volume_up`) or the button label (`'Vol +'`, `ok`, `back`). An unknown name that starts with `KEY_` is sent as-is.

Common keys:

| Key | Name to use |
|---|---|
| arrows | `up`, `down`, `left`, `right` |
| OK | `enter` or `ok` |
| Back | `return` or `back` |
| Home | `home` |
| Volume | `volup`, `voldown`, `mute` |
| Channels | `chup`, `chdown` |
| Power | `power` |
| Source, Menu | `source`, `menu` |
| Media | `play`, `pause`, `stop`, `rewind`, `ff` |
| Colours | `red`, `green`, `yellow`, `blue` |
| Digits | `0` to `9` |

## Information

| Command | Description | Example |
|---|---|---|
| `info` | device information: name, model, power, resolution, network, IP, Wi-Fi MAC, OS | `tv-remote-tui info` |
| `status` | power, volume, mute, and what the TV's media player is playing | `tv-remote-tui status --json` |
| `discover [--timeout SECS]` | list Samsung TVs and media players on the LAN (SSDP) | `tv-remote-tui discover` |
| `discover --update` | find the active TV again and save its new IP address | `tv-remote-tui discover --update` |

## Apps

| Command | Description | Example |
|---|---|---|
| `apps` | list installed apps with their ids | `tv-remote-tui apps` |
| `launch <APP_ID>` | launch an app by id | `tv-remote-tui launch 111299001912` |
| `youtube <VIDEO>` | open a YouTube video (URL or 11-character id) | `tv-remote-tui youtube https://youtu.be/aaaaaaaaaaa` |

## Casting

| Command | Description | Example |
|---|---|---|
| `cast <FILE\|URL>` | cast a file or an http(s) URL; stays attached until playback ends (Ctrl-C stops it) | `tv-remote-tui cast film.mkv` |
| `netcheck <FILE>` | list the tracks, the compatibility, the network demand, the verdict and fixes; nothing is cast, it only asks the TV (one local request) whether it is wired or on Wi-Fi | `tv-remote-tui netcheck film.mkv` |
| `nettest --yes` | measure the network capacity (plays a test pattern on the TV for about 10 s) | `tv-remote-tui nettest --yes` |
| `pause` | pause the TV's media player | `tv-remote-tui pause` |
| `play` | resume playback | `tv-remote-tui play` |
| `stop` | stop playback | `tv-remote-tui stop` |
| `seek <POSITION>` | seek, as `HH:MM:SS`, `MM:SS` or seconds (direct casts only) | `tv-remote-tui seek 1:02:30` |

Options for `cast` and `netcheck`:

| Option | What it does |
|---|---|
| `--friendly` | TV-friendly mode: one audio track, DTS → AC3, no seeking |
| `--audio <N>` | audio track to send (index from `netcheck`); implies TV-friendly |
| `--sub <N>` | text subtitle track to keep (index from `netcheck`); implies TV-friendly |
| `--reduce <MBPS>` | re-encode the video to this bitrate (1–80) with VAAPI; implies TV-friendly |
| `--quick` | skip the packet analysis; use the cached one or an estimate |

Options for `cast` only:

| Option | What it does |
|---|---|
| `--force` | cast even if the verdict is ❌ or the file looks unsupported |
| `--title <TITLE>` | title shown on the TV |

```sh
tv-remote-tui netcheck film.mkv
tv-remote-tui cast film.mkv --audio 2 --sub 5 --title "Film"
```

See [Casting](casting.md).

## SmartThings

`smartthings` can be shortened to `st`. See [SmartThings](smartthings.md) for the setup.

| Command | Description | Example |
|---|---|---|
| `smartthings app-import [FILE]` | import the app you registered (default `~/.config/samsung-tv/st-app-created.json`) | `tv-remote-tui st app-import` |
| `smartthings login [--local]` | log in through the browser, then paste the httpbin.org address (or code); `--local` tries the automatic localhost redirect instead | `tv-remote-tui st login` |
| `smartthings refresh [--quiet]` | renew the access token now | `tv-remote-tui st refresh` |
| `smartthings logout` | delete the SmartThings tokens from the config | `tv-remote-tui st logout` |
| `smartthings auth-status` | show the login state | `tv-remote-tui st auth-status` |
| `smartthings devices` | list all devices in the account (TVs are marked `[TV]`) | `tv-remote-tui st devices` |
| `smartthings status` | the TV's state: power, volume, input, picture and sound modes, capabilities | `tv-remote-tui st status` |
| `smartthings input <SOURCE>` | switch the input, by id or name | `tv-remote-tui st input HDMI1` |
| `smartthings picture-mode <MODE>` | set the picture mode, by id or name | `tv-remote-tui st picture-mode movie` |
| `smartthings sound-mode <MODE>` | set the sound mode, by id or name | `tv-remote-tui st sound-mode standard` |

## Several TVs

See [Several TVs](multiple-tvs.md).

| Command | Description | Example |
|---|---|---|
| `tvs list [--offline]` | your TVs and their status; `--offline` contacts nothing | `tv-remote-tui tvs list` |
| `tvs scan [--timeout SECS]` | find TVs on the LAN and in SmartThings (default 3 s) | `tv-remote-tui tvs scan` |
| `tvs add <CANDIDATE> [--name NAME] [--use]` | add a TV from the last scan, or any IP address | `tv-remote-tui tvs add 1 --name "Living room"` |
| `tvs rename <TV> <NAME>` | rename a TV | `tv-remote-tui tvs rename tv1 "Living room"` |
| `tvs remove <TV>` | forget a TV and delete its pairing token (alias `rm`) | `tv-remote-tui tvs remove tv2` |
| `tvs use <TV>` | make a TV the active one | `tv-remote-tui tvs use bedroom` |
| `tvs link <TV> <DEVICE_ID>` | link a TV to a SmartThings device (`st:` prefix allowed) | `tv-remote-tui tvs link tv1 00000000-0000-4000-8000-000000000001` |
| `tvs unlink <TV>` | unlink it; it won't be linked automatically again | `tv-remote-tui tvs unlink tv1` |

`tvs list` can be shortened to `tvs ls`.

## Exit codes

| Code | Meaning |
|---|---|
| 0 | success |
| 1 | error: TV unreachable, timeout, config problem, SmartThings error… |
| 2 | usage error: bad arguments, unknown key or TV, `nettest` without `--yes` |
| 3 | refused: the network verdict is ❌ or the file is unsupported (add `--force`), or a re-encode needs VAAPI |
| 4 | the SmartThings login expired: run `smartthings login` again (the daily refresh timer does not retry on this code) |
| 130 | interrupted with Ctrl-C |

Ctrl-C stops a running command cleanly: a cast stops playback, and a network test stops the test pattern. Press Ctrl-C a second time to exit at once.

## Examples for scripts and shortcuts

```sh
# Turn on the TV and open YouTube
tv-remote-tui on && tv-remote-tui launch 111299001912

# Mute toggle for a keyboard shortcut
tv-remote-tui mute toggle

# Volume of every TV, as JSON
for tv in tv1 tv2; do tv-remote-tui --tv "$tv" status --json; done
```
