# Casting

Casting plays a file from your PC on the TV. The TV's built-in media player (DLNA) pulls the file from a small web server that TV Remote runs while you cast. No app needs to be installed on the TV.

You can cast video, audio and image files. You can also cast an http(s) link: the TV then fetches it itself, always in direct mode, and there is no network check.

## Quick start

In the desktop app, open the **Cast** page, pick a file, and press **Cast to the TV**. The page picks the right mode for you.

From the command line:

```sh
tv-remote-tui cast ~/Videos/film.mkv
```

The command stays attached and shows progress. Press Ctrl-C to stop playback.

## Direct and TV-friendly modes

| | Direct | TV-friendly |
|---|---|---|
| What is sent | the file as-is | a stream converted on the fly by ffmpeg |
| Video | unchanged | copied unchanged (HDR kept), or re-encoded to a lower bitrate with VAAPI |
| Audio | all tracks; the TV plays the default one | one track you choose; DTS and TrueHD are converted to AC3 (640 kbit/s) |
| Subtitles | whatever the TV finds in the file | one text subtitle track (SRT, ASS, WebVTT…) kept as a soft SRT track; image subtitles (PGS, VobSub) can't be kept |
| Seeking | yes | no (the stream is live) |
| Pausing | yes | no: the TV refuses to pause a live stream |
| PC load | almost none | light when copying video; GPU work when re-encoding |

Use **direct** when the file plays as-is. Use **TV-friendly** when:

- the audio is DTS (the TV has no DTS decoder, so you'd get no sound);
- you want another audio language than the file's default;
- the file has many audio tracks and your network struggles (TV-friendly sends only one);
- the video bitrate is too high for the TV or your network, and you have VAAPI.

On the command line, `--friendly` selects TV-friendly mode. `--audio`, `--sub` and `--reduce` imply it. If a file needs TV-friendly mode (for example DTS as the default audio track, or HEVC in an AVI file), the command switches to it on its own and tells you. A ⚠ file whose default tracks play as they are (an extra DTS track next to an AC3 one, or a codec that wasn't tested) is cast directly, so you can still seek; pick another track with `--audio`.

## Compatibility badges

When you pick a file, TV Remote reads it with `ffprobe` and shows a badge:

| Badge | Meaning |
|---|---|
| ✅ | the TV plays it directly |
| ⚠ | needs TV-friendly mode (for example DTS audio), or uses a format that wasn't tested |
| ❌ | the TV can't play it, even in TV-friendly mode (for example 8K, or a re-encode is needed and VAAPI isn't available) |

Expand **Compatibility** in the app, or run `netcheck`, to see every track and every issue:

```sh
tv-remote-tui netcheck ~/Videos/film.mkv
```

## What the TV supports

These limits were measured on a 2025 Samsung QLED. Other models are similar but not identical.

| | Supported | Notes |
|---|---|---|
| Video codecs (tested) | H.264, HEVC (H.265), VP9, AV1 | |
| Video codecs (untested) | MPEG-2, MPEG-4 Part 2, VC-1, VP8, MJPEG, WMV3 | probably play; shown as ⚠ |
| Resolution and frame rate | up to 4K (4096 × 2176) at 60 fps | 4K at 120 fps and 8K are rejected |
| HDR | HDR10 | HDR10+ and HLG are kept but weren't verified. Dolby Vision files play their HDR10 base layer; Dolby Vision profile 5 has none, so colours are wrong |
| Audio | AAC, AC3, E-AC3, MP3, MP2, FLAC, Opus, Vorbis, ALAC, PCM | |
| Audio, not supported | DTS (all variants) | convert to AC3 with TV-friendly mode |
| Audio, not verified | TrueHD | converted to AC3 in TV-friendly mode |
| Images | JPEG, PNG, BMP | |

Video bitrate caps (above these, the TV stutters or refuses):

| Codec | Max video bitrate |
|---|---|
| HEVC, VP9 | 80 Mbit/s |
| H.264 | 60 Mbit/s |
| AV1 | 40 Mbit/s |

If the TV doesn't start playing within 30 seconds, TV Remote stops waiting and reports that the format is probably unsupported. Try TV-friendly mode.

## Choosing the audio track

Many films carry several audio tracks: languages, commentaries, or the same mix in DTS and AC3.

- In the app: choose TV-friendly mode, then pick the **Audio track**.
- On the command line: `netcheck` lists the tracks with their index (`#1`, `#2`…). Pass that index to `--audio`.

```sh
tv-remote-tui netcheck ~/Videos/film.mkv
tv-remote-tui cast ~/Videos/film.mkv --audio 2
```

A DTS track is converted to AC3 automatically. An AC3, E-AC3 or AAC track is copied unchanged.

To keep a text subtitle track, pass its index to `--sub`:

```sh
tv-remote-tui cast ~/Videos/film.mkv --audio 2 --sub 5
```

## The network check

Before casting a file, TV Remote compares what the file needs with what your network can deliver. Video bitrate isn't constant: action scenes need far more than the average. So the check looks at the peaks.

### Demand

- **Provisional**: right after you pick a file, the peak is estimated as 1.5 × the average bitrate.
- **Measured**: TV Remote then reads the packet sizes of the whole file to find the highest 10-second and 2-second peaks. Large remuxes take a few minutes. The result is cached, so the next time is instant. On the command line, `--quick` skips this step.

Only the tracks that are actually sent count. In TV-friendly mode, a re-encoded video counts at its target bitrate, and AC3 at 640 kbit/s.

### Capacity: Test network

**Test network** (in the app) or `nettest` (on the command line) measures how fast the TV can pull data from your PC.

- It plays a noise test pattern on the TV for about 10 seconds. Whatever is playing on the TV is interrupted.
- The first run encodes a 20-second, 150 Mbit/s test clip (about 375 MB, stored in `~/.cache/tv-remote/`). That takes about a minute.
- You can cancel at any time. The test pattern stops and nothing is saved.
- The result is considered stale after 24 hours, or when your PC connects to another network (another Wi-Fi network, or Wi-Fi instead of a cable). Test again then. A result measured on another connection isn't used for the verdict at all: the defaults below apply until you test again.

```sh
tv-remote-tui nettest --yes
```

### Verdict

| Verdict | With a measured capacity | Without one (TV on Wi-Fi or unknown) | Without one (TV wired) |
|---|---|---|---|
| ✅ smooth | capacity ≥ 1.5 × the 10 s peak, and ≥ the 2 s peak | peak ≤ 20 Mbit/s | peak ≤ 50 Mbit/s |
| ⚠ may stutter in busy scenes | capacity ≥ the 10 s peak | peak ≤ 40 Mbit/s | peak ≤ 80 Mbit/s |
| ❌ will stutter | capacity below the 10 s peak | above 40 Mbit/s | above 80 Mbit/s |

Many TVs have a 100 Mbit/s Ethernet port, which is why a wired TV isn't assumed to be faster than that. Measuring gives a far better verdict than these defaults.

With ❌, the app asks you to confirm before casting. The command line refuses unless you add `--force` (exit code 3).

### Fixes

With ⚠ or ❌, TV Remote suggests fixes. Each shows the bitrate and verdict you would get. In the app, click a fix to apply it. In the terminal UI, press its number. On the command line, each fix shows the flags to add.

| Fix | What it does | Needs |
|---|---|---|
| TV-friendly mode with only the video and one audio track | drops the extra audio and subtitle tracks, which can add several Mbit/s each (a DTS-HD track is about 3–4 Mbit/s) | nothing |
| …with another audio language | the same, with another track (converted to AC3 if it's DTS) | nothing |
| Re-encode the video to 40, 25 or 15 Mbit/s HEVC | lowers the video bitrate; HDR is kept, and the result is capped at 4K and 60 fps | VAAPI (only listed when VAAPI works on this PC) |
| Copy the file to a USB drive or SSD plugged into the TV | no network at all | a USB drive |

Example output from the command line (on a PC with VAAPI; without it, the re-encode lines are left out):

```text
Verdict   ❌ …
Fixes:
  ⚠ TV-friendly mode with only the video and the #1 … audio track (6 other tracks dropped) → 48.2 Mbps  [--audio 1]
  ✅ Re-encode the video to 25 Mbps HEVC (TV-friendly, one audio track) → 26.0 Mbps (needs VAAPI)  [--audio 1 --reduce 25]
  ✅ Copy the file to a USB drive/SSD plugged into the TV and play it there → 0.0 Mbps
```

### While playing

If playback freezes for 4 seconds, or the TV starts buffering in the middle, TV Remote warns you and suggests a fix. The throughput graph on the **Cast** page shows how fast the TV is pulling data. A photo stays at 0:00 and doesn't trigger this warning.

To seek, click or drag the seek bar on the **Remote** page (it appears while something casts) or on the **Cast** page, or use the remote's rewind / fast-forward keys and the ← → arrows, which jump 10 seconds while a cast plays. In the terminal UI, click the progress bar, use ← → or `<` `>`, or press `0` to `9` to jump to 0 % to 90 %. On the command line, `seek` takes a time, `+10` / `-10`, or a percentage. This also works for a cast started by another device or program, as long as the TV reports its length.

Right after a seek, the TV briefly refuses a pause or another seek. On a wired LAN this clears within about 0.2 s, so TV Remote retries for up to about 1.5 s and then tells you the TV can't do it right now. The TV never seeks to the very end: a jump to the end lands 1 second before it. Seeks land on a keyframe, often 1 to 3 seconds before the point you picked.

A TV-friendly cast is a live stream even when the TV shows its length: TV Remote recognises it (from this app or another instance), never offers to seek in it, and greys out pause. In the first seconds of any cast the TV doesn't know the length yet; a seek then says the cast is still loading.

## When to use a USB drive instead

Copy the file to a USB drive or SSD and plug it into the TV when:

- the verdict stays ❌ and you have no VAAPI to re-encode;
- the file is a 4K Blu-ray remux (peaks of 80–100 Mbit/s or more) and the TV is on Wi-Fi;
- you want to seek in a file that needs TV-friendly mode.

The TV's own player has the same codec limits: a DTS-only file has no sound from USB either.

## Tips

- Wire the PC to the router if you can. Then only the TV's link matters.
- If the TV is on Wi-Fi, 5 GHz and a short distance to the router help most. The same goes for the PC: on 2.4 GHz Wi-Fi a PC measured about 40 Mbit/s to the TV, against about 160 Mbit/s when wired.
- The cast server listens on a random TCP port. If you run a firewall on the PC, allow incoming connections from your LAN: see [Troubleshooting](troubleshooting.md#firewall).
- The ffmpeg log of the last TV-friendly cast is in `~/.cache/tv-remote/ffmpeg-last.log`.
