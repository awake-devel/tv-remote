# Troubleshooting

Start with these two commands. They show what TV Remote knows and what answers:

```sh
tv-remote-tui tvs list
tv-remote-tui info
```

## The TV isn't found

`tvs scan` or **Scan** finds nothing, or the TV shows as `unreachable`.

1. Turn the TV on. A TV in deep standby doesn't answer.
2. Check that the PC and the TV are on the **same network and subnet**. See [Different subnets or VLANs](#different-subnets-or-vlans).
3. Check your PC's firewall: see [Firewall](#firewall). The scan needs the TV's SSDP answers (UDP) to come in.
4. Add the TV by its IP address. You can find the address in the TV's network settings, or in your router's list of devices:

   ```sh
   tv-remote-tui tvs add 192.0.2.10
   ```

## The approval prompt keeps appearing

The TV asks again each time a client connects without a token it knows.

- Set **Access Notification** to **First Time Only** (Notification d'accès → Première fois uniquement). See [First connection](first-connection.md#4-ask-only-the-first-time).
- Check that the config file is writable. The token is saved in `~/.config/samsung-tv/config.json` after you accept. If that file is broken, TV Remote refuses to overwrite it and says so: fix or move the file.
- Each separate client asks once. If you also use other remote apps or scripts, each of them asks too.
- `tvs remove` deletes a TV's token. Adding the TV again shows the prompt again.

## The first command times out while waiting for approval

The command waits while the prompt is on screen, and gives up after about 12 seconds. TV Remote keeps listening for about 30 seconds in total. If you accept within that time, the token is still saved.

Run the command again. It should work without a prompt.

## The TV refused the connection

The app shows "The TV refused the connection", or commands fail as unauthorized. You probably pressed **Deny**, or the TV forgot the PC.

1. On the TV, open **Device Connection Manager → Device List** (Gestionnaire de connexion des périphériques → Liste des périphériques). See [First connection](first-connection.md#4-ask-only-the-first-time) for the full path.
2. Find the entry for TV Remote. Set it to **Allow**, or delete it.
3. Send a command again and accept the prompt.

## The app list is empty or incomplete

Recent firmware ignores the remote-channel request that used to list installed apps (`ed.installedApp.get`). So TV Remote asks the TV's REST API about a list of well-known app ids (YouTube, Netflix, Prime Video, and others) and shows those the TV reports as installed.

- An app that isn't in that list doesn't appear. If you know its id, launch it directly:

  ```sh
  tv-remote-tui launch 3201907018807
  ```

- The TV must be on. In the app, open the **Apps** page with the TV on, or press refresh.

## Typing doesn't reach YouTube or Netflix

Text input only reaches the TV's system keyboard: the web browser, settings, and most search boxes. YouTube and Netflix draw their own on-screen keyboard, which ignores typed text. Use the arrows and OK there.

For YouTube, you can also try opening a video directly (on TVs that refuse video links, this opens the YouTube app instead):

```sh
tv-remote-tui youtube https://youtu.be/aaaaaaaaaaa
```

## Wake-on-LAN doesn't wake the TV

1. Enable **Power On with Mobile** (Allumer avec un appareil mobile) on the TV. See [First connection](first-connection.md#6-allow-turning-the-tv-on-from-the-network).
2. Check that TV Remote knows the TV's MAC address. `tvs list --json` shows `mac`. If it's empty, run `tvs scan` with the TV on, or enter it in the desktop app's **Device** page (**TV MAC address**).
3. Wait about 15 seconds after turning the TV off. Some TVs ignore wake-up packets right after shutting down.
4. A TV on Wi-Fi often can't be woken from deep standby. A wired connection is more reliable.
5. [Link the TV to SmartThings](smartthings.md). `on` then also asks SmartThings to turn the TV on.
6. Wake-on-LAN uses broadcast, which doesn't cross routers or VLANs.

## Casting stutters

1. Run the network test, in the app (**Test network**) or with `tv-remote-tui nettest --yes`. The verdict is much more accurate with a measured capacity.
2. Use **TV-friendly** mode and keep only one audio track. Remuxes often carry several DTS-HD tracks of 3–4 Mbit/s each, which the TV doesn't even use.
3. If you have VAAPI, apply a "Re-encode the video" fix.
4. Move the TV closer to the router, use 5 GHz Wi-Fi, or wire it.
5. For 4K remuxes over Wi-Fi, use a USB drive plugged into the TV.

See [Casting](casting.md#the-network-check).

## The cast never starts

"The TV never started playing" means the TV didn't accept the file within 30 seconds. The format is probably unsupported. Try TV-friendly mode (`--friendly`). Check the [supported formats](casting.md#what-the-tv-supports).

If it never starts for any file, check the [firewall](#firewall): the TV must be able to connect to your PC.

## No sound when casting

The audio track is probably DTS. The TV has no DTS decoder. Use TV-friendly mode: it converts DTS to AC3. In the app, choose **TV-friendly**; on the command line:

```sh
tv-remote-tui cast film.mkv --friendly
```

Also check that the chosen audio track is the one you want: `netcheck` lists them.

## A YouTube video doesn't open (HTTP 403)

`youtube`, and the YouTube field on the **Apps** page, open the video with DIAL. Some TVs refuse that: the 2025 Q7F tested on 2026-10-11 answers HTTP 403 to every YouTube DIAL launch, even when logged in to YouTube, and has no other direct way to open a video (its other launch commands open YouTube's home screen or are ignored).

When the TV refuses the link, TV Remote opens the **YouTube app** instead and says so ("this TV doesn't accept YouTube video links"). Pick the video on the TV. On TVs that accept DIAL, the video opens directly.

## The TV is unmuted after turning it on

The TV doesn't keep mute across standby: after `off` then `on` it comes back unmuted at its previous volume. Changing the volume (a volume key, or `volume N` through SmartThings) also unmutes it, as on the TV's own remote.

## Seeking doesn't work

Seeking only works in direct mode. A TV-friendly stream is converted live, so it can't jump ahead.

## SmartThings login page shows "403 Access forbidden to this app"

This happens when the login redirects to `localhost` (`smartthings login --local`). Use the default login, which redirects to httpbin.org and asks you to paste the address:

```sh
tv-remote-tui smartthings login
```

Also check that the scopes in the login link are the ones your app was registered with (`docs/smartthings-app.json`): a scope the app doesn't have makes the login fail.

## SmartThings: `invalid_grant`

The refresh token was refused. It expired (for example the PC was off for weeks and the daily timer didn't run), or it was revoked. Log in again:

```sh
tv-remote-tui smartthings login
```

Then check that the timer is active:

```sh
systemctl --user list-timers tv-remote-st-refresh.timer
```

See [SmartThings](smartthings.md).

## SmartThings: picture or sound mode doesn't change

Some TVs (seen on the 2025 Q7F) accept these commands without applying them. Volume, mute, power and input source don't have this problem. Change the mode with the TV's own remote.

## Firewall

The TV connects back to your PC to fetch the stream when you cast. The cast server listens on a **random TCP port** each time, so you can't open a single port.

Allow incoming connections from your LAN, or at least from the TV's address. For example, with firewalld:

```sh
sudo firewall-cmd --permanent --zone=trusted --add-source=192.0.2.10
sudo firewall-cmd --reload
```

With ufw:

```sh
sudo ufw allow from 192.0.2.10
```

Replace `192.0.2.10` with your TV's address. Discovery also needs the TV's UDP answers (SSDP) to come in.

## Different subnets or VLANs

TV Remote is designed for a TV and a PC on the same network.

- Discovery (SSDP multicast) and Wake-on-LAN (broadcast) don't cross routers or VLANs.
- Some routers isolate Wi-Fi clients, or put the TV on a guest network. Then the PC can't reach the TV at all.

If your network is split, you can still add the TV by IP address (`tvs add <ip>`) if your router allows traffic between the networks. Casting then also needs the TV to reach the PC on any TCP port. Turning on works through [SmartThings](smartthings.md) instead of Wake-on-LAN.

## The config file is broken

If `~/.config/samsung-tv/config.json` can't be read, TV Remote never overwrites it. The desktop app shows a warning and runs without saving. If the file breaks while an app is running (for example after a hand edit), saving fails with an error instead of overwriting it. Fix the JSON, or move the file away to start over:

```sh
mv ~/.config/samsung-tv/config.json ~/.config/samsung-tv/config.json.bak
```

Starting over means pairing again.
