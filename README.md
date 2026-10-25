# SecondScrn

Use an Android tablet as a wireless **extended monitor** for a Windows PC.
Move windows onto it, arrange it in Windows Display settings, and use touch
or a pressure-sensitive pen to work on your PC.

**[Download v0.0.1](https://github.com/niposch/SecondScrn/releases/tag/v0.0.1)** ·
[Report an issue](https://github.com/niposch/SecondScrn/issues)

SecondScrn is an early release. It has been tested on Windows 11 with an
NVIDIA RTX 4080 SUPER and a Xiaomi Pad 7 Pro. Other GPUs, tablets and networks
may behave differently; see [tested features and limitations](docs/status.md).

## Features

* A real additional Windows display through the open-source
  [Virtual Display Driver](https://github.com/VirtualDrivers/Virtual-Display-Driver),
  with portrait/landscape support and remembered monitor placement.
* Hardware video encoding and Android hardware decoding, with automatic codec
  negotiation or a choice of HEVC, H.264 and AV1 where supported.
* Native resolution or 85/75/67/50 % scaling, and selectable frame rates up to
  the tablet's refresh rate. Optional Snapdragon GSR upscaling on supported Adreno GPUs.
* Multi-touch, touch-as-mouse mode, and Windows pen input with pressure,
  tilt, hover, barrel button and eraser support. Pen and pressure have been
  manually tested on the physical tablet.
* Automatic discovery, PIN pairing, reconnection, a Windows tray popup and
  tablet rotation lock.
* Direct local connections: no account, cloud service or signalling server.
  Pinned mutual TLS protects control traffic; WebRTC DTLS-SRTP encrypts video.

The shared core is Rust; the apps use WinUI 3 (C#) and Jetpack Compose (Kotlin).

## Requirements

* Windows 11 x64 and a GPU with a supported Media Foundation hardware encoder.
  NVIDIA has been tested; AMD and Intel need broader validation.
* Android 10 or newer, with a compatible hardware video decoder. The release
  APK includes arm64-v8a and x86_64.
* Both devices on the same reachable network. A strong 5 GHz or 6 GHz Wi-Fi
  connection is preferable for high resolutions and frame rates.
* Administrator access once to install the signed Virtual Display Driver.
  The SecondScrn Windows app itself installs per user.

## Getting started

1. From [Releases](https://github.com/niposch/SecondScrn/releases/latest),
   download and run **SecondScrn-win-Setup.exe** on the PC. This first release
   is not code-signed, so Windows may show a SmartScreen warning.
2. Open **Display → Install driver…** in SecondScrn. This installs the
   separately distributed Virtual Display Driver through winget, with an
   administrator prompt. Allow SecondScrn through Windows Firewall when asked.
3. Install **SecondScrn-0.0.1-android.apk** on the tablet. Android will ask
   you to allow installation from the browser or file manager you use.
4. Open the tablet app and select the PC. If discovery does not find it, use
   **Connect manually** with an address shown in the Windows app. Guest Wi-Fi,
   access-point isolation and some VPNs can prevent the devices reaching each other.
5. Enter the tablet's six-digit pairing code in the PC's pairing prompt.
   Arrange the new monitor in **Windows Settings → System → Display**.
   The paired tablet can reconnect automatically afterwards.

Tap to click, drag to select or move, long-press for a right-click, and use two
fingers to scroll. Three-finger tap or Android Back opens session options.
Choose touch-as-mouse if a Windows app does not handle touch well.

The APK uses a dedicated release signing key. If you previously installed a
development APK signed with a debug key, Android requires you to uninstall it
first; this removes its pairings and settings. Subsequent releases will use
the same release key.

## Presets

Selecting a different preset sets resolution, frame rate and the bitrate
policy together. Resolution and frame rate can still be adjusted afterwards
and are remembered for the tablet. Frame rates are limited to the panel's capabilities.

| Preset | Resolution | Frame rate | Use |
|---|---|---|---|
| Automatic | Native | Up to 60 fps | Mixed desktop work; adaptive bitrate |
| High quality | Native | Up to 60 fps | Higher bitrate for detail |
| Smooth | 75 % | Up to 120 fps | Motion with fewer pixels to decode |
| Weak Wi-Fi | 67 % | Up to 30 fps | Lower network and decoding load |

Performance hints wait ten seconds after startup, reconnection or a stream
change and require repeated evidence. They recommend adjustments; they do
not change your selected resolution or frame rate.

## Limitations and troubleshooting

* One tablet at a time; SDR BT.709 video with 4:2:0 chroma. Very fine coloured
  text can look softer than black-and-white text. No HDR or keyboard forwarding yet.
* High frame rates at native resolution depend on the tablet's decoder and
  compositor. Lower the resolution or frame rate if it cannot sustain the stream.
* A new display mode can reload the driver for a few seconds; Windows may
  move windows during the change. Elevated Windows apps cannot receive injected input.
* One-finger scrolling can travel less than the finger movement, reported in
  Google News on Brave. A verified fix is still outstanding.
* Virtual Display Driver 2.35 can crash during repeated mode changes. If the
  tablet cannot reconnect afterwards, restart that driver in Device Manager
  with administrator approval, or restart Windows.
* If the tablet cannot connect, check Windows Firewall and network isolation.
  For stuttering, close other screen-sharing tools (including scrcpy), then
  try the Smooth or Weak Wi-Fi preset.
* Windows updates use the GitHub release feed. Android updates are installed
  manually from Releases. Driver installation and updates are separate.

When reporting an issue, include app versions, GPU, tablet model, Android
version, codec, resolution/frame rate and network setup. The Diagnostics page
can export logs; review them for personal information before posting publicly.

## Build and documentation

See [Building](docs/building.md) for toolchain setup and platform builds.

```powershell
cargo test --workspace
dotnet build apps/windows/SecondScrn.App/SecondScrn.App.csproj
cd apps/android
./gradlew assembleDebug
```

* [Architecture](docs/architecture.md) · [Security](docs/security.md)
* [Testing](docs/testing.md) · [Test pattern](docs/test-pattern.html)
* [Releasing](docs/releasing.md) · [Virtual Display Driver](docs/virtual-display-driver.md)
* [Status and roadmap](docs/status.md) · [Dependency inventory](docs/dependencies.md)

## License

MIT — see [LICENSE](LICENSE). Third-party components retain their own licences;
see [Third-party notices](THIRD_PARTY_NOTICES.md) and the
[dependency inventory](docs/dependencies.md).
