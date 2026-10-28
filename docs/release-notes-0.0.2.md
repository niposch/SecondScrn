# SecondScrn 0.0.2 (draft)

Lower latency on busy or unreliable Wi-Fi, fewer freezes, a reorganised
Windows app, **a first Linux app**, and versions that are updated in
lockstep.

**Update both apps.** Until 1.0 the PC and tablet apps only work together at
the same version. Tablets still on 0.0.1 show "the host and tablet app
versions are incompatible" against a 0.0.2 PC: install the 0.0.2 APK. From
0.0.2 on, a version mismatch names the exact version the older side needs.

## Downloads

* **SecondScrn-win-Setup.exe** — Windows 11 x64 installer.
* **SecondScrn-win-Portable.zip** — portable Windows app.
* **SecondScrn-0.0.2-android.apk** — signed Android 10+ APK (arm64-v8a and x86_64).
* **secondscrn_0.0.2_amd64.deb** — Linux (Ubuntu 24.04+, Debian-based).
* **SecondScrn-0.0.2-linux-x86_64.tar.gz** — Linux, other distributions
  (`sudo ./install.sh`).
* **SHA256SUMS.txt** — checksums for the binary assets.

## Changes

* **Linux (new, early):** the tablet becomes a monitor on GNOME and KDE
  Plasma 6 on Wayland through the desktop's screen sharing portal — no
  driver, no root. Hardware encoding with VA-API (Intel, AMD) or NVENC
  (NVIDIA) through GStreamer; touch, mouse and the pen with pressure and tilt
  (a virtual tablet, mapped to the tablet's monitor on KDE Plasma). The app
  has the same pages and options as the Windows app, tray quick controls
  (preset, resolution, frame rate) and notifies about updates.

* **Latency stays low when the network struggles.** The tablet reports when
  each frame arrived; the PC lowers the bitrate within about 100 ms of frames
  starting to queue, never queues frames that could only arrive late, and
  retransmits only what can still arrive in time. In a simulated link drop
  from 8 to 3 Mbit/s, latency settled at ~35 ms instead of ~1 s, with no
  freeze; quality recovers fully once the link clears.
* **Fewer freezes after loss.** With NVIDIA H.264 and AV1 encoding, a loss is
  recovered with one small frame from a reference picture the tablet
  confirmed instead of a large keyframe. HEVC and other encoders recover with
  keyframes as before.
* **Warning when the network is too slow** for the chosen resolution and
  frame rate, on both apps. Nothing changes automatically.
* **Encoder GPU: Automatic** by default — the GPU Windows runs SecondScrn on
  (the screens' GPU on desktops, the power-saving one on laptops).
* **Windows app reorganised:** Home (connection, the tablet's preset,
  resolution and frame rate, statistics), Tablets, Settings (quality
  defaults, tablet monitor, app, network, Advanced) and Diagnostics (encoders,
  performance test, logs). Presets belong to each tablet; changing
  resolution or frame rate shows the preset as changed, with Reset to preset.
  One frame-rate limit applies to every preset; Weak Wi-Fi's frame rate is a
  default, no longer a hidden cap.
* Frame rates update every 250 ms and show what the tablet displays;
  statistics use fixed-width digits.

## Tested and known limitations

Tested on Windows 11 / NVIDIA RTX 4080 SUPER and Xiaomi Pad 7 Pro. The Linux
app was tested without a desktop (test pattern, CI-style); verification on
KDE Plasma and GNOME desktops is pending. Recovery
frames on the tablet's decoder over real Wi-Fi, hybrid-GPU laptops and AMD/Intel
encoders still need validation. See [status](status.md).
