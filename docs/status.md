# Status, known limitations and roadmap

Version 0.1 — first functional release candidate.

## Verified

| Area | How | Result |
|---|---|---|
| Protocol, security, pairing, negotiation, adaptation | `cargo test` (unit + integration) | pass |
| WebRTC transport incl. loss/burst/jitter/bottleneck/outage | `transport/tests/impairment.rs` | pass |
| Engines: pairing, streaming, input (mouse, touch frames, pen), revocation, reconnection, resolution/frame-rate switching from tablet, PC and Windows settings | `session/tests/engines.rs` | pass |
| GPU BT.709 conversion | GPU output = CPU reference ±1 code value | pass (RTX 4080 SUPER) |
| Hardware encoders | H.264 High, HEVC Main, AV1 Main up to 3840×2160 via NVIDIA MFTs; BT.709 limited range signalled (ffprobe) | pass |
| Virtual monitor | VDD attach/detach, new modes (driver reload), portrait, other displays protected, orphan cleanup after crash | pass |
| Windows end to end | VDD → WGC → HEVC → WebRTC → recording client; ffprobe-valid; input mapping; synthetic touch (2 fingers) and pen frames accepted by Windows | pass |
| Android emulator | pairing, 2560×1600 HEVC stream, touch input, reconnect after host crash, identity-change detection | pass |
| WinUI app | pairing dialog, overview metrics, tray popup, single instance | pass (manual + UI Automation) |
| Velopack | Setup.exe install, delta package generation | pass |
| **Physical tablet** (Xiaomi Pad 7 Pro, 3200×2136, Snapdragon, Wi-Fi) | discovery, pairing, hardware HEVC decoding (`c2.qti.hevc.decoder`) at native resolution; switching to 50 % / 75 % / native from the tablet overlay; real two-finger touch reaching Windows (valid frames, none rejected) | pass |

### Fixed after the first physical-tablet test

The first build on a real tablet was laggy with very low bandwidth. Measured
cause: str0m paces media at 1.1 × its GCC estimate, which collapses to the
idle send rate on a mostly static desktop (~1 Mbit/s), so large frames waited
seconds in the pacer (4–6 fps on the tablet). The pacer/estimator was replaced
by RTCP-feedback AIMD control; on the same tablet the stream then ran at
28–42 fps with bursts up to ~20 Mbit/s and 5–30 ms RTT. In the same pass:
1 ms Windows timer resolution (default ~15.6 ms capped pacing near 32 fps),
low-latency Wi-Fi lock on the tablet while streaming, RTP gap detection with
keyframe recovery after burst loss.

### Resolution, frame rate, touch and pen (0.1, second pass)

* The stream resolution can be changed from the tablet overlay, the PC app
  (Display page, tray popup) and the Windows display settings. Offered sizes
  are 100/85/75/67/50 % of the panel, keeping its aspect ratio; any other mode
  picked in Windows is followed (letterboxed on the tablet). The choice is
  remembered per tablet and kept across rotations.
* Frame rate per tablet (30/60/90/120/144 up to the panel's refresh rate);
  overrides the global default.
* Touch is sent as real Windows touch (up to ten fingers, Windows gestures);
  "touch as mouse" remains as an option. Stylus input is a Windows pen with
  pressure, tilt, hover, barrel button and eraser; fingers are ignored while
  the pen is near the screen.
* Rotation lock on the tablet.
* **Driver limit found:** VDD 2.35 crashed (access violation; Windows took the
  device offline after five restarts) when ten more resolutions were added at
  once, because every entry is multiplied by every global refresh rate.
  SecondScrn now keeps at most four entries of its own in
  `vdd_settings.xml` (tagged, removed again when no longer needed). A new
  size therefore costs one driver reload (~5 s, the monitor briefly
  disappears) the first time it is used; rotating and returning to the native
  size do not.

### Stutter at full resolution and idle responsiveness (0.1, third pass)

Video at 3200×2136 stuttered every few seconds while half resolution was
smooth. Measured on the tablet: loss bursts of 5–17 %, a keyframe every ~3 s,
the bitrate budget collapsing after each one, seconds at 0 fps. Causes, all
local: Windows' 64 KB default UDP send buffer dropped the tail of every large
frame; the tablet discarded frames when its 3-frame decode queue overflowed
after a burst, and every discard requested another (large) keyframe. With
8 MB socket buffers, decode-everything/render-newest and keyframe-request
throttling, two minutes of full-resolution video ran at 0 % loss, 0
retransmissions, 1 keyframe, 30 fps (video) to 55 fps (scrolling) at up to
57 Mbit/s.

Idle round trip with a 1 s keepalive: 61 ms median, 181 ms p90 (the link
dozes); with 200 ms: 28 ms / 62 ms. The keepalive is now 200 ms.

Touch no longer leaves the mouse cursor on the tablet display: its position is
restored after each touch gesture unless the mouse was moved meanwhile.

## Not yet verified (hardware-dependent claims)

* End-to-end latency, smoothness at 60 fps, battery, thermals, colour accuracy
  on the tablet panel, sleep/resume — see the procedure in
  [testing.md](testing.md#physical-tablet-validation-not-yet-performed).
* AMD/Intel hardware encoders (code path is vendor-neutral Media Foundation,
  only NVIDIA was available).
* Velopack update apply (download + restart) end to end.
* Pen pressure/tilt on a physical stylus (the injection path is tested with
  synthetic frames; the tilt direction mapping from Android is unverified).
* Three- and more-finger gestures on the physical tablet.

## Known limitations

* **Windows Firewall:** the first time SecondScrn listens, Windows asks whether
  to allow it. If that prompt is declined, tablets cannot connect (the per-user
  installer cannot add firewall rules). Fix: allow *SecondScrn* in *Windows
  Security → Firewall → Allow an app*. Detection and guidance in the app are
  planned.
* Input: no keyboard yet. Elevated windows don't receive input (Windows UIPI).
* Changing to a resolution not used before reloads the driver once; windows on
  the tablet monitor may move to the main screen during that.
* One tablet at a time; one virtual monitor.
* 4:2:0 chroma: coloured 1-px text is slightly soft (AVC444/4:4:4 experiments
  are on the roadmap).
* No HDR. SDR BT.709 only.
* The virtual display driver must be installed once with administrator
  approval; Velopack updates never touch it.
* The virtual monitor stays connected-but-detached in Windows between
  sessions (the driver keeps at least one monitor); it is not part of the
  desktop and can't receive windows.
* Builds are unsigned until certificates are available (SmartScreen warning on
  Windows; Android release signing needs a keystore).

## Roadmap

1. Firewall detection and one-click elevated rule creation (separate from the
   driver setup).
2. Physical-device validation matrix (latency, battery, thermals, colour) and
   tuning of bitrate floors per preset from measurements.
3. Keyboard input; on-screen keyboard toggle.
4. Text-sharpness experiments behind a flag, benchmarked before adoption:
   dirty-region/ROI encoding, HEVC 4:4:4 where decoders support it, AVC444
   (two 4:2:0 streams), lossless refresh of static regions.
5. Optional USB transport (adb reverse) behind the existing transport boundary.
6. HDR (HDR10 via the driver's HDR mode, FP16 capture, PQ/BT.2020 shader,
   HEVC Main10, HDR surface on the tablet).
7. Signed releases and CI publishing.
