# Status, known limitations and roadmap

Version 0.1 — first functional release candidate.

## Verified

| Area | How | Result |
|---|---|---|
| Protocol, security, pairing, negotiation, adaptation | `cargo test` (unit + integration) | pass |
| WebRTC transport incl. loss/burst/jitter/bottleneck/outage | `transport/tests/impairment.rs` | pass |
| Engines: pairing, streaming, input, revocation, reconnection | `session/tests/engines.rs` | pass |
| GPU BT.709 conversion | GPU output = CPU reference ±1 code value | pass (RTX 4080 SUPER) |
| Hardware encoders | H.264 High, HEVC Main, AV1 Main up to 3840×2160 via NVIDIA MFTs; BT.709 limited range signalled (ffprobe) | pass |
| Virtual monitor | VDD attach/detach, new modes (driver reload), portrait, other displays protected, orphan cleanup after crash | pass |
| Windows end to end | VDD → WGC → HEVC → WebRTC → recording client; ffprobe-valid; input mapping | pass |
| Android emulator | pairing, 2560×1600 HEVC stream, touch input, reconnect after host crash, identity-change detection | pass |
| WinUI app | pairing dialog, overview metrics, tray popup, single instance | pass (manual + UI Automation) |
| Velopack | Setup.exe install, delta package generation | pass |
| **Physical tablet** (Xiaomi Pad 7 Pro, 3200×2136, Snapdragon, Wi-Fi) | discovery, pairing, hardware HEVC decoding (`c2.qti.hevc.decoder`) at native resolution | pass |

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

## Not yet verified (hardware-dependent claims)

* End-to-end latency, smoothness at 60 fps, battery, thermals, colour accuracy
  on the tablet panel, sleep/resume — see the procedure in
  [testing.md](testing.md#physical-tablet-validation-not-yet-performed).
* AMD/Intel hardware encoders (code path is vendor-neutral Media Foundation,
  only NVIDIA was available).
* Velopack update apply (download + restart) end to end.

## Known limitations

* **Windows Firewall:** the first time SecondScrn listens, Windows asks whether
  to allow it. If that prompt is declined, tablets cannot connect (the per-user
  installer cannot add firewall rules). Fix: allow *SecondScrn* in *Windows
  Security → Firewall → Allow an app*. Detection and guidance in the app are
  planned.
* Input: mouse emulation only (tap/drag/right-click/scroll). No keyboard, pen
  pressure or multi-touch gestures yet. Elevated windows don't receive input
  (Windows UIPI).
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
3. Keyboard and pen input; on-screen keyboard toggle.
4. Text-sharpness experiments behind a flag, benchmarked before adoption:
   dirty-region/ROI encoding, HEVC 4:4:4 where decoders support it, AVC444
   (two 4:2:0 streams), lossless refresh of static regions.
5. Optional USB transport (adb reverse) behind the existing transport boundary.
6. Higher refresh rates (90/120 Hz) when both ends support them.
7. Signed releases and CI publishing.
