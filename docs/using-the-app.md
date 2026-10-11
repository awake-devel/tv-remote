# Using the app

This page covers the desktop app (`tv-remote`) page by page, then the terminal UI (`tv-remote-tui`).

## Desktop app

The window has a sidebar with six pages. At the top of the sidebar, the **TV switcher** shows the TV you're controlling, with two status dots (see [Several TVs](multiple-tvs.md#status-dots)). Click it to switch TV or to manage your TVs.

The button at the right of the header bar shows the connection and power state. Click it to open the **Device** page.

When the window is narrow, the sidebar folds away and a back button appears.

To open the app on a given page, use `--page`:

```sh
tv-remote --page cast
```

Pages: `remote`, `touchpad`, `apps`, `cast`, `picture`, `device`.

### Remote

Every button of the physical remote: power, Source, Menu, Info, Guide, Tools, Exit, the arrows and OK, Back, Home, volume, mute, channels and channel list, the number pad, the four colour keys, and the media keys (rewind, play, pause, stop, fast-forward). Hover a button to see its key code.

Keyboard shortcuts work on the **Remote** and **Touchpad & Keyboard** pages, as long as the focus isn't in a text field:

| Key | TV key |
|---|---|
| Arrow keys | arrows |
| Enter | OK |
| Backspace | Back |
| `+` or `=` | volume up |
| `-` | volume down |
| `M` | mute |
| `H` | Home |
| Page Up / Page Down | channel up / down |
| `0` to `9` | digits |

### Touchpad & Keyboard

**Touchpad.** Moves the TV's pointer. The pointer only exists in the TV's web browser and in apps that show one.

- On Wayland desktops that support pointer capture: click the pad to capture the mouse, then move the mouse to steer the TV pointer. Clicks go to the TV. **Press Esc to release the mouse.** Switching to another window or page also releases it.
- Otherwise: drag on the pad to move the pointer. Keep dragging past the edge to keep moving. Tap or right-click to click.

Right-click always sends a click to the TV. **Pointer speed** sets the sensitivity.

**Keyboard.** Type text and press **Send** to put it in the TV's focused text field. **Enter** presses OK on the TV, and the button next to it presses Back.

Typing works when the TV shows its own system keyboard: the web browser, settings, and most search boxes. YouTube and Netflix draw their own keyboard, so typed text doesn't reach them. Use the arrows there.

When the TV opens its keyboard, the app notices. On this page, it focuses the text field. On other pages, it shows a "The TV is waiting for text" notice with a **Type** button.

### Apps

- **Quick launch**: paste a YouTube link or video id to play it on the TV. The **Netflix** row opens the Netflix app.
- **Installed apps**: click an app to launch it. The list loads when you open the page with the TV on. Press the refresh button to load it again.

The list comes from a set of well-known app ids that the TV is asked about (see [App list empty](troubleshooting.md#the-app-list-is-empty-or-incomplete)).

### Cast

Plays a file from your PC on the TV. The left side is a file browser with your Videos and Downloads folders, home, and mounted drives. You can also drop a file on the page, or use the open button for the system file chooser.

When you pick a file, the right side shows:

- **Compatibility**: ✅ plays, ⚠ needs TV-friendly mode or is untested, ❌ won't play. Expand it for details.
- **How to cast**: **Direct** (the file as-is, seekable) or **TV-friendly** (converted on the fly). With TV-friendly, you choose the **Audio track**, the **Subtitles** and the **Video quality**, and see the **Estimated stream bitrate**.
- **Network check**: whether your network can carry this stream, with suggested fixes you can click. **Test network** measures your network.
- **Cast to the TV**: starts playback. It becomes available once the network check has a verdict.
- **Cast a link**: an http(s) media URL that the TV fetches itself.
- **Now playing**: title, position, pause/resume, stop, seek (direct casts only), and the throughput graph.

The stream comes from the app, so closing the window stops playback. The app asks first.

All details are on the [Casting](casting.md) page.

### Picture & Input

This page needs [SmartThings](smartthings.md). Without it, the page explains how to set it up.

It shows only what your TV reports through SmartThings: **Picture mode**, **Sound mode**, **Input source**, **Power**, **Volume**, **Mute**, **Channel** up/down, and **Go to channel number**. Press the reload button to read the current state.

Some TVs accept picture and sound mode changes without applying them. The page says so when that applies. See [SmartThings](smartthings.md#what-works).

### Device

- **This TV**: model, resolution, network type, IP address, Wi-Fi MAC, REST API version, and the state of the remote connection.
- **Turn on** and **Turn off**.
- **Connection**: **Reconnect** the remote channel, and **Rediscover** the TV if its IP address changed.
- **TVs**: your TVs with their status dots, a **Scan** button, and a **Found** list of new TVs to add. Each TV has a **Use** button and a **⋮** menu: Rename, Link or Unlink SmartThings device, Turn on or off, Remove. See [Several TVs](multiple-tvs.md).
- **Settings of this TV**: the address, the MAC address for Wake-on-LAN, and the SmartThings section. Press **Save settings** after changes.

## Terminal UI

Run `tv-remote-tui` with no arguments to start the terminal UI. Add `--tv <name>` to control another TV than the active one.

The main view shows the TV, its state, and what's playing. The bottom line lists the keys for the current screen. Press `?` or F1 for help.

### Main view

| Key | Action |
|---|---|
| Arrow keys | arrows |
| Enter | OK |
| Backspace | Back |
| `h` | Home |
| `+` or `=` / `-` | volume up / down |
| `m` | mute |
| `M` | Menu |
| `S` | Source |
| `I` | Info |
| `G` | Guide |
| `x` | Exit |
| Page Up / Page Down | channel up / down |
| `0` to `9` | digits |
| F2, F3, F4, F5 | red, green, yellow, blue |
| `<` / `>` | rewind / fast-forward |
| `p` | power: turn off if on, else turn on |
| Space | pause or resume the cast |
| `s` | stop the cast |
| `t` | type text on the TV |
| `a` | apps |
| `c` | cast a file or URL |
| `n` | network test |
| `T` | TV picker |
| `?` or F1 | help |
| `q` or Esc | quit |

### TV picker (`T`)

| Key | Action |
|---|---|
| ↑ ↓ (or `k` `j`) | select |
| Enter | switch to the TV, or add a newly found TV |
| `s` | scan the LAN and SmartThings |
| Esc, `q` or `T` | close |

### Apps (`a`)

| Key | Action |
|---|---|
| ↑ ↓ (or `k` `j`), Home, End | select |
| Enter | launch |
| `r` | refresh the list |
| `y` | open a YouTube URL or video id |
| Esc or `q` | back |

### Casting (`c`)

1. **File browser.** ↑ ↓ to move, Enter or → to open a folder or choose a file, Backspace or ← for the parent folder, Tab to switch between places and files, `.` to show hidden files, `u` to type a URL instead, `r` to reload.
2. **Profile form.** ↑ ↓ to pick a row, ← → (or `h` `l`, Space) to change the audio track, subtitles or quality. Enter runs the network check.
3. **Verdict.** Enter (or `c`) casts. `1` to `9` applies a suggested fix. `n` runs the network test. ↑ ↓ scroll. Esc cancels the analysis or goes back. To cast despite a ❌ verdict, confirm with `y`.

Badges in the terminal: ✅ or OK plays; ⚠ or WARN needs TV-friendly mode or is risky; ❌ or BAD won't play or will stutter.

## Command line

Every action is also a command, for scripts and keyboard shortcuts. See the [Command-line reference](cli-reference.md).
