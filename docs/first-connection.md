# First connection

This page connects TV Remote to your TV. You do it once per TV. It takes about two minutes.

Samsung menu names vary a little between models and firmware versions. The French names are given in parentheses.

## Before you start

- **Turn the TV on.** It must be on (not in standby) for pairing.
- **Put the TV and the PC on the same network.** They must be in the same subnet, for example both on your home router. TV Remote finds the TV with multicast (SSDP), and Wake-on-LAN uses broadcast. Neither crosses routers, VLANs, or guest Wi-Fi networks that isolate clients. Wired or Wi-Fi doesn't matter.
- **Install TV Remote** first: see [Installation](installation.md).

## 1. Find the TV

```sh
tv-remote-tui tvs scan
```

This searches your network for 3 seconds. You see a numbered list:

```text
Scanning the LAN (3s)…
  #  WHERE     NAME                 IP              MODEL           SMARTTHINGS                KNOWN
  1  LAN       Samsung Q7F Series   192.0.2.10      QE65Q7FAAU      –                          new
Add one with `tv-remote-tui tvs add <#>`.
(no SmartThings token: LAN only)
```

If nothing is found, see [The TV isn't found](troubleshooting.md#the-tv-isnt-found).

If you know the TV's IP address, you can skip the scan and use the address in the next step.

## 2. Add the TV

Use the number from the list:

```sh
tv-remote-tui tvs add 1
```

Or use the TV's IP address:

```sh
tv-remote-tui tvs add 192.0.2.10
```

The first TV you add becomes the active TV. Every command, the terminal UI and the desktop app control the active TV.

## 3. Accept the prompt on the TV

Send any command, for example the Home key:

```sh
tv-remote-tui key home
```

**Look at the TV.** It shows a prompt asking whether to allow a new device. Select **Allow** (Autoriser) with the TV's own remote.

- The command waits while the prompt is on screen. If you're slow, it may time out. That's fine: the token is still saved when you accept. Just run the command again.
- TV Remote saves the pairing token in `~/.config/samsung-tv/config.json`. The TV doesn't ask again for this program on this PC.
- Each new client asks once. The desktop app, the terminal UI and the command line share the token, so you accept once for all three.

If you pressed **Deny** (Refuser) by mistake, see [The TV refused the connection](troubleshooting.md#the-tv-refused-the-connection).

## 4. Ask only the first time

By default, some TVs ask every time a client connects without a known token. Set the TV to ask only the first time:

**Settings → General → External Device Manager → Device Connection Manager → Access Notification → First Time Only**

(Paramètres → Général → Gestionnaire de périphériques externes → Gestionnaire de connexion des périphériques → Notification d'accès → Première fois uniquement)

On recent models, External Device Manager is under **Settings → Connection** (Paramètres → Connexion) instead of General.

The same Device Connection Manager has a **Device List** (Liste des périphériques). It shows every client you allowed or denied, and you can change or delete them there.

## 5. Give the TV a name

The TV gets a short id (`tv1`) and the name it reports. Give it a name you'll recognise:

```sh
tv-remote-tui tvs rename tv1 "Living room"
```

You can then use the name in commands: `--tv "living room"` or just `--tv living`. See [Several TVs](multiple-tvs.md).

## 6. Allow turning the TV on from the network

To turn the TV on from the PC (Wake-on-LAN), enable:

**Settings → General → Network → Expert Settings → Power On with Mobile**

(Paramètres → Général → Réseau → Paramètres expert → Allumer avec un appareil mobile)

On recent models, Network is under **Settings → Connection** (Paramètres → Connexion).

Then test it. Turn the TV off, wait 15 seconds, and run:

```sh
tv-remote-tui on
```

TV Remote learns the TV's MAC address during the scan. If `on` says the MAC address is missing, run `tvs scan` again with the TV on.

If the TV is [linked to SmartThings](smartthings.md), `on` also asks SmartThings to turn it on. That helps when Wake-on-LAN doesn't work over Wi-Fi.

## 7. Open the app

Open **TV Remote** from your app launcher, or run:

```sh
tv-remote
```

The status in the header bar shows the connection and power state. Next: [Using the app](using-the-app.md).

## Doing it from the desktop app instead

You can also pair from the desktop app:

1. Open **TV Remote** and go to the **Device** page.
2. In the **TVs** section, press **Scan**.
3. Under **Found**, add your TV.
4. Press **Use** on it, then accept the prompt on the TV.
5. Use the **⋮** menu on the TV's row to rename it.
