# TV Remote for Android

An Android remote for Samsung Tizen TVs, built on the Rust core of
[tv-remote](../tv-remote) (`tvremote_core`) through UniFFI bindings, with a Jetpack Compose
(Material 3) UI. Package `fr.ryoku.tvremote`, app name "TV Remote", minSdk 26, targetSdk 37.

Milestone 1, "Remote first":

- **TVs**: search the Wi-Fi (SSDP + REST), add by scan or by IP, pair (the TV shows an
  approval prompt; the token is saved), rename, remove, several TVs with one active.
- **Remote**: every key of the core (D-pad, OK, back, home, source, settings, info, options,
  exit, volume and channel rockers with hold-to-repeat, mute, media keys, number pad with
  previous channel, channel list, guide, colour keys, power off), haptic feedback.
- **Power**: on with Wake-on-LAN (the MAC comes from a scan, or from the TV once it is on;
  the phone's Wi-Fi broadcast address is passed to the core), off over the remote channel.
  Never a power key after Wake-on-LAN (the core's rule).
- **Volume**: rockers (keys), mute toggle and a slider (UPnP volume).
- **Text**: a text box, opened automatically when the TV shows its keyboard.
- **Touchpad**: drag to move the TV's pointer, tap to click.
- **Apps**: installed apps (tap to launch), YouTube and Netflix shortcuts, and "Share → Play
  on TV" for YouTube links (DIAL; falls back to opening the app).

Not in this milestone: casting, SmartThings.

## Layout

```
rust/              tvremote-ffi: UniFFI proc-macro bindings over tvremote_core (rustls build)
  src/lib.rs       RemoteCore (TV list, scan, events), TvRemote (one TV), records/enums
  src/tests.rs     offline unit tests (host)
  src/bin/uniffi-bindgen.rs   the bindgen CLI, same uniffi version as the crate
app/               Android app (Compose)
  build.gradle.kts cargoNdkBuild + uniffiBindgen tasks, wired as generated jniLibs/Kotlin sources
  src/main/java/fr/ryoku/tvremote/
    MainActivity.kt, MainViewModel.kt, TvRemoteApp.kt (core + network helpers)
    logic/         pure state and math (JVM unit tests in src/test)
    ui/            screens and controls
```

The core comes from the `android-core` branch of tv-remote, checked out next to this
repository: `../tv-remote-android-core/core` (see `rust/Cargo.toml`). That branch adds a
`rustls` TLS feature (no OpenSSL to cross-compile), `config::set_cache_dir` and
`wol::set_broadcast_hints`.

## Toolchain

Everything lives in `~/Android` (nothing system-wide):

| What | Where |
|---|---|
| JDK (Temurin 21) | `~/Android/jdk/` |
| Android SDK: cmdline-tools, platform-tools, platform 37, build-tools 37, NDK 29 | `~/Android/sdk/` |
| Rust (rustup, stable, Android targets, cargo-ndk) | `~/Android/rust/{rustup,cargo}` |
| Gradle (wrapper downloads) | `~/Android/gradle/` |

`~/Android/env.sh` exports `JAVA_HOME`, `ANDROID_HOME`, `ANDROID_NDK_HOME`, `RUSTUP_HOME`,
`CARGO_HOME`, `GRADLE_USER_HOME` and puts their `bin` directories first in `PATH`. It is a
bash script; from fish run builds through bash:

```fish
bash -c 'source ~/Android/env.sh && ./gradlew assembleDebug'
```

`local.properties` (not committed) holds `sdk.dir=/home/<you>/Android/sdk`.

## Build, test, install

```sh
source ~/Android/env.sh
./gradlew assembleDebug                         # app/build/outputs/apk/debug/app-debug.apk
./gradlew assembleDebug -PrustAbis=arm64-v8a    # one ABI: faster while developing
./gradlew testDebugUnitTest                     # Kotlin/JVM tests (pure logic)
(cd rust && cargo test && cargo clippy --all-targets)   # Rust FFI tests (host, offline)
adb install -r app/build/outputs/apk/debug/app-debug.apk
```

The Gradle build runs `cargo ndk` for arm64-v8a, armeabi-v7a and x86_64 (release profile)
and `uniffi-bindgen generate --library` on a host build of the crate; the Kotlin bindings
land in `app/build/generated/uniffi/kotlin` (package `fr.ryoku.tvremote.core`).

Without adb: copy the APK to the phone and open it (allow installing unknown apps for the
file manager when asked).

## Notes

- The TV's HTTP services (REST :8001, DIAL :8080, UPnP :9197) are cleartext: Android's
  network security config cannot scope cleartext to IP ranges, so it is allowed for the app
  (`res/xml/network_security_config.xml`). The remote channel is TLS (wss :8002, the TV's
  self-signed certificate is accepted, as on the desktop).
- The scan holds a Wi-Fi multicast lock (`CHANGE_WIFI_MULTICAST_STATE`).
- The pairing prompt on the TV names the phone "TV Remote (<model>)". The app only connects
  by itself to a TV that is already paired; the first connection follows "Pair" or a key press.
- Config: `files/config.json` in the app's private storage (same format as the desktop's).
