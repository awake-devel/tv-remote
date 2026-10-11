# SmartThings (optional)

SmartThings is Samsung's cloud service for connected devices. TV Remote works without it. Set it up if you want:

- picture mode, sound mode and input source (the TV's local interfaces don't offer these);
- power, volume and mute that still work when the TV doesn't answer on the local network;
- turning the TV on when Wake-on-LAN fails (common over Wi-Fi);
- TVs found through your Samsung account, not only on the LAN.

Your TV must already be in your SmartThings account. Add it with the SmartThings phone app if it isn't.

## Why the setup has several steps

SmartThings personal access tokens (PATs) now expire 24 hours after you create them. That's fine for a quick test, but not for daily use. So TV Remote uses OAuth instead, like the official apps do: you register a small private app in your own SmartThings account once, log in once, and TV Remote then renews its access on its own.

You need Node.js for step 2 (the official SmartThings CLI runs with `npx`). You only need it once.

## Setup

### 1. Copy the app template and name your app

The repository includes a template, [`docs/smartthings-app.json`](smartthings-app.json). Copy it and set your own `appName`:

```sh
cp docs/smartthings-app.json ~/smartthings-app.json
```

Open `~/smartthings-app.json` in a text editor and change the `appName` value to something unique to you, for example `tv-remote-yourname`. Lowercase letters, digits and dashes are safest. Leave the other fields as they are.

### 2. Register the app in your account

Run the official SmartThings CLI. It opens your browser so you can log in to your Samsung account:

```sh
mkdir -p ~/.config/samsung-tv
npx --yes @smartthings/cli apps:create -i ~/smartthings-app.json -o ~/.config/samsung-tv/st-app-created.json -j
chmod 600 ~/.config/samsung-tv/st-app-created.json
```

This creates a **private app in your own account only**. Nobody else can see or use it, and it isn't published anywhere.

The output file `st-app-created.json` contains the app's **client secret**. Treat it like a password: never commit it to Git, paste it in an issue, or share it.

### 3. Import the app into TV Remote

```sh
tv-remote-tui smartthings app-import
```

This reads `~/.config/samsung-tv/st-app-created.json` and saves the app's id and credentials in your TV Remote config. If you saved the file elsewhere, pass its path:

```sh
tv-remote-tui smartthings app-import ~/path/to/st-app-created.json
```

### 4. Log in

```sh
tv-remote-tui smartthings login
```

1. Your browser opens the Samsung login page. Log in.
2. Choose the **location** (home) that contains your TV.
3. Approve the access.
4. The browser lands on an **httpbin.org** page showing some JSON. Its address contains `code=…`.
5. Paste that address (or just the code) into the terminal and press Enter.

The command then says you're connected. The code is single-use, expires after about 5 minutes, and is useless without your app's secret, which stays on your PC.

In the app: **Device** page → SmartThings → **Connect SmartThings**, then paste the address into the dialog.

> **Why paste?** SmartThings answers *"403 Access forbidden to this app"* for apps that redirect to `localhost` on some accounts, so the httpbin redirect is the default. To try the automatic local redirect anyway: `tv-remote-tui smartthings login --local`.

### 5. Keep the login alive

The access token lasts 24 hours. TV Remote renews it with a refresh token, which SmartThings expires if it isn't used for a while. To keep it fresh even when you don't use TV Remote for weeks, a systemd user timer, `tv-remote-st-refresh.timer`, runs `tv-remote-tui smartthings refresh --quiet` once a day.

Check the login and the timer:

```sh
tv-remote-tui smartthings auth-status
systemctl --user list-timers tv-remote-st-refresh.timer
```

You can renew by hand at any time:

```sh
tv-remote-tui smartthings refresh
```

### 6. Link the TV

Run a scan. It matches the TV found on your LAN with the one in your SmartThings account (by MAC address, unique id, or model and name):

```sh
tv-remote-tui tvs scan
```

The `SMARTTHINGS` column shows the matched device id. If your TV appears twice (once `LAN`, once `CLOUD`) instead of once as `LAN+CLOUD`, link it by hand. Find its device id:

```sh
tv-remote-tui smartthings devices
```

Then link it (`tv1` is your TV's id or name from `tvs list`):

```sh
tv-remote-tui tvs link tv1 00000000-0000-4000-8000-000000000001
```

In the desktop app, use **Link SmartThings device…** in the TV's **⋮** menu on the **Device** page.

Check that it works:

```sh
tv-remote-tui smartthings status
```

## What works

What SmartThings offers depends on the TV model. TV Remote only shows what your TV reports. `smartthings status` lists your TV's capabilities.

| Feature | Command | Notes |
|---|---|---|
| Power on and off | `on`, `off` | reliable; used together with Wake-on-LAN |
| Volume and mute | `volume`, `mute` | reliable; used when the LAN doesn't answer |
| Status (power, volume, input, modes) | `smartthings status` | reliable; when the TV is offline (unplugged, no network), it says so, and the values shown are the last ones the TV reported |
| Input source | `smartthings input HDMI1` | by id or name, any case |
| Picture mode | `smartthings picture-mode movie` | see the limitation below |
| Sound mode | `smartthings sound-mode standard` | see the limitation below |
| Channel up, down, number | desktop app, **Picture & Input** page | when the TV reports it |

**Known limitation.** On the 2025 Q7F, SmartThings accepts picture mode and sound mode commands and reports them as completed, but the TV doesn't apply them. Volume, mute and power work. Other models may behave differently.

There is no brightness control through SmartThings on the tested TV.

**Stale volume.** On the same TV, the volume SmartThings reports can lag behind (it said 0 while the TV was at 3). The desktop app's **Picture & Input** page therefore shows the TV's own volume and mute when the TV answers on the LAN. Setting the volume through SmartThings unmutes the TV.

## Log out

```sh
tv-remote-tui smartthings logout
```

This deletes the tokens from your config. The registered app stays in your account, so you can log in again later without step 2.

To remove access completely, also delete the app: `npx --yes @smartthings/cli apps` lists it, and `npx --yes @smartthings/cli apps:delete` removes it.

## Legacy: personal access tokens

Older setups used a personal access token (PAT), stored as `smartthings_token` in the config or entered in the desktop app's **Device** page. A PAT still works, but tokens created since the end of 2024 expire after 24 hours, so you'd need to create a new one every day at [account.smartthings.com/tokens](https://account.smartthings.com/tokens).

When both are configured, the OAuth login is used and the PAT is ignored. Use the OAuth setup above.

## Privacy

- The app credentials and tokens stay on your PC, in `~/.config/samsung-tv/` (mode 0600).
- TV Remote talks only to the official SmartThings API (`api.smartthings.com`) and Samsung's login pages.
- Everything else TV Remote does stays on your local network.
