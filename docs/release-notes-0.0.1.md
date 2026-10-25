# SecondScrn 0.0.1

First public release: use an Android tablet as an extended Windows monitor,
with local encrypted streaming, PIN pairing, touch and pressure-sensitive pen input.

## Downloads

* **SecondScrn-win-Setup.exe** — Windows 11 x64 installer.
* **SecondScrn-win-Portable.zip** — portable Windows app.
* **SecondScrn-0.0.1-android.apk** — signed Android 10+ APK (arm64-v8a and x86_64).
* **SHA256SUMS.txt** — checksums for the binary assets.

The `.nupkg`, `RELEASES` and JSON files support Windows automatic updates;
users normally only need the installer and APK. Source archives are supplied
by GitHub with the release tag.

## Included

* A real extended monitor, portrait/landscape, remembered display placement,
  and native or reduced resolution with selectable frame rates.
* Hardware HEVC/H.264/AV1 streaming where supported, automatic reconnection,
  Windows tray controls and Android session options.
* Multi-touch and pen support; pen and pressure manually tested on the
  physical tablet. Optional touch-as-mouse and Snapdragon GSR upscaling.
* Presets that set resolution, frame rate and bitrate policy together:
  Automatic (native/60), High quality (native/60, higher bitrate), Smooth
  (75 %/up to 120), Weak Wi-Fi (67 %/30). You can adjust resolution/FPS afterwards.
* Conservative performance hints: ten-second settling period after startup,
  stream changes and reconnection, followed by repeated bad reports before a
  hint appears. Existing warnings clear when the stream restarts.

## Tested and known limitations

Tested on Windows 11 / NVIDIA RTX 4080 SUPER and Xiaomi Pad 7 Pro. The
[performance matrix](performance-0.0.1.md) was rerun with scrcpy stopped;
previous runs with another screen-sharing workload are not tablet limits.
Results are short animation samples, not end-to-end latency, thermal or battery claims.

This is an early release. AMD/Intel encoders and other tablets need broader
validation. One tablet at a time; SDR BT.709 / 4:2:0 video; no HDR or keyboard
forwarding. Elevated Windows apps cannot receive injected input. A new display
mode may briefly reload the virtual driver and move windows.

* **Known touch issue:** one-finger scrolling can move content less than the
  finger travels; reported in Google News on Brave. The cause is unconfirmed
  and this release does not include a verified fix.
* **Driver 2.35 stability:** repeated display-mode changes can crash the
  separately installed Virtual Display Driver. If it stops, restart the
  driver from Device Manager (administrator approval) or restart Windows.

Windows binaries are not code-signed; SmartScreen may warn. The separately
installed Virtual Display Driver is signed and requires administrator approval.
Allow SecondScrn through Windows Firewall when prompted.

The Android APK uses a dedicated release key. A previously installed debug APK
must be uninstalled first, which removes its settings and pairings. Future
public versions use the same release key. Android updates are installed manually.

See the [README](../README.md) for setup and [status](status.md) for the full
validation record and outstanding hardware checks.
