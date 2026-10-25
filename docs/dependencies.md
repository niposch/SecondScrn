# Dependency inventory

The release is built from the exact Rust versions in [Cargo.lock](../Cargo.lock),
Android versions in [the Gradle version catalog](../apps/android/gradle/libs.versions.toml),
and Windows packages in [SecondScrn.App.csproj](../apps/windows/SecondScrn.App/SecondScrn.App.csproj).

## Rust

[Complete resolved Rust inventory](rust-dependencies.md) (including platform,
build and test dependencies), with upstream license declarations and source links.
Refresh it with `python tools/dependency_inventory.py` after fetching dependencies.
The script collects license/notice texts from the resolved package sources into
[licenses/RUST-NOTICES.txt](../licenses/RUST-NOTICES.txt).

Key components: str0m (WebRTC, locally patched), rustls/ring (TLS), SPAKE2
(pairing), Tokio, mdns-sd, UniFFI, Windows bindings, and Android NDK/JNI bindings.
UniFFI is MPL-2.0; its source is available at the versioned upstream links in
the inventory. SecondScrn's Rust source is included in this repository.

## Windows UI and runtime

| Direct package | Version | Declared licence |
|---|---|---|
| Microsoft.WindowsAppSDK | 2.5.1 | Microsoft Windows App SDK binary terms; upstream source includes MIT components |
| Microsoft.Windows.SDK.BuildTools | 10.0.28000.2705 | Microsoft SDK terms (build tool) |
| H.NotifyIcon.WinUI | 2.4.1 | MIT |
| Velopack | 1.2.161 | MIT |
| CommunityToolkit.Mvvm | 8.4.2 | MIT |

The self-contained app includes the .NET runtime and Windows App SDK runtime.
Their package license and third-party notice files accompany the Windows
release. NuGet resolves additional platform/runtime packages; see the restored
`obj/project.assets.json` when reproducing a build.

## Android UI and runtime

| Component | Version | Licence |
|---|---|---|
| AndroidX Core KTX | 1.16.0 | Apache-2.0 |
| AndroidX Activity Compose | 1.10.1 | Apache-2.0 |
| AndroidX Lifecycle | 2.9.1 | Apache-2.0 |
| Jetpack Compose BOM | 2025.06.00 | Apache-2.0 |
| Kotlin / Compose compiler plugin | 2.2.20 | Apache-2.0 |
| kotlinx.coroutines | 1.10.2 | Apache-2.0 |
| JNA | 5.17.0 | LGPL-2.1-or-later OR Apache-2.0 (Apache-2.0 option used) |
| Android Gradle Plugin (build tool) | 8.12.2 | Apache-2.0 |

Compose modules are resolved by the BOM, rather than sharing its version number.
Use `./gradlew :app:dependencies --configuration releaseRuntimeClasspath` to
inspect the complete Android runtime dependency graph. Platform MediaCodec
and Windows Media Foundation provide codecs; SecondScrn does not bundle FFmpeg
or a standalone H.264/HEVC/AV1 codec implementation.

## Vendored code and separately installed software

* [str0m 0.24.1](../vendor/str0m): MIT OR Apache-2.0. The HEVC depacketizer patch
  and regression-test reference are documented in [SECONDSCRN-PATCH.md](../vendor/str0m/SECONDSCRN-PATCH.md).
* Qualcomm Snapdragon GSR 1 shader: BSD-3-Clause, with its exact copyright and
  terms in [LICENSE-SGSR.txt](../crates/android-client/src/shaders/LICENSE-SGSR.txt).
* Virtual Display Driver is installed separately and is not bundled in the
  SecondScrn installer. Its source, releases and terms belong to the
  [VirtualDrivers project](https://github.com/VirtualDrivers/Virtual-Display-Driver).

[Third-party notices](../THIRD_PARTY_NOTICES.md) describes the bundled notices.
