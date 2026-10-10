# Several TVs

TV Remote can control any number of TVs. Each TV has its own pairing token, so each one shows the approval prompt once. One TV is **active**: commands, the terminal UI and the desktop app control it unless you pick another one.

## Commands

| Command | What it does |
|---|---|
| `tvs list` | your TVs with their LAN, cloud and power status (`*` marks the active one) |
| `tvs scan` | finds TVs on the LAN and in SmartThings, and matches them |
| `tvs add <n>` | adds a TV from the last scan, or any IP address |
| `tvs rename <tv> <name>` | renames a TV |
| `tvs remove <tv>` | forgets a TV and deletes its pairing token |
| `tvs use <tv>` | makes a TV the active one |
| `tvs link <tv> <device-id>` | links a TV to a SmartThings device |
| `tvs unlink <tv>` | unlinks it; it won't be linked automatically again |

Wherever a command takes `<tv>`, you can give the id (`tv2`), the name in any case (`bedroom`), the IP address, or a unique prefix (`bed`).

## Add a second TV

1. Scan:

   ```sh
   tv-remote-tui tvs scan
   ```

   ```text
     #  WHERE     NAME                 IP              MODEL           SMARTTHINGS                KNOWN
     1  LAN+CLOUD Living room          192.0.2.10      QE65Q7FAAU      00000000-000… (Mac)        tv1
     2  LAN       Samsung 7 Series     192.0.2.11      UE50TU7000      –                          new
   Add one with `tv-remote-tui tvs add <#>`.
   ```

2. Add it, give it a name, and make it active:

   ```sh
   tv-remote-tui tvs add 2 --name "Bedroom" --use
   ```

3. Send a command and accept the prompt on that TV:

   ```sh
   tv-remote-tui key home
   ```

`tvs add` accepts the number from the last scan, its IP address, its name, `st:<device-id>` for a TV found only in SmartThings, or any IP address (even one the scan didn't find).

## Pick a TV for one command: `--tv`

`--tv` works with every command and with the terminal UI. It doesn't change the active TV.

```sh
tv-remote-tui --tv bedroom volume 15
tv-remote-tui --tv tv1 off
tv-remote-tui --tv bedroom          # terminal UI on the bedroom TV
```

## Change the active TV

```sh
tv-remote-tui tvs use bedroom
```

The desktop app and the terminal UI also start on the active TV.

- **Desktop app:** click the TV switcher at the top of the sidebar and pick a TV. **Manage TVs…** opens the **Device** page, where each TV has a **Use** button and a **⋮** menu (Rename, Link or Unlink SmartThings device, Turn on or off, Remove).
- **Terminal UI:** press `T` to open the TV picker. Enter switches to the selected TV. `s` scans, and Enter on a newly found TV adds it.

A cast keeps playing on the TV it started on, even if you switch to another TV.

## Status dots

The desktop app shows two dots next to each TV: the first for the LAN, the second for SmartThings.

| Colour | First dot (LAN) | Second dot (SmartThings) |
|---|---|---|
| green | on | online |
| orange | standby, or unknown | state unknown |
| red | unreachable | offline |
| grey | not checked yet | not linked, or not checked yet |

The LAN is checked every 5 seconds and SmartThings every minute. `tvs list` shows the same information as text:

```text
  ID    NAME                 HOST            LAN         CLOUD     POWER    SMARTTHINGS
* tv1   Living room          192.0.2.10      reachable   online    on       00000000-000…
  tv2   Bedroom              192.0.2.11      unreachable –         off      –
```

`tvs list --offline` reads only the config and contacts nothing.

## Fallback through SmartThings

When a TV is [linked to SmartThings](smartthings.md), volume, mute and power work even if the TV doesn't answer on the LAN:

- TV Remote tries the LAN first. If the last LAN request in the past 30 seconds failed, it tries SmartThings first instead.
- If SmartThings reported the TV offline in the past 2 minutes, it's skipped.
- `on` sends Wake-on-LAN and also asks SmartThings to turn the TV on: immediately if the LAN is silent, otherwise after 4 seconds.

Other features (keys, apps, casting) need the LAN.

A TV found only in SmartThings (no LAN address) can still be added. Then only power, volume, mute and the SmartThings features work, until a scan on its network finds its address.

## When a TV's IP address changes

Your router gives the TV its IP address (DHCP), and that address can change, for example after a power cut.

- `tvs scan` recognises a known TV by its MAC address and updates its address. It prints `address changed … (saved)`.
- `discover --update` does the same for the active TV.
- In the desktop app, use **Rediscover** on the **Device** page.

To avoid the problem, reserve a fixed address for the TV in your router's DHCP settings ("DHCP reservation" or "static lease").
