# FAQ

## Which TVs work?

Samsung TVs running Tizen that are on your local network. TV Remote was developed and tested on a 2025 QLED (Q7F series). Other Tizen models use the same local interfaces, but some details differ: the app list, text input, Wake-on-LAN, and which SmartThings features work.

Older Samsung TVs (Orsay, before 2016) and other brands aren't supported.

## Does it work on X11, or only on Wayland?

Both. Capturing the mouse on the **Touchpad & Keyboard** page needs a Wayland desktop with pointer capture support. Elsewhere, the touchpad works by dragging.

## Do I need SmartThings or a Samsung account?

No. Everything except picture mode, sound mode, input source and the cloud fallback works on your local network without any account. See [SmartThings](smartthings.md).

## Does it send anything to the internet?

Only if you set up SmartThings, and then only to the official SmartThings API. Your TV list, pairing tokens and SmartThings tokens stay in `~/.config/samsung-tv/` on your PC. There is no telemetry.

## Can I use it from outside my home?

No. It talks to the TV on your local network. Through a VPN into your home network, the remote and casting can work if the VPN puts you on the same subnet. SmartThings works from anywhere, for the features it covers.

## Why does the TV ask for approval again?

Every new client asks once. See [The approval prompt keeps appearing](troubleshooting.md#the-approval-prompt-keeps-appearing).

## Can I cast Netflix or YouTube from the PC?

Not as a stream. TV Remote casts your own files and media links. For YouTube and Netflix, it opens the TV's own apps (`youtube <url>` opens a given video).

## Can I mirror my screen?

No. TV Remote casts files and links, not your desktop.

## Why can't I seek in TV-friendly mode?

The stream is converted while it plays, so there's nothing ahead to jump to. Use direct mode when the file allows it. See [Casting](casting.md).

## Why no DTS?

The TV has no DTS decoder. TV-friendly mode converts DTS to AC3, which the TV plays.

## Is this allowed by Samsung's terms?

TV Remote uses:

- the TV's own local interfaces, which Samsung provides for remote and companion apps; the remote control channel only works after you approve the PC on the TV's screen;
- open standards: DLNA for casting, DIAL for launching YouTube and Netflix, Wake-on-LAN, and SSDP for discovery;
- the official, public SmartThings API, through an app you register in your own account.

It doesn't modify the TV, bypass any protection, or use anything that requires breaking into the TV. It's meant for personal use with your own TV.

TV Remote is not affiliated with or endorsed by Samsung. This isn't legal advice: if you need certainty for your situation, read Samsung's terms or ask a lawyer.

## Is it free?

Yes. TV Remote is open source under the MIT license.

## How do I report a bug?

Open an issue on GitHub. Include your TV model, the output of `tv-remote-tui info` and `tv-remote-tui tvs list`, and the error message. **Remove** IP and MAC addresses, device ids and tokens first. Never paste `config.json` or `st-app-created.json`.
