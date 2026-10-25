# Building

All components build on Windows 11 x64. The Rust core crates (everything
except `windows-host` and `android-client`'s decoder) also build and test on
Linux/macOS.

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

## Dev client

`secondscrn-dev-client <host> [--out stream.h265] [--seconds 10] [--size WxH]`
connects like a tablet, prints the pairing code, and records the stream for
inspection with ffprobe/ffplay.
