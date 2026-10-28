# SecondScrn

Use an Android tablet as a wireless **extended monitor** for a Windows PC
or a Linux PC (GNOME or KDE Plasma on Wayland). Move windows onto it, arrange
it in the display settings, and use touch or a pressure-sensitive pen to work
on your PC.

**[Download v0.0.1](https://github.com/niposch/SecondScrn/releases/tag/v0.0.1)** ·
[Report an issue](https://github.com/niposch/SecondScrn/issues)

SecondScrn is an early release. It has been tested on Windows 11 with an
NVIDIA RTX 4080 SUPER and a Xiaomi Pad 7 Pro. Other GPUs, tablets and networks
may behave differently; see [tested features and limitations](docs/status.md).
The Linux app is new in 0.0.2 and less tested than the Windows app.

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

The shared core is Rust; the apps use WinUI 3 (C#), GTK 4 / libadwaita
(Rust, Linux) and Jetpack Compose (Kotlin).

## Requirements

* Windows 11 x64 and a GPU with a supported Media Foundation hardware encoder.
  NVIDIA has been tested; AMD and Intel need broader validation.
* Android 10 or newer, with a compatible hardware video decoder. The release
  APK includes arm64-v8a and x86_64.
* Both devices on the same reachable network. A strong 5 GHz or 6 GHz Wi-Fi
  connection is preferable for high resolutions and frame rates.
* Administrator access once to install the signed Virtual Display Driver.
  The SecondScrn Windows app itself installs per user.
* **Linux:** GNOME or KDE Plasma 6 on **Wayland** (the tablet becomes a
  monitor through the desktop's screen sharing portal; X11 is not
  supported), GTK 4.14+ and libadwaita 1.5+ (Ubuntu 24.04+, Fedora 40+,
  Arch, openSUSE Tumbleweed, KDE neon), GStreamer 1.22+ with the PipeWire
  plugin, and VA-API (Intel, AMD) or the NVIDIA driver for hardware
  encoding.

## Getting started

1. From [Releases](https://github.com/niposch/SecondScrn/releases/latest),
   download and run **SecondScrn-win-Setup.exe** on the PC. This first release
   is not code-signed, so Windows may show a SmartScreen warning.
2. Choose **Install driver…** on SecondScrn's Home page (also under
   **Settings → Tablet monitor**). This installs the separately distributed
   Virtual Display Driver through winget, with an administrator prompt. Allow
   SecondScrn through Windows Firewall when asked.
3. Install **SecondScrn-0.0.1-android.apk** on the tablet. Android will ask
   you to allow installation from the browser or file manager you use.
4. Open the tablet app and select the PC. If discovery does not find it, use
   **Connect manually** with an address shown in the Windows app. Guest Wi-Fi,
   access-point isolation and some VPNs can prevent the devices reaching each other.
5. Enter the tablet's six-digit pairing code in the PC's pairing prompt.
   Arrange the new monitor in **Windows Settings → System → Display**.
   The paired tablet can reconnect automatically afterwards.

### Linux

1. Install **secondscrn_0.0.2_amd64.deb** (Debian, Ubuntu, KDE neon), or
   unpack **SecondScrn-0.0.2-linux-x86_64.tar.gz** and run
   `sudo ./install.sh` (other distributions; see its README for the
   GStreamer packages). Log out and in once, so the pen tablet permission
   applies.
2. Start SecondScrn from the application menu and choose **Allow…** on the
   Home page: the desktop asks once whether SecondScrn may share a screen
   and control the pointer. The permission is remembered.
3. Install the tablet app of the same version, connect and pair as above,
   then arrange the new monitor in the system display settings
   (**Display settings…** on the Home page). The monitor exists while a
   tablet is connected (plus the linger time).

Tap to click, drag to select or move, long-press for a right-click, and use two
fingers to scroll. Three-finger tap or Android Back opens session options.
Choose touch-as-mouse if a Windows app does not handle touch well.

The APK uses a dedicated release signing key. If you previously installed a
development APK signed with a debug key, Android requires you to uninstall it
first; this removes its pairings and settings. Subsequent releases will use
the same release key.

## Presets

A preset sets resolution, frame rate and the bitrate policy together. Each
tablet keeps its own preset, chosen on the PC's Home page, in the tray popup,
on the Tablets page or on the tablet; **Settings → Default preset** applies
to tablets without one. Resolution and frame rate can still be adjusted
afterwards (the preset then shows as changed, with **Reset to preset**) and
are remembered for the tablet. Frame rates are limited to the panel's
capabilities and to **Settings → Highest frame rate**.

| Preset | Resolution | Frame rate | Use |
|---|---|---|---|
| Automatic | Native | Up to 60 fps | Mixed desktop work; adaptive bitrate |
| High quality | Native | Up to 60 fps | Higher bitrate for detail |
| Smooth | 75 % | Up to 120 fps | Motion with fewer pixels to decode |
| Weak Wi-Fi | 67 % | Up to 30 fps | Lower network and decoding load |

Performance hints wait ten seconds after startup, reconnection or a stream
change and require repeated evidence. When the network can't carry the
chosen resolution and frame rate for a few seconds, both apps say so. Hints
recommend adjustments; they never change your selected resolution or frame
rate. The bitrate adapts to the network on its own, keeping the picture
recent rather than letting frames queue up.

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
* Until version 1.0 the PC and tablet apps must run the **same version**.
  The PC app checks the GitHub release feed and installs updates only when
  you choose to; install the tablet app of the same version from Releases at
  the same time. If the versions differ, both apps name the exact version the
  older side needs. Driver installation and updates are separate.
* With an integrated and a dedicated GPU, SecondScrn encodes on the GPU
  Windows runs it on (**Settings → Advanced → Encode on**: Automatic). The
  choice there, or Windows' own graphics setting, overrides it.
* **Linux:** the pen has pressure and tilt through a virtual tablet
  (`/dev/uinput`, granted by the package's udev rule); on KDE Plasma it is
  mapped to the tablet's monitor automatically; on GNOME it may span all
  monitors (not yet verified). Without the rule the pen moves the pointer.
  Touch is real multi-touch where the desktop's portal offers it (the
  Diagnostics page says); otherwise the first finger moves the pointer. Without a hardware encoder SecondScrn falls back to software
  H.264 (higher CPU load). The Linux app shows available updates but
  doesn't install them.

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

On Linux: `cargo build -p secondscrn-linux-app` (see Building for the
system packages).

* [Architecture](docs/architecture.md) · [Security](docs/security.md)
* [Testing](docs/testing.md) · [Test pattern](docs/test-pattern.html)
* [Releasing](docs/releasing.md) · [Virtual Display Driver](docs/virtual-display-driver.md)
* [Status and roadmap](docs/status.md) · [Dependency inventory](docs/dependencies.md)

## License

MIT — see [LICENSE](LICENSE). Third-party components retain their own licences;
see [Third-party notices](THIRD_PARTY_NOTICES.md) and the
[dependency inventory](docs/dependencies.md).
