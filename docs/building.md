# Building

The Windows and Android apps build on Windows 11 x64. The Rust core crates
(everything except `windows-host` and `android-client`'s decoder) also build
and test on Linux/macOS. The Linux app builds on Linux (WSL works for building
and the headless tests).

## Toolchain

| Tool | Version used | Notes |
|---|---|---|
| Rust | 1.95 (stable, edition 2024) | `rustup target add aarch64-linux-android x86_64-linux-android` |
| cargo-ndk | 4.1 | `cargo install cargo-ndk --locked` |
| uniffi-bindgen-cs | 0.11.1+v0.31.0 | `cargo install uniffi-bindgen-cs --git https://github.com/NordSecurity/uniffi-bindgen-cs --tag v0.11.1+v0.31.0` |
| .NET SDK | 10.0 | WinUI app (Windows App SDK 2.5, self-contained) |
| Visual Studio Build Tools | 2026 | MSVC linker and Windows 10/11 SDK (10.0.26100) |
| Android SDK / NDK | platform 36, NDK r29 (29.0.14206865) | JDK 21 (Android Studio's JBR works) |
| Velopack CLI | 1.2.161 | `dotnet tool install -g vpk` |
| ffmpeg/ffprobe | any | optional, used by tests to validate bitstreams |

## Rust

```powershell
cargo test                      # default members: platform-independent crates + devtools
cargo test -p secondscrn-windows-host          # Windows platform (needs a GPU for the conversion test)
cargo clippy --all-targets
cargo fmt --all
```

## Windows app

```powershell
dotnet build apps/windows/SecondScrn.App/SecondScrn.App.csproj
```

The project's `BuildRustCore` target runs `cargo build -p secondscrn-windows-host
--release`, regenerates `Generated/secondscrn_host.cs` with uniffi-bindgen-cs,
and copies `secondscrn_host.dll` next to the app. Pass `-p:SkipRust=true` to
skip it, or `-p:RustProfile=debug` to use a debug core.

Run from `apps/windows/SecondScrn.App/bin/Debug/net10.0-windows10.0.22621.0/win-x64/SecondScrn.exe`.
Data lives in `%LOCALAPPDATA%\SecondScrnData`.

### Headless host

`secondscrn-host-cli` (in `target/debug`) runs the same host without UI:

```powershell
secondscrn-host-cli probe                      # verified hardware encoders
secondscrn-host-cli monitors | targets         # desktop monitors / display targets
secondscrn-host-cli vdd status|attach 2560x1600@60|detach
secondscrn-host-cli record primary hevc 3 out.h265
secondscrn-host-cli serve [--existing [\\.\DISPLAYn]] [--pin-file path] [--data-dir dir]
```

## Android app

```powershell
cd apps/android
./gradlew assembleDebug          # needs JAVA_HOME (JDK 17+), ANDROID_HOME / local.properties
```

The `buildRust` task cross-compiles `libsecondscrn_client.so` for arm64-v8a
and x86_64 with cargo-ndk (always optimised), and `generateBindings` writes the
Kotlin bindings with `uniffi-bindgen` (library mode). Both outputs are
gitignored.

## Linux app

System packages (Ubuntu 24.04; the oldest supported base, since binaries
need its glibc and GTK/libadwaita or newer):

```bash
sudo apt install build-essential pkg-config clang libgtk-4-dev libadwaita-1-dev \
  libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev gstreamer1.0-plugins-base \
  gstreamer1.0-plugins-good gstreamer1.0-plugins-bad gstreamer1.0-plugins-ugly \
  gstreamer1.0-pipewire libdbus-1-dev ffmpeg dpkg-dev fakeroot
cargo build -p secondscrn-linux-app            # target/debug/secondscrn
cargo test -p secondscrn-linux-host            # includes an end-to-end test-pattern stream
packaging/linux/build-release.sh               # .deb and .tar.gz in releases/linux/
```

`secondscrn-linux-cli` is the headless host:

```bash
secondscrn-linux-cli probe                     # verified GStreamer encoders, portal support
secondscrn-linux-cli portal 10                 # opens the virtual monitor for 10 s (permission test)
secondscrn-linux-cli serve [--test-pattern] [--pin-file path] [--data-dir dir]
```

Development switches: `SECONDSCRN_TEST_PATTERN=1` makes the app stream a
moving test pattern instead of a virtual monitor (no desktop needed);
`SECONDSCRN_ENCODERS=x264enc,vah265enc` limits the encoder candidates (NVENC
in WSL produces broken pictures, so tests there use x264). With
`GDK_BACKEND=broadway` and `gtk4-broadwayd` the app's UI can be viewed in a
browser without a desktop. Data lives in `~/.local/share/SecondScrn`.

## Dev client

`secondscrn-dev-client <host> [--out stream.h265] [--seconds 10] [--size WxH]`
connects like a tablet, prints the pairing code, and records the stream for
inspection with ffprobe/ffplay.
