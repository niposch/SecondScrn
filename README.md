# SecondScrn

Use an Android tablet as a wireless **extended monitor** for a Windows PC.
Windows sees a real additional display — move windows onto it, arrange it in
Display settings, use it like any other screen. Built for programming,
reading, chat and video next to your main monitor: sharp text, correct
colours, stable frames.

* **Real second monitor** via the open-source [Virtual Display Driver](https://github.com/VirtualDrivers/Virtual-Display-Driver), at the tablet's native resolution and refresh rate, portrait or landscape. Lower resolutions (85–50 %) and frame rates for weak Wi-Fi, switchable from the tablet, the PC app or the Windows display settings.
* **Hardware video end to end:** Windows Graphics Capture → GPU colour conversion → NVIDIA/AMD/Intel encoder (HEVC, AV1, H.264) → WebRTC → Android MediaCodec onto a native surface. No CPU pixel copies.
* **Secure on untrusted networks:** device identities, pinned mutual TLS 1.3, SPAKE2 PIN pairing, DTLS-SRTP media, authenticated input only.
* **Just works:** automatic discovery, one-time pairing, automatic reconnection, tray icon with a quick-control popup, rotation lock.
* **Touch and pen:** real multi-touch (Windows gestures), pressure-sensitive pen with tilt, hover and eraser; optional touch-as-mouse mode.
* **Local only:** no accounts, no cloud, no signalling servers.

Core logic is Rust (shared by both apps); the UIs are native WinUI 3 (C#)
and Jetpack Compose (Kotlin).

## Status

Early but functional (v0.1). See [docs/status.md](docs/status.md) for what is
verified, what is not, and the roadmap. Physical-tablet performance has **not**
been validated yet — all Android testing so far used the emulator.

## Getting started

1. **PC:** install SecondScrn (`SecondScrn-win-Setup.exe` from Releases). On
   first start, open **Display → Install driver…** (one administrator prompt;
   installs the signed VirtualDrivers driver through winget).
2. **Tablet:** install the APK, open it, pick your PC (or *Connect manually*
   with the address shown on the PC), and type the 6-digit code shown on the
   tablet into the PC's pairing prompt.
3. The tablet becomes a monitor next to your screens. Arrange it under
   *Windows Settings → Display*. Afterwards the tablet reconnects by itself.

On the tablet: tap = click, drag = drag/select, long-press = right-click, two
fingers = scroll, three-finger tap or Back = session menu.

## Building

See [docs/building.md](docs/building.md). In short (Windows):

```powershell
cargo test                                   # Rust unit + integration tests
dotnet build apps/windows/SecondScrn.App      # builds Rust core, bindings, WinUI app
cd apps/android; ./gradlew assembleDebug      # builds Rust for Android, bindings, APK
```

## Documentation

* [Architecture](docs/architecture.md) — components, data flow, design decisions and deviations
* [Security](docs/security.md) — identities, pairing, encryption, threat model
* [Building](docs/building.md) — development environment and build commands
* [Testing](docs/testing.md) — automated tests, emulator workflow, physical-device and network-impairment plans
* [Releasing](docs/releasing.md) — Velopack packaging, signing, Android release builds
* [Virtual Display Driver](docs/virtual-display-driver.md) — installation and management
* [Status and roadmap](docs/status.md) — verified features, known limitations
* [Third-party notices](THIRD_PARTY_NOTICES.md) and [dependency inventory](docs/dependencies.md)

## License

MIT — see [LICENSE](LICENSE). Third-party components keep their own licences
(see [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)); video codec patent
considerations are summarised in [docs/dependencies.md](docs/dependencies.md).
