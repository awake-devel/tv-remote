# Feature research: what else tv-remote could do

Research date: 2026-10-10. Scope: new features for tv-remote, based on what three surfaces can do: the TV's **LAN** interfaces, the **SmartThings** cloud API (ST), and the **Samsung account** and its services. Target TV: 2025 85" Q7F (Tizen), the TV the app was developed on.

Method: I read the repository docs (`README.md`, `docs/*.md`, `core/API.md`) and skimmed `core/src/*.rs`, so this report doesn't re-propose features that already exist. I then read the code of other open-source projects and public documentation. **Nothing in this report was tested against the TV or the SmartThings API.** "Confirmed" in the feasibility column means confirmed by the app's own live testing (see the TV notes) or by a working implementation of the same model generation. Everything else is "likely" or "unknown" and comes with an open question.

Already in tv-remote, so not proposed again: keys, long-press (`hold_key`), text into the system keyboard, touchpad (move/click), REST app status and close (`app_status`, `close_app` exist in core but have no UI), the known-app probe, DIAL YouTube (`v=`) and Netflix (empty body), DLNA cast with seek, volume and mute, Wake-on-LAN plus ST `switch.on`, REST `PowerState`, ST status, input source, picture and sound mode, channel up/down/number, multi-TV fleet with LAN/cloud routing, and OAuth with automatic refresh.

Example values in this document are fake: TV at `192.0.2.10`, device id `00000000-0000-4000-8000-000000000001`.

---

## 1. Executive summary: top 10 by value ÷ effort

| # | Feature | Surface | Effort | Why it ranks here |
|---|---|---|---|---|
| 1 | **SmartThings inspector**: `st capability <id>`, `st execute <href> [json]` with read-back, `st raw` | ST | S | Unlocks everything else on the ST side. It answers the open "Mode Image via `execute`" question in an afternoon, using the user's own token on their own TV. |
| 2 | **Search on the TV** through `custom.tvsearch` (and `execute /sec/tv/searchall`) | ST | S | The capability itself runs the TV's universal search with the string, so nothing has to reach the YouTube or Netflix keyboards. No key navigation and no typing into an app. If the capability doesn't search by itself on the Q7F, drop the idea; there's no keyboard fallback. |
| 3 | **Foreground app indicator + "close current app"** | LAN | S | REST `GET /api/v2/applications/{id}` already works on this TV and returns `running`/`visible`. The app already probes these ids, so this is mostly UI. |
| 4 | **Sleep timer and schedules** ("off at 00:30", "volume 10 at 22:00") with systemd user timers | LAN+ST | S | No new protocol. It reuses `on`/`off`/`volume`/`launch` with the existing LAN→cloud fallback. Doesn't need the Rules API, which may need an Enterprise account. |
| 5 | **Cloud remote keys** through `samsungvd.remoteControl.send(key, "PRESS_AND_RELEASED")` | ST | S | A remote that works when the WebSocket is down or you're away from home. Your TV reports the capability, and a 2026 integration uses it. It only sends the single key the user pressed in the remote UI, never scripted sequences. |
| 6 | **Picture and TV settings through `execute`** (`/sec/tv/mode/picture`, `/sec/tv/optioncontrol`, …) | ST | M | This is how the phone app most likely reaches Mode Image, Clarté de l'image and the other settings. High value, but payloads must be discovered first (that's what #1 is for). |
| 7 | **Netflix title and YouTube timestamp deep links** (DIAL `m=…&source_type=4`, `v=…&t=…`) | LAN | S | One-line changes to `dial.rs`. They also give "Cast to TV" from a browser URL. |
| 8 | **Subtitles in direct casts** (`sec:CaptionInfoEx`, the `CaptionInfo.sec` header, `X_ControlCaption`) | LAN | M | Today subtitles need TV-friendly mode, which loses seeking. Samsung's DMR side-loads SRT. |
| 9 | **Cast queue, photo slideshow and "why did it stop?"** (`SetNextAVTransportURI`, `X_SetTVSlideShow`, `X_GetStoppedReason`) | LAN | M | Folder casting, binge-watching episodes, and a real reason instead of `NeverStarted`. |
| 10 | **Pushed state instead of polling**: UPnP GENA on RenderingControl/AVTransport (LAN), SmartThings SSE subscription (cloud) | LAN+ST | M | Instant volume, mute, power and input updates in the GUI when they change on the TV or the phone. GENA is a standard. ST SSE is undocumented but used by openHAB. |

Honourable mentions, just below the cut: art mode on the Q7F (Samsung put the Art Store on 2025 QLEDs from Q7F up), an energy graph from `powerConsumptionReport`, "open URL in the TV browser", PVR record start/stop (`custom.recording`), and panel-hours and serial from `execute /sec/tv/deviceinfo`.

---

## 2. All ideas

Legend: **Off** = official or publicly documented API · **Undoc** = undocumented but used by open-source projects, no circumvention · **Priv** = private or reverse-engineered with ToS concerns. Feasibility on the Q7F: **confirmed** / **likely** / **unknown**.

| Feature | Surface | How it works | Off / Undoc | Q7F feasibility | Effort | Value | Risks / terms | Sources |
|---|---|---|---|---|---|---|---|---|
| ST capability inspector | ST | `GET /v1/capabilities/{id}/{version}` (the same call as the CLI's `smartthings capabilities <id> -j`) | Off | likely | S | High (enabler) | none; 12 req/min/device limit | [CLI tip](https://community.smartthings.com/t/samsung-tv-capabilities-samsungvd-ambient/268102), [rate limits](https://developer.smartthings.com/docs/getting-started/rate-limits) |
| `execute` explorer (query + set) | ST | `POST /devices/{id}/commands` `execute.execute(href[, json])`, then `GET …/components/main/capabilities/execute/status` → `data.value.payload` | Off endpoint, Undoc hrefs | likely (capability reported) | S | High (enabler) | `execute` is marked *deprecated*; may vanish | [gist](https://gist.github.com/c-kick/e15c233262e0050dd3f56a6070525862), [execute def](https://github.com/hongtat/smartthings-capabilities/blob/HEAD/json/execute.json) |
| Device facts from `/sec/tv/deviceinfo` (panel hours, year, serial, BT/Ethernet MAC, DUID, country) | ST | `execute("/sec/tv/deviceinfo")` → `x.com.samsung.tv.paneltime`, `.year`, `.sn`, `.ethmac`, `.duid`, … | Undoc | likely | S | Low–Med | the payload has the serial number: never log it | [pysmartthings fixture 2023](https://github.com/pySmartThings/pysmartthings/blob/HEAD/tests/fixtures/device_status/vd_stv_2023_1.json) |
| Picture mode through OCF | ST | `execute("/sec/tv/mode/picture", {"x.com.samsung.tv.currentMode": "<id>"})`, guessed from the input-source pattern | Undoc | unknown | M | High | may hit the same "accepted, ignored" behaviour | [OCF resource list](https://github.com/QuiteYellow/SmartThings-Local/blob/HEAD/docs/ocf-vd-devices.md), [SharpTools log](https://community.sharptools.io/t/samsung-smartthings-enabled-smarttv-commands/2524) |
| Extra settings (Clarté, Nuance coul., Espace couleur, Format audio, Mode Jeu, Format image, Veille, Sous-titres, Éco) | ST | probably `execute("/sec/tv/optioncontrol" or "/sec/tv/uicontrol", {...})`; discover with the explorer | Undoc | unknown | M–L | High | ST staff say what isn't a capability "can't be automated"; may need the phone plugin's context | [OCF list](https://github.com/QuiteYellow/SmartThings-Local/blob/HEAD/docs/ocf-vd-devices.md), [brightness thread](https://community.smartthings.com/t/tv-brightness-control/261457), [Q990F thread](https://community.smartthings.com/t/q990f-soundbar-can-night-mode-be-changed-via-the-api/306784) |
| Now-playing info (programme, thumbnail) | ST | `execute("/sec/tv/contentinfo")`, `/sec/tv/contentinfo/thumbnail` | Undoc | unknown | M | Med | — | [OCF list](https://github.com/QuiteYellow/SmartThings-Local/blob/HEAD/docs/ocf-vd-devices.md) |
| TV search | ST | `custom.tvsearch.search(<text>)`; alternative `execute("/sec/tv/searchall")`. Kept only if the capability performs the search itself; no variant that types into an app | Undoc (capability exists, args unknown) | likely | S | High | — | [pysmartthings commands](https://github.com/pySmartThings/pysmartthings/blob/HEAD/src/pysmartthings/command.py), [smartest-tv #8](https://github.com/Hybirdss/smartest-tv/issues/8) |
| Launch any app by name or id via cloud | ST | `custom.launchapp.launchApp(appId?, appName?)`; by name: `[null, "Netflix"]` | Undoc (used by integrations) | likely | S | Med | — | [ha_samsung_smartthings](https://github.com/joeblack2k/ha_samsung_smartthings/blob/HEAD/custom_components/samsung_smartthings/__init__.py) |
| Cloud remote keys | ST | `samsungvd.remoteControl.send(["HOME", "PRESS_AND_RELEASED"])`, one key per button the user presses; no sequences | Undoc (used by integrations) | likely | S | Med–High | key names beyond HOME not documented | [remote.py](https://github.com/joeblack2k/ha_samsung_smartthings/blob/HEAD/custom_components/samsung_smartthings/remote.py), [switch.py](https://github.com/joeblack2k/ha_samsung_smartthings/blob/HEAD/custom_components/samsung_smartthings/switch.py) |
| Cloud media keys | ST | `mediaPlayback.play/pause/stop/fastForward/rewind`, `mediaTrackControl.nextTrack/previousTrack` | Off (standard capabilities) | likely | S | Low–Med | `playbackStatus` often null | [pysmartthings](https://github.com/pySmartThings/pysmartthings/blob/HEAD/src/pysmartthings/command.py) |
| PVR record start/stop | ST | `custom.recording.recordStart/recordStop` | Undoc | unknown (tuner + USB only) | S | Low–Med | — | [pysmartthings](https://github.com/pySmartThings/pysmartthings/blob/HEAD/src/pysmartthings/command.py) |
| Accessibility shortcuts (e.g. "picture off") | ST | `custom.accessibility.setAccessibility(?)` | Undoc | unknown | S | Med (screen off with sound) | args unknown | [rules thread](https://community.smartthings.com/t/can-picture-mode-be-controlled-with-rules-api/283693) |
| Energy saving level | ST | `custom.energyType.setEnergySavingLevel(n)`; read `energySavingLevel`, `supportedEnergySavingLevels` | Undoc | unknown | S | Low–Med | — | [frame 2024 fixture](https://github.com/pySmartThings/pysmartthings/blob/HEAD/tests/fixtures/device_status/vd_frame_2024.json) |
| Energy graph | ST | `powerConsumptionReport.powerConsumption` {energy, deltaEnergy, power, start, end}, polled or read from `/history/devices` | Off attr, Undoc history | likely | M | Med | 15-min granularity | [fixture 2023](https://github.com/pySmartThings/pysmartthings/blob/HEAD/tests/fixtures/device_status/vd_stv_2023_1.json), [history](https://community.smartthings.com/t/totally-lost-trying-to-get-historic-events/232030) |
| Event log ("who turned the TV on, from where") | ST | `GET /history/devices?locationId=…&deviceId=…` | Undoc (used by CLI `locations:history`) | likely | M | Med (also a debugging aid) | delayed, not real-time | [community](https://community.smartthings.com/t/smartthings-history-via-api/276392) |
| Live cloud events without a server | ST | `POST /subscriptions` {DEVICEIDS filter} → `registrationUrl` → SSE stream of `DEVICE_EVENT` | Undoc | unknown with OAuth (works with PAT in openHAB) | M | Med | may 403; access "may be restricted" | [openHAB](https://github.com/openhab/openhab-addons/blob/main/bundles/org.openhab.binding.samsungtv/src/main/java/org/openhab/binding/samsungtv/internal/service/SmartThingsApiService.java), [403 thread](https://community.smartthings.com/t/403-on-device-subscriptions/207733) |
| Webhook subscriptions | ST | `installedapps/{id}/subscriptions` → POST to your public URL | Off | n/a (needs a public HTTPS server) | L | Low | needs hosting | [subscribe docs](https://developer.smartthings.com/docs/service-integrations/subscribe-to-events) |
| Automations in the cloud (Rules API) | ST | `POST /rules?locationId=` with `every.specific` + `command` | Off | unknown (scopes `r/w:rules:*` may need Enterprise) | M | Med | the app would need re-registration with new scopes | [rules docs](https://developer.smartthings.com/docs/automations/rules), [scopes thread](https://community.smartthings.com/t/oauth-in-app-and-scopes/309234) |
| Run SmartThings scenes ("Cinéma": lights + TV) | ST | `GET /scenes`, `POST /scenes/{id}/execute` (scopes `r:scenes:*`, `x:scenes:*`) | Off | likely | S | Low–Med | re-register the app with new scopes | [rate limits](https://developer.smartthings.com/docs/getting-started/rate-limits), [scopes thread](https://community.smartthings.com/t/oauth-in-app-and-scopes/309234) |
| Ambient / art mode via cloud | ST | `samsungvd.ambient.setAmbientOn`, `samsungvd.art.setArtOn`, `samsungvd.ambientContent.setAmbientContent(json)` | Undoc | unknown (Q7F reports `samsungvd.ambient`, `.art`) | S | Med | Frames often return 422 even with the capability | [device.py](https://github.com/joeblack2k/ha_samsung_smartthings/blob/HEAD/custom_components/samsung_smartthings/device.py) |
| Samsung TV Plus launch | ST | `samsungvd.launchService.launchTVPlus` (args unknown) | Undoc | unknown | S | Low | channel deep links: none known | [pysmartthings](https://github.com/pySmartThings/pysmartthings/blob/HEAD/src/pysmartthings/command.py) |
| Supported-features panel | ST | `samsungvd.supportsFeatures`: `artSupported`, `imeAdvSupported`, `mediaOutputSupported`, `mobileCamSupported`, `remotelessSupported`, `executableServiceList`, `wifiUpdateSupport` | Undoc | likely | S | Low | — | [attribute.py](https://github.com/pySmartThings/pysmartthings/blob/HEAD/src/pysmartthings/attribute.py) |
| Foreground app + close it | LAN | poll `GET http://tv:8001/api/v2/applications/{id}` → `{running, visible}`; `DELETE` closes | Undoc (widely used) | confirmed (REST probe works) | S | Med–High | ~25 requests per poll; poll slowly | [tizenws](https://github.com/caphm/tizenws), [samsungtvws rest.py](https://github.com/xchwarze/samsung-tv-ws-api/blob/HEAD/samsungtvws/rest.py) |
| Open the store page of an app | LAN | `PUT /api/v2/applications/{id}` | Undoc | unknown | S | Low | — | [samsungtvws rest.py](https://github.com/xchwarze/samsung-tv-ws-api/blob/HEAD/samsungtvws/rest.py) |
| Open a URL in the TV browser | LAN | `ed.apps.launch` {appId `org.tizen.browser`, `NATIVE_LAUNCH`, metaTag=url}. Direct metaTag only; the "launch the browser, then type the URL" fallback was dropped | Undoc | likely | S | Med | URL may not be filled on some models; then the feature just isn't offered | [samsungtvws remote.py](https://github.com/xchwarze/samsung-tv-ws-api/blob/HEAD/samsungtvws/remote.py), [openHAB url channel](https://www.openhab.org/addons/bindings/samsungtv) |
| Netflix title deep link | LAN | DIAL `POST :8080/ws/apps/Netflix` body `m=https://www.netflix.com/watch/<id>&source_type=4` | Undoc (DIAL is standard; body is Netflix's) | unknown (Tizen 9 reports mixed) | S | Med | — | [smartest-tv tests](https://github.com/Hybirdss/smartest-tv/pull/22) |
| YouTube start time / playlist | LAN | DIAL body `v=<id>&t=<secs>` (and `list=`) | Undoc | likely | S | Low–Med | — | [smartest-tv #8](https://github.com/Hybirdss/smartest-tv/issues/8) |
| Websocket deep links (Disney+, Prime…) | LAN | `ed.apps.launch` `DEEP_LINK` metaTag=<content id> | Undoc | unknown (often ignored on Tizen 9) | S | Low | — | [smartest-tv #8](https://github.com/Hybirdss/smartest-tv/issues/8) |
| Password / IME through REST | LAN | `POST /api/v2/remoteControl/imeInput/<base64>?token=<token>`, an alternative way to send text to the field the user has already focused (like today's text feature). Not used to reach or type into app search boxes | Undoc (added to samsungtvws Feb 2026) | unknown | S | Low–Med | — | [samsungtvws rest.py](https://github.com/xchwarze/samsung-tv-ws-api/blob/HEAD/samsungtvws/rest.py) |
| `custom.remote.textReceived` preamble | LAN | broadcast `ms.channel.emit {event:"custom.remote.textReceived", to:"broadcast"}` before the first `SendInputString` | Undoc | unknown | S | Low–Med | — | [samsungtvws remote.py](https://github.com/xchwarze/samsung-tv-ws-api/blob/HEAD/samsungtvws/remote.py) |
| Read back the TV's text field | LAN | log `ms.remote.imeStart` / `imeUpdate` / `imeEnd` `data` (likely base64) | Undoc | unknown | S | Med | — | — (no public documentation found) |
| Mouse right-click and moves with a duration | LAN | `ProcessMouseDevice` `RightClick`; `Move` with `Time` | Undoc | likely | S | Low | — | [openHAB keyCode](https://www.openhab.org/addons/bindings/samsungtv), [samsungtvws](https://github.com/xchwarze/samsung-tv-ws-api/blob/HEAD/samsungtvws/remote.py) |
| Art mode / own photos as art | LAN | `wss://tv:8002/api/v2/channels/com.samsung.art-app`, `art_app_request` (`get_artmode_status`, `set_artmode_status`, `send_image`, `select_image`, `set_slideshow_status`, brightness…) | Undoc | unknown (Q7F is in the 2025 Art TV group) | M | Med | — | [samsungtvws art.py](https://github.com/xchwarze/samsung-tv-ws-api/blob/HEAD/samsungtvws/art/art.py), [SamMobile](https://www.sammobile.com/news/samsung-famous-art-store-now-available-on-more-tv-models/) |
| DLNA subtitles | LAN | DIDL `<sec:CaptionInfoEx sec:type="srt">URL</sec:CaptionInfoEx>` + HTTP header `CaptionInfo.sec: URL`; or `X_ControlCaption` | Undoc (Samsung vendor) | likely | M | Med–High | — | [minidlna](https://github.com/azatoth/minidlna/blob/HEAD/upnpsoap.c), [Samsung SCPD](https://github.com/azizulhakim/u-pot/tree/HEAD/device-descriptors/Samsung%20SmartTV) |
| Pick the audio track without remuxing | LAN | `RenderingControl X_UpdateAudioSelection(AudioPID, AudioEncoding)` / `X_GetAudioSelection` | Undoc | unknown | M | High if it works (seekable + right language) | — | [SCPD](https://github.com/azizulhakim/u-pot/tree/HEAD/device-descriptors/Samsung%20SmartTV) |
| Cast queue / gapless next | LAN | `AVTransport SetNextAVTransportURI`, `GetMediaInfo` (`NextURI`) | Off (UPnP AV) | likely | M | Med | — | [SCPD](https://github.com/azizulhakim/u-pot/tree/HEAD/device-descriptors/Samsung%20SmartTV) |
| Photo slideshow | LAN | image items + `X_SetTVSlideShow(CurrentShowState, CurrentShowTheme)`, `X_GetTVSlideShow` | Undoc | unknown | M | Med | — | [SCPD](https://github.com/azizulhakim/u-pot/tree/HEAD/device-descriptors/Samsung%20SmartTV) |
| Why playback stopped | LAN | `AVTransport X_GetStoppedReason` → `StoppedReason`, `StoppedReasonData` | Undoc | likely | S | Med | — | [SCPD](https://github.com/azizulhakim/u-pot/tree/HEAD/device-descriptors/Samsung%20SmartTV) |
| Aspect ratio / zoom for casts | LAN | `X_SetAspectRatio(Default\|FitScreen)`, `X_SetZoom(x,y,w,h)` | Undoc | unknown | S | Low–Med | — | [SCPD](https://github.com/azizulhakim/u-pot/tree/HEAD/device-descriptors/Samsung%20SmartTV) |
| 360° video view control (DLNA actions, not keys) | LAN | `X_Move360View`, `X_Zoom360View`, `X_Origin360View` | Undoc | unknown | S | Low | — | [SCPD](https://github.com/azizulhakim/u-pot/tree/HEAD/device-descriptors/Samsung%20SmartTV) |
| Prefetch next item | LAN | `X_PrefetchURI` | Undoc | unknown | S | Low | — | [SCPD](https://github.com/azizulhakim/u-pot/tree/HEAD/device-descriptors/Samsung%20SmartTV) |
| Pushed volume, mute and transport state | LAN | UPnP GENA `SUBSCRIBE` to RenderingControl/AVTransport event URLs; `NOTIFY` with `LastChange` | Off (UPnP) | likely | M | Med | needs a callback port (the cast server exists) | [HA media_player](https://github.com/home-assistant/core/blob/dev/homeassistant/components/samsungtv/media_player.py) |
| YouTube app control (queue, seek, now playing) | LAN+Google | DIAL `screenId` → YouTube Lounge API | Priv (Google, undocumented) | likely | L | Med | Google ToS grey zone; codes get revoked | [iSponsorBlockTV](https://github.com/dmunozv04/iSponsorBlockTV), [pyytlounge](https://pyytlounge.readthedocs.io/en/stable/usage.html) |
| Desktop mirroring | LAN | Miracast with GNOME Network Displays (external tool) | Off (Wi-Fi Alliance) | unknown (Samsung reports mixed) | M | Med | — | [Phoronix](https://www.phoronix.com/news/GNOME-Network-Displays-0.91), [Fedora thread](https://discussion.fedoraproject.org/t/how-do-i-screencast-from-a-fedora-41-laptop-to-a-samsung-smart-tv/147724) |
| WoL hardening | LAN | magic packet to the subnet broadcast *and* the TV IP, ports 7 and 9; document "Power On with Mobile" | Off | confirmed (exists, small improvement) | S | Low–Med | — | [Samsung AU](https://www.samsung.com/au/support/tv-audio-video/power-on-tv-smartthings-app/), [Samsung IP control](https://image-us.samsung.com/SamsungUS/samsungbusiness/tv-ci-resources/Samsung-IP-Control.pdf) |
| Sleep timer / schedules | LAN+ST | systemd `--user` timers calling `tv-remote-tui off` etc. | — | confirmed | S | Med–High | — | (no new protocol) |
| Firmware version and update hint | ST/Account | `samsungvd.firmwareVersion`, `supportsFeatures.wifiUpdateSupport`; link to Samsung's support page | Off attr | likely | S | Low | no API for "update available" | [attribute.py](https://github.com/pySmartThings/pysmartthings/blob/HEAD/src/pysmartthings/attribute.py) |
| Gaming Hub, TV Plus, other Samsung apps | Account | launch only (already in `KNOWN_APPS` or via `custom.launchapp`) | Off-ish | confirmed (launch) | S | Low | no content APIs | [homebridge app ids](https://tavicu.github.io/homebridge-samsung-tizen/extra/applications.html) |

---

## 3. LAN details

### 3.1 REST (`http://192.0.2.10:8001/api/v2/`)

- `GET /api/v2/applications/{appId}` returns `{"id", "name", "running": bool, "visible": bool, "version"}`. `POST` launches, `DELETE` closes, and `PUT` opens the app's install page ([samsungtvws `rest.py`](https://github.com/xchwarze/samsung-tv-ws-api/blob/HEAD/samsungtvws/rest.py)).
  - **Foreground app**: core already has `app_status(id)`. A poller over `KNOWN_APPS` every 5 to 10 s (only while the window is visible) can show "Netflix is on screen" in the header, offer a **Close** button (`close_app`), and light up the app tile. tizenws does the same over the `/api/v2` WebSocket with `ms.application.get` / `ms.webapplication.get` and `{running, visible}` ([tizenws](https://github.com/caphm/tizenws)).
  - Open question: does `DELETE` close Netflix and YouTube on Tizen 2025, or only web apps?
- `POST /api/v2/remoteControl/imeInput/<base64(text)>?token=<pairing token>`: added to samsungtvws in Feb 2026 as `send_password` ([commit "add text support to remote control"](https://github.com/xchwarze/samsung-tv-ws-api/blob/HEAD/samsungtvws/rest.py)). **Open question:** is it more reliable than `SendInputString` for the field the user has focused (for example password fields)? It's scoped to the same use as today's text feature: the user brings up the field, and the app never navigates to a search box.
- `GET /api/v2/` `device.isSupport` and the device fields: `FrameTVSupport`, `ImeSyncedSupport`, `TokenAuthSupport`, `VoiceSupport`, `GamePadSupport`. samsungtvws decides art support from `FrameTVSupport == "true"` ([art.py](https://github.com/xchwarze/samsung-tv-ws-api/blob/HEAD/samsungtvws/art/art.py)). **Open question:** what does the Q7F report for `FrameTVSupport`? (`tv-remote-tui info --json` should show it if the raw JSON is printed.)

### 3.2 Remote WebSocket (`wss://192.0.2.10:8002/api/v2/channels/samsung.remote.control`)

- `ms.remote.control` with `TypeOfRemote: "SendRemoteKey"`, `Cmd: "Click" | "Press" | "Release"`, `DataOfCmd: "KEY_…"`, `Option: "false"`. Press/Release already exists (`hold_key`). Nothing new is proposed here: keys are only sent one at a time, when the user presses them (see "Removed: key-navigation features").
- `TypeOfRemote: "ProcessMouseDevice"`: `Cmd: "Move"` with `Position: {x, y, Time}`, `"LeftClick"`, `"RightClick"` ([samsungtvws](https://github.com/xchwarze/samsung-tv-ws-api/blob/HEAD/samsungtvws/remote.py), [openHAB](https://www.openhab.org/addons/bindings/samsungtv)). There is no documented scroll command. Open question: does a `Move` with large `y` while a button is held scroll in the browser?
- Text: samsungtvws sends `{"method":"ms.channel.emit","params":{"event":"custom.remote.textReceived","to":"broadcast"}}` once before the first `SendInputString`, with the comment "Some TVs require this broadcast before accepting text" ([remote.py](https://github.com/xchwarze/samsung-tv-ws-api/blob/HEAD/samsungtvws/remote.py)). Cheap to try.
- Events: samsungtvws knows `ed.apps.launch`, `ed.edenTV.update`, `ed.installedApp.get`, `ms.channel.*`, `ms.error`, `ms.voiceApp.hide`, `ms.remote.imeStart`, `ms.remote.imeEnd` ([event.py](https://github.com/xchwarze/samsung-tv-ws-api/blob/HEAD/samsungtvws/event.py)). I found no public documentation of the `data` field of `imeStart`/`imeUpdate`. **Open question:** log the raw `RemoteEvent.data` once. If it carries the field's current text (base64 on other Samsung channels), the Keyboard page can pre-fill and live-mirror the TV's text field. `ed.edenTV.update` may signal home-screen and app changes, which could replace polling for the foreground app.
- `ed.apps.icon` (`{"event":"ed.apps.icon","to":"host","data":{"iconPath": …}}`) returns an app icon as base64 ([samsung-tv-control](https://github.com/Toxblh/samsung-tv-control)). It needs `iconPath` from `ed.installedApp.get`, which this TV ignores, so it has **no use here**.
- Open a web page: `ed.apps.launch` with `appId: "org.tizen.browser"`, `action_type: "NATIVE_LAUNCH"`, `metaTag: "<url>"` ([samsungtvws `open_browser`](https://github.com/xchwarze/samsung-tv-ws-api/blob/HEAD/samsungtvws/remote.py)). openHAB warns that on some post-2016 TVs the URL isn't filled. No fallback: if the metaTag isn't honoured on the Q7F, don't offer the feature (a "launch, then type the URL" fallback was removed).

### 3.3 DIAL (`http://192.0.2.10:8080/ws/apps/<App>`)

- **Netflix title**: body `m=https://www.netflix.com/watch/<titleId>&source_type=4` ([smartest-tv test](https://github.com/Hybirdss/smartest-tv/pull/22)). Today `dial.rs` sends an empty body. Accept a pasted `netflix.com/title/<id>` or `/watch/<id>` URL, the same way YouTube URLs are parsed.
- **YouTube**: `v=<id>` already; add `&t=<seconds>` from `?t=` in the pasted URL (likely, not verified on Tizen 2025).
- `GET /ws/apps/YouTube` returns `<additionalData>` with a `screenId`. That's the entry point to the YouTube Lounge API (see "Not recommended").
- WebSocket `DEEP_LINK` metaTags for other apps are reported as ignored or app-only on Tizen 9 ([smartest-tv #8](https://github.com/Hybirdss/smartest-tv/issues/8)). Low priority.

### 3.4 DLNA renderer (`http://192.0.2.10:9197/dmr`)

The published SCPDs of a Samsung Tizen DMR (2017 UN49M5300, `sec:ProductCap` `Tizen,Y2017,WebURIPlayable,NavigateInPause,…`) list these actions ([u-pot device descriptors](https://github.com/azizulhakim/u-pot/tree/HEAD/device-descriptors/Samsung%20SmartTV)). The 2025 TV may differ, so the first step is to read its own `/dmr` and SCPD files (the app may; this research didn't).

AVTransport: `Play, Stop, Pause, Seek (Unit: REL_TIME|ABS_TIME|X_DLNA_REL_BYTE|…), Next, Previous, SetPlayMode (NORMAL only), GetMediaInfo, GetDeviceCapabilities, SetAVTransportURI, SetNextAVTransportURI, GetTransportSettings, GetTransportInfo, GetPositionInfo, GetCurrentTransportActions, X_DLNA_GetBytePositionInfo, X_PrefetchURI, X_GetStoppedReason, X_PlayerAppHint(UpnpClass: image|audio|video, PlayerHint: load|unload)`.

RenderingControl: `Get/SetVolume, Get/SetMute, ListPresets, SelectPreset, X_GetAspectRatio/X_SetAspectRatio (Default|FitScreen), X_Move360View, X_Zoom360View, X_Origin360View, X_ControlCaption(Operation: Enable|Disable, Name, ResourceURI, CaptionURI, CaptionType, Language, Encoding), X_GetCaptionState, X_GetServiceCapabilities, X_UpdateAudioSelection(AudioPID, AudioEncoding), X_GetAudioSelection, X_UpdateVideoSelection, X_GetVideoSelection, X_SetZoom(x,y,w,h), X_GetTVSlideShow, X_SetTVSlideShow(CurrentShowState, CurrentShowTheme)`. There's also a `StreamSplicing` service (rvualliance) that isn't useful here.

Ideas:

1. **Subtitles in direct mode.** Serve the SRT (converted with ffmpeg if the source is ASS or embedded) from the existing cast HTTP server, and:
   - add to the DIDL item `<sec:CaptionInfoEx sec:type="srt">http://192.0.2.20:PORT/sub.srt</sec:CaptionInfoEx>` (namespace `xmlns:sec="http://www.sec.co.kr/"`), and
   - when the TV's GET carries `getCaptionInfo.sec: 1`, answer with the header `CaptionInfo.sec: http://…/sub.srt`.

   Both are what minidlna does for Samsung ([upnpsoap.c](https://github.com/azatoth/minidlna/blob/HEAD/upnpsoap.c), [upnphttp.c](https://github.com/azatoth/minidlna/blob/HEAD/upnphttp.c)). The minidlna commit notes that Series J dropped the old header and needs `CaptionInfoEx`. Alternative: `X_ControlCaption(Enable, …, CaptionURI, "srt", "fr", "UTF-8")` during playback. **Open questions:** which one the 2025 TV honours, and whether the user can then toggle subtitles with the TV's own menu.
2. **Audio track without remux:** `X_GetAudioSelection` / `X_UpdateAudioSelection`. If the TV maps `AudioPID` to an MKV track, direct mode could pick the French track and stay seekable. DTS still won't play (no DTS on this TV). **Open question:** the PID semantics for MKV and MP4.
3. **Queue and folders:** `SetNextAVTransportURI` for the next episode or song; `GetMediaInfo` shows `NextURI`. "Cast this folder" and "Play next episode automatically" become possible. **Open question:** does the 2025 DMR advance automatically (AVTransport says it should)?
4. **Photo slideshow:** cast image items with `X_PlayerAppHint(object.item.imageItem, load)`, then `X_SetTVSlideShow` for the TV's built-in transitions. Otherwise the app advances `SetAVTransportURI` itself every N seconds.
5. **Diagnosis:** on `NeverStarted` or a stall, call `X_GetStoppedReason` and show `StoppedReason`/`StoppedReasonData` (for example an unsupported codec) instead of a guess.
6. **Pushed state:** `SUBSCRIBE` to the RenderingControl and AVTransport `eventSubURL` with a callback to a small HTTP listener on the PC. Home Assistant core does this with `async_upnp_client` `DmrDevice.async_subscribe_services(auto_resubscribe=True)` and tolerates TVs that reject it ([HA media_player](https://github.com/home-assistant/core/blob/dev/homeassistant/components/samsungtv/media_player.py)). This gives instant volume and mute in the GUI even when the physical remote changes them, and cast position without 1 s polling.

### 3.5 Art mode channel

`wss://192.0.2.10:8002/api/v2/channels/com.samsung.art-app?name=<base64>&token=<token>`, messages `ms.channel.emit` with `event: "art_app_request"` and `data` a JSON string `{"request": "get_artmode_status" | "set_artmode_status" | "get_content_list" | "select_image" | "send_image" | "set_slideshow_status" | "get_brightness" | "set_brightness" | …, "id": "<uuid>"}`. Uploads go through a separate D2D TCP socket ([samsungtvws art.py](https://github.com/xchwarze/samsung-tv-ws-api/blob/HEAD/samsungtvws/art/art.py)). The openHAB binding says the art API "was removed in 2022, then restored in firmware 1622 for >2021 Frame TVs" and warns not to link art channels on non-Frame TVs because they confuse power handling ([openHAB](https://www.openhab.org/addons/bindings/samsungtv)).

**Why it may matter for the Q7F:** Samsung expanded the Art Store to 2025 Neo QLED and QLED models, and its South Africa note puts "QLED models starting from Q7F" in the "Art TV" group ([SamMobile](https://www.sammobile.com/news/samsung-famous-art-store-now-available-on-more-tv-models/), [CEPro](https://www.cepro.com/news/samsung-expands-art-store-to-additional-tvs/617346)). The TV also reports `samsungvd.art` and `samsungvd.ambient` through SmartThings. **Open questions:** does `FrameTVSupport` say true? Does `get_api_version` answer on the art channel? A connection attempt costs one approval prompt on the TV.

Feature if it works: "Art" page with art mode on/off, show my photo (upload from the PC), slideshow, art brightness.

### 3.6 Wake-on-LAN and power

Already handled: WoL every 2 s plus ST `switch.on`, and REST `PowerState`. Small hardening: also send the packet unicast to the TV's last IP and to UDP 7, and document Samsung's setting path **Settings › Connections › Network › Expert Settings › Power On with Mobile** ([Samsung AU support](https://www.samsung.com/au/support/tv-audio-video/power-on-tv-smartthings-app/)). Samsung's IP-control guide says the TV keeps its IP server for about a minute after power-off, then needs WoL ([Samsung IP Control PDF](https://image-us.samsung.com/SamsungUS/samsungbusiness/tv-ci-resources/Samsung-IP-Control.pdf)). openHAB adds that WoL fails on Ethernet with a soundbar on ARC ([openHAB](https://www.openhab.org/addons/bindings/samsungtv)), which is worth a line in Troubleshooting.

### 3.7 Screen mirroring

Samsung TVs are Miracast sinks (Smart View or Screen Mirroring). On Linux the practical option is GNOME Network Displays, which since 0.91 also does MICE (Miracast over LAN) and Chromecast ([Phoronix](https://www.phoronix.com/news/GNOME-Network-Displays-0.91)). Samsung success reports are mixed ([Fedora thread](https://discussion.fedoraproject.org/t/how-do-i-screencast-from-a-fedora-41-laptop-to-a-samsung-smart-tv/147724)). Proposal: a "Mirror my screen" button that launches `gnome-network-displays` if installed, plus a docs page. Implementing Miracast in tv-remote isn't worth it.

---

## 4. SmartThings details

### 4.1 The `execute` capability: how the phone app reaches more settings

Definition (status **deprecated**, but still present on every Samsung TV profile): command `execute(command: string ≤255, args?: object)`, attribute `data` ([execute.json](https://github.com/hongtat/smartthings-capabilities/blob/HEAD/json/execute.json)).

**Pattern, used in production for soundbars** ([gist](https://gist.github.com/c-kick/e15c233262e0050dd3f56a6070525862), [ha_samsung_smartthings device.py](https://github.com/joeblack2k/ha_samsung_smartthings/blob/HEAD/custom_components/samsung_smartthings/device.py)):

```json
POST https://api.smartthings.com/v1/devices/00000000-0000-4000-8000-000000000001/commands
{"commands":[{"component":"main","capability":"execute","command":"execute",
  "arguments":["/sec/networkaudio/advancedaudio"]}]}
```
Wait about 0.5 to 1 s, then:
```
GET /v1/devices/{id}/components/main/capabilities/execute/status   → data.value.payload
```
Write with the same href plus an object: `"arguments":["/sec/networkaudio/advancedaudio",{"x.com.samsung.networkaudio.nightmode":1}]`.

**The TV's resource tree.** A 2020 TV (`VD-STV_2018_K`, Tizen 5.5) lists these OCF hrefs in its `/oic/res` ([SmartThings-Local, ocf-vd-devices.md](https://github.com/QuiteYellow/SmartThings-Local/blob/HEAD/docs/ocf-vd-devices.md)). The `execute` capability is the cloud's way to reach them:

| href | rt | Likely use |
|---|---|---|
| `/sec/tv/deviceinfo` | `x.com.samsung.tv.deviceinfo` | identity, panel hours (read: confirmed in public fixtures) |
| `/sec/tv/mode/picture` | `x.com.samsung.tv.mode` | **picture mode** |
| `/sec/tv/mode/sound` | `x.com.samsung.tv.mode` | sound mode |
| `/sec/tv/inputsource` | `x.com.samsung.tv.mode` | input; known write `{"x.com.samsung.tv.currentMode":"dtv"}` ([SharpTools log](https://community.sharptools.io/t/samsung-smartthings-enabled-smarttv-commands/2524)) |
| `/sec/tv/optioncontrol` | `x.com.samsung.tv.optioncontrol` | **best candidate for the extra settings** (energy, sleep, picture options) |
| `/sec/tv/uicontrol` | `x.com.samsung.tv.uicontrol` | UI and settings toggles |
| `/sec/tv/contentinfo`, `/sec/tv/contentinfo/thumbnail` | `x.com.samsung.tv.contentinfo` | what's on screen, with a thumbnail |
| `/sec/tv/channel` | `x.com.samsung.tv.channel` | channel |
| `/sec/tv/remotecontrol` | `x.com.samsung.tv.remotecontrol` | keys (the `samsungvd.remoteControl` backend?) |
| `/sec/tv/ime` | `x.com.samsung.tv.ime` | text entry from the cloud |
| `/sec/tv/searchall`, `/sec/tv/searchonweb` | — | search |
| `/sec/tv/launchapp`, `/sec/tv/launchnativeapp` | — | app launch |
| `/sec/tv/showmenu` | — | open a menu page |
| `/sec/tv/audio` | `oic.r.audio` | volume |
| `/sec/tv/appdata/*` (ambientapp, contentsapp, …) | `x.com.samsung.tv.appdata` | app-specific data |

Public status fixtures of 2022 and 2023 TVs show `execute.data.value.payload` with `rt: ["x.com.samsung.tv.deviceinfo"]` and keys `x.com.samsung.tv.paneltime` (5 and 197 in two fixtures, probably panel-on hours), `.year`, `.swmodel`, `.sn`, `.duid`, `.udn`, `.wifimac`, `.ethmac`, `.btmac`, `.countrycode`, `.language`, `.category` (`tv`/`frametv`), `.onsupport`, `.ambientsupport` ([vd_stv_2023_1.json](https://github.com/pySmartThings/pysmartthings/blob/HEAD/tests/fixtures/device_status/vd_stv_2023_1.json), [vd_stv_2022.json](https://github.com/pySmartThings/pysmartthings/blob/HEAD/tests/fixtures/device_status/vd_stv_2022.json)). So the cloud does forward `execute` reads to TVs, not only to soundbars.

**Suggested discovery procedure** (the "ST inspector", idea #1). It uses only the user's own account and TV, through the official endpoint:

1. `tv-remote-tui st execute /sec/tv/deviceinfo`: sanity check (prints the payload; redact `sn`, MACs and `duid` by default).
2. `st execute /sec/tv/mode/picture`: read. Expect something like `{"x.com.samsung.tv.supportedModes": [...], "x.com.samsung.tv.currentMode": "..."}`.
3. Write `st execute /sec/tv/mode/picture '{"x.com.samsung.tv.currentMode":"<id>"}'` with the TV on and the TV's own Mode Image menu not greyed out. (It was greyed out during the earlier test, which may explain why `samsungvd.pictureMode` was "ignored": the TV may lock picture mode in some states, such as Game Mode or certain HDR or AI modes.)
4. Read `/sec/tv/optioncontrol` and `/sec/tv/uicontrol` with the TV on, before and after changing *Clarté de l'image* or *Veille* in the phone app, and diff the payloads. This is the method recommended on the SmartThings forum: "check the execute status after changing an option in the SmartThings app" ([TV brightness thread](https://community.smartthings.com/t/tv-brightness-control/261457)).
5. Cross-check with `GET /history/devices?locationId=…&deviceId=…` to see which events the phone app's change produced (§4.4).

Caveats: a SmartThings staff member said settings not exposed as capabilities "cannot be automated, even through the API" ([Q990F thread](https://community.smartthings.com/t/q990f-soundbar-can-night-mode-be-changed-via-the-api/306784)), and another noted that OCF commands "work on the plugin level (app), not the API level, so not all commands will work as expected" ([TV brightness thread](https://community.smartthings.com/t/tv-brightness-control/261457)). Some soundbars accept writes but return empty read-backs ([Developer Reference](https://github.com/joeblack2k/ha_samsung_smartthings/blob/HEAD/wiki/Developer-Reference.md)). Treat each setting as unknown until tested. Ship the inspector first, and only build UI for settings proven to apply.

### 4.2 Capabilities reported by the Q7F

Commands and attributes from the pysmartthings capability tables ([command.py](https://github.com/pySmartThings/pysmartthings/blob/HEAD/src/pysmartthings/command.py), [attribute.py](https://github.com/pySmartThings/pysmartthings/blob/HEAD/src/pysmartthings/attribute.py)) and fixtures. Argument schemas for the `custom.*` and `samsungvd.*` ones aren't public. Get them with `npx @smartthings/cli capabilities <id> 1 -j` or with idea #1.

| Capability | Commands | Attributes | Already used | Idea |
|---|---|---|---|---|
| switch, audioVolume, audioMute | on/off, setVolume, volumeUp/Down, mute/unmute | switch, volume, mute | yes | — |
| tvChannel | channelUp/Down, setTvChannel, setTvChannelName | tvChannel, tvChannelName | yes | show channel name if filled |
| mediaInputSource, samsungvd.mediaInputSource | setInputSource | inputSource, supportedInputSourcesMap | yes | — |
| samsungvd.pictureMode / soundMode | setPictureMode / setSoundMode | pictureMode, supported…Map | yes (ignored on Q7F) | retry via execute (§4.1) |
| mediaPlayback | play, pause, stop, fastForward, rewind, setPlaybackStatus | playbackStatus, supportedPlaybackCommands | no | cloud media keys |
| mediaTrackControl | nextTrack, previousTrack | supportedTrackControlCommands | no | cloud next/previous |
| custom.launchapp | launchApp(appId?, appName?) | — | no | cloud launch by name: `[null, "YouTube"]` sends only a name ([`__init__.py`](https://github.com/joeblack2k/ha_samsung_smartthings/blob/HEAD/custom_components/samsung_smartthings/__init__.py)) |
| custom.tvsearch | search | — | no | **search on TV** |
| custom.recording | recordStart, recordStop | — | no | PVR buttons when the input is TV |
| custom.accessibility | setAccessibility | — | no | probe args; maybe picture-off |
| samsungvd.remoteControl | send(keyValue, keyState) | — | no | **cloud remote**; `"PRESS_AND_RELEASED"` confirmed, also `"PRESSED"`/`"RELEASED"` likely (the HA integration accepts `KEY:STATE`) ([remote.py](https://github.com/joeblack2k/ha_samsung_smartthings/blob/HEAD/custom_components/samsung_smartthings/remote.py)) |
| samsungvd.ambient | setAmbientOn, sendData | info | no | ambient or art on; Frames often answer 422 |
| samsungvd.ambientContent | setAmbientContent(json) | supportedAmbientApps (e.g. `["weather"]`) | no | low priority |
| samsungvd.art | setArtOn | — | no | art mode via cloud |
| samsungvd.launchService | launchTVPlus | — | no | TV Plus button |
| samsungvd.lightControl | setLightControlMode | supportedModes, selectedMode, streamControl, … | no | ambient light sync; low value |
| samsungvd.supportsFeatures | — | artSupported, imeAdvSupported, mediaOutputSupported, mobileCamSupported, remotelessSupported, executableServiceList, wifiUpdateSupport | no | drive the UI (show Art page only if `artSupported`) |
| samsungvd.thingStatus | — | status (`Idle`, …), updatedTime | no | extra state hint |
| samsungvd.firmwareVersion | setFirmwareVersion | firmwareVersion | partly (ocf `mnfv`) | — |
| samsungvd.supportsPowerOnByOcf | — | supportsPowerOnByOcf | used for power on | — |
| samsungvd.deviceCategory | — | category | no | — |
| powerConsumptionReport | — | powerConsumption {energy, deltaEnergy, power, powerEnergy, energySaved, start, end} | no | **energy graph** |
| custom.energyType | setEnergySavingLevel | energyType, energySavingLevel, supportedEnergySavingLevels, energySavingOperation, energySavingSupport, drMaxDuration | no | maybe the phone's "Solution d'économie d'énergie" |
| demandResponseLoadControl | requestDrlcAction, overrideDrlcAction | drlcStatus {drlcType, drlcLevel, start, duration, override} | no | utility demand-response; not useful |
| custom.error, custom.disabledCapabilities | setError / — | error / disabledCapabilities | no | hide controls listed in `disabledCapabilities` |
| samsungim.fixedFindNode | refresh | — | no | the TV relays SmartThings Find; **no relevance** |
| ocf, refresh, execute | refresh / execute | ocf fields / data | ocf yes | `refresh` before status reads; execute (§4.1) |

**Cloud remote, payload:**
```json
{"commands":[{"component":"main","capability":"samsungvd.remoteControl","command":"send",
  "arguments":["HOME","PRESS_AND_RELEASED"]}]}
```
Open questions: the full `keyValue` enum (read it with the capability inspector; the generic `keypadInput` capability uses `UP, DOWN, LEFT, RIGHT, SELECT, BACK, EXIT, MENU, SETTINGS, HOME, NUMBER0-9` ([forum](https://community.smartthings.com/t/keypadinput-sendkey-command-is-not-working-via-the-smartthings-api-for-samsung-smart-tv/295203)), and `samsungvd.remoteControl` probably has its own list). Also the latency, given the 12 commands/min/device limit: fine for a remote "away" mode, not for fast use. Scope: one key per button the user presses; never scripted sequences.

### 4.3 Events, push and history

- **History:** `GET https://api.smartthings.com/v1/history/devices?locationId=<loc>&deviceId=<id>` (CLI: `smartthings locations:history`). Not real time; events "might take a while to appear" ([forum](https://community.smartthings.com/t/totally-lost-trying-to-get-historic-events/232030), [forum](https://community.smartthings.com/t/smartthings-history-via-api/276392)). Uses: an "Activity" list (on/off times, who changed the input), energy history, and reverse-engineering the phone app's changes.
- **Push without a public server (undocumented):** openHAB's Samsung TV binding (merged in openHAB, uses a PAT) does `POST https://api.smartthings.com/subscriptions` with
  ```json
  {"name":"tv-remote","subscriptionFilters":[{"type":"DEVICEIDS","value":["00000000-0000-4000-8000-000000000001"]}]}
  ```
  then `GET <registrationUrl>` with the bearer token, and reads a Server-Sent-Events stream: `CONTROL_EVENT` (`welcome`) and `DEVICE_EVENT` with capability/attribute/value ([SmartThingsApiService.java](https://github.com/openhab/openhab-addons/blob/main/bundles/org.openhab.binding.samsungtv/src/main/java/org/openhab/binding/samsungtv/internal/service/SmartThingsApiService.java)). Community members call this route undocumented and say "access to it may be restricted somehow" ([forum](https://community.smartthings.com/t/can-i-subscribe-to-smartthings-devices-using-the-smartthings-api/278070)). A 2020 thread had 403s, and staff hinted that for OAuth apps the subscription must use the installedAppId bound to the token ([forum](https://community.smartthings.com/t/403-on-device-subscriptions/207733)). Rate limits: 40 subscription creates per 15 min, 15 other calls per 15 min ([rate limits](https://developer.smartthings.com/docs/getting-started/rate-limits)). **Open question:** does it work with the app's OAuth token? Try once. If it does, the GUI gets live input, volume, power and art changes from any source.
- **Official push** is webhook-only: an installed SmartApp gets `DEVICE_EVENT` POSTs at a public HTTPS URL ([docs](https://developer.smartthings.com/docs/service-integrations/subscribe-to-events)). It doesn't fit a desktop app. Skip it.

### 4.4 Rules, scenes, locations, notifications

- **Rules API**: `POST /rules?locationId=…`, rule JSON with `every.specific` (references like `Noon` plus an offset) and `command` actions. Rules show up as routines in the phone app ([rules docs](https://developer.smartthings.com/docs/automations/rules)). The current app registration has scopes `r:devices:* w:devices:* x:devices:* r:locations:*`. Rules need `r:rules:*`/`w:rules:*`, which the CLI offers, but a third-party summary says Rules "require an Enterprise account" ([scopes thread](https://community.smartthings.com/t/oauth-in-app-and-scopes/309234)). **Recommendation:** do schedules locally with systemd timers (#4). Consider Rules only as an optional "make it work when the PC is off" mode after a test.
- **Scenes**: `GET /scenes`, `POST /scenes/{id}/execute` (50/min). Needs `r:scenes:* x:scenes:*` added to `docs/smartthings-app.json`, so users must update or re-create their app. Medium-low value: a "Scenes" drop-down for "Cinéma" lights plus TV.
- **Locations and rooms**: `r:locations:*` is already granted. Show "Living room" next to cloud TVs. Rooms: 50 requests/hour.
- **Notifications**: no general "push to my phone" API for API-only apps was found. Skip.
- **Rate limits that matter** ([source](https://developer.smartthings.com/docs/getting-started/rate-limits)): **12 requests/min per device** for status reads and commands combined, 10 commands per request, 429 with `X-RateLimit-*` headers. The GUI's 60 s cloud health poll is fine. A cloud remote and frequent status reads must share the budget. Batch several commands into one request where possible.

---

## 5. Samsung account and services

| Service | What a third-party Linux app can legitimately use | Verdict |
|---|---|---|
| SmartThings | The public REST API with the user's own OAuth app (already in place), everything in §4 | **Use** |
| Samsung TV Plus | Launch the app (`custom.launchapp` by name, `samsungvd.launchService.launchTVPlus`, or its Tizen app id). No public API for channels or EPG; channel deep links unknown | Launch button only |
| Gaming Hub | Launch only (id already in `KNOWN_APPS`). No public API | Launch only |
| Samsung Health | Data SDKs are for Android or Galaxy apps; nothing for a TV remote | No relevance |
| SmartThings Find | The TV appears as `samsungim.fixedFindNode` (it relays Find tags). Nothing for a remote | No relevance |
| Bixby | No public API for driving a TV from a PC | No |
| Firmware updates | `samsungvd.firmwareVersion`, `supportsFeatures.wifiUpdateSupport`. No API reports "update available"; link to the model's Samsung support page | Show version + link |
| Smart Hub content (recommendations, "continue watching") | Smart Hub Preview is a TV-app developer feature, not a query API ([smartest-tv #8](https://github.com/Hybirdss/smartest-tv/issues/8)) | No |
| Samsung account login for other APIs | Only through SmartThings OAuth. Samsung Account SDKs are partner-only | No |

---

## 6. Open questions to verify on the real TV (in order)

1. `tv-remote-tui info --json`: `FrameTVSupport`, `ImeSyncedSupport`, `isSupport` flags.
2. ST inspector: `samsungvd.remoteControl`, `custom.tvsearch`, `custom.recording`, `custom.accessibility`, `samsungvd.launchService`, `samsungvd.art`, `custom.energyType` definitions (`st capability <id>`).
3. `execute` reads: `/sec/tv/deviceinfo`, `/sec/tv/mode/picture`, `/sec/tv/optioncontrol`, `/sec/tv/uicontrol`, `/sec/tv/contentinfo`. Then one write to picture mode while the TV's own menu allows changing it.
4. `custom.tvsearch.search("interstellar")`: does the capability itself run the TV's search and show results (including YouTube and Netflix rows)? If not, drop the feature; no keyboard fallback.
5. `samsungvd.remoteControl.send("HOME","PRESS_AND_RELEASED")` with the LAN unplugged.
6. REST `imeInput` into a field the user has focused (system keyboard); the `custom.remote.textReceived` preamble; raw `ms.remote.imeStart`/`imeUpdate` data.
7. The TV's `/dmr` SCPDs: are `X_ControlCaption`, `X_UpdateAudioSelection`, `X_GetStoppedReason`, `X_SetTVSlideShow` still there in 2025? Then the CaptionInfoEx test on a short MKV with an SRT.
8. GENA `SUBSCRIBE` accepted? SmartThings SSE subscription with the OAuth token: 200 or 403?
9. DIAL Netflix body `m=https://www.netflix.com/watch/<id>&source_type=4`: does it start the title?
10. Art channel: does `get_api_version` answer on the Q7F (one approval prompt)?

---

## 7. Removed: key-navigation features

Rule: no feature may work by sending scripted remote-key sequences to drive an app's or the TV's UI. Such sequences are fragile: they break whenever an app or the firmware changes its layout, focus order or menu tree, or when the TV is in an unexpected state. Single keys sent because the user pressed them in the remote UI are still fine.

| Removed | Was | Kept instead |
|---|---|---|
| **Key macros** | client-side key queues with delays and long presses (openHAB's `KEY_MENU,500,KEY_DOWN,-3000` style), for "jump to Picture settings" shortcuts | nothing; settings go through SmartThings `execute` (§4.1) or capabilities |
| **Setting the TV's own sleep timer by key sequence** | menu navigation to the Veille setting | sleep timer as a local systemd timer that sends a direct power-off (top-10 #4), or `execute` if a sleep href is found |
| **Browser "launch, then type the URL" fallback** | launch `org.tizen.browser`, then type the URL with the keyboard | the direct `ed.apps.launch` metaTag only; not offered if the TV ignores it |
| **Typing into app search boxes** (REST `imeInput` aimed at the YouTube/Netflix keyboards) | reaching an in-app keyboard to search | `custom.tvsearch` (only if the capability searches by itself), DIAL deep links for YouTube and Netflix |

The top 10 is unchanged: none of its items depended on navigation. Items #2 (TV search), #5 (cloud remote keys) and "open URL in the TV browser" now say explicitly that they have no navigation fallback.

---

## 8. Not recommended

| Idea | Why not |
|---|---|
| **Local OCF/CoAP control** of the TV (DTLS with OCF certificates, taking ownership, extracting the owner credential from SmartThings' onboarding) | The SmartThings-Local notes show protected TV resources answer `4.01 Unauthorized`. Getting a client credential means re-claiming the device (unpairing it from SmartThings) or extracting the owner credential from Samsung's onboarding flow ([ocf-vd-devices.md](https://github.com/QuiteYellow/SmartThings-Local/blob/HEAD/docs/ocf-vd-devices.md)). That is circumvention of Samsung's ownership model and could break the user's SmartThings setup. Use cloud `execute` instead. |
| **YouTube Lounge API** (queue, now playing, seek inside the YouTube app) | Undocumented Google API, reverse-engineered. YouTube recently changed the screen-id format and is revoking existing pairings ([iSponsorBlockTV](https://github.com/dmunozv04/iSponsorBlockTV), [ytcast](https://github.com/forkkit/ytcast)). Brittle, and a grey zone under Google's terms. At most a separate opt-in plugin. |
| **Samsung TV Plus streams or EPG from third-party scrapers** (community M3U playlists and EPGs) | Scraped private endpoints, outside Samsung's terms. Launching the TV Plus app is fine. |
| **Smart Hub / Samsung account private endpoints** (content recommendations, Samsung account tokens from the phone app) | Needs credential scraping or private APIs. Violates the "own credentials, official APIs" rule. |
| **Default Media Player (Smart View SDK DMP)** | Documented for 2015–2016 TVs only ([Samsung](https://developer.samsung.com/smarttv/develop/extension-libraries/smart-view-sdk/default-media-player.html)). DLNA already covers casting. |
| **Webhook SmartApp** | Needs a public HTTPS endpoint. SSE (if it works) or local polling is enough for a desktop app. |
| **Demand-response control** (`demandResponseLoadControl`) | Meant for utility programs. No user value, and possible side effects on energy-provider programs. |
| **SmartThings Find, Samsung Health, Bixby** | No relevance to a TV remote, or no public API. |
| **App icons via `ed.apps.icon`** | Needs `iconPath` from `ed.installedApp.get`, which this TV ignores. Use bundled icons instead. |
| **Implementing Miracast in tv-remote** | Large (Wi-Fi Direct, RTSP, encoding) with mixed Samsung results. Launch GNOME Network Displays instead. |
| **ST Rules as the main scheduler** | Scopes may need an Enterprise account and would force users to re-register their app. Local systemd timers do the same with no cloud dependency. |

---

*Not affiliated with Samsung. All Samsung, SmartThings and app ids above come from public sources. Nothing in this document was sent to the TV or to Samsung's servers during this research.*
