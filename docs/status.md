# Status, known limitations and roadmap

Version 0.0.1 — first public release.

## Release validation (2026-10-25)

The final workspace unit/integration suite passed (96 tests, 4 opt-in tests
ignored). Windows packaging and signed Android release builds succeeded.
The [nine-case physical performance matrix](performance-0.0.1.md) was rerun
without scrcpy: native resolution reached 117.5 / 141.1 reported fps at
120 / 144 fps settings, with 0 % reported loss in every case. Earlier results
below are historical measurements, not current throughput limits.

An additional hardware-test run accepted touch/pen injection, but its two
display/capture tests failed while the external VDD 2.35 driver was unstable.
The driver also crashed during a later resolution-change probe and was
restarted successfully with administrator approval. These extra hardware
checks are not counted as passing for this release.

One-finger scrolling was reported to travel less than the finger movement
in Google News on Brave. The cause remains unconfirmed; no verified fix is
included in 0.0.1.

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

### Latency, frame rate and picture integrity (0.1, fourth pass)

Measured on the tablet with an animated test window on its monitor:

* **Corrupted video (ghost cursors, smeared blocks) and low tablet frame
  rate:** str0m 0.24.1 delivered HEVC pictures made of several slices as
  several frames (the NVENC preset below produces two slices per picture).
  Fixed in a vendored str0m (`vendor/str0m`, one line) with a regression
  test. Before: 1.7× more frames received than sent, 0–14 fps shown; after:
  frames match, 40–58 fps at a 60 fps setting.
* **Encoder latency:** NVENC quality-vs-speed 70 → 33: 11.6 → 4.8 ms per
  3200×2136 frame.
* **Decoder latency:** maximum operating rate for Qualcomm decoders: ~27 →
  ~13 ms; frames presented at the current time instead of their stream
  timestamp (the platform paced them like video and held some for seconds);
  frames that waited > 250 ms for the decoder are skipped to the next
  keyframe.
* **Frame rates above 60:** Windows Graphics Capture delivered at most 60
  frames/s by default (16 ms minimum update interval, now 1 ms); the tablet
  asks the display for the stream's rate and reports the panel's maximum
  (144 Hz on the test tablet). 1600×1068 now streams at 70–85 fps.
* **Windows scaling** of the tablet's monitor from the tablet (stepper in
  the session options).
* **Snapdragon GSR 1** upscales streams below panel resolution on Adreno
  GPUs (toggle in the session options); verified visually at 1600×1068 →
  3200×2136. GSR 2 is not applicable (temporal, needs a game engine's motion
  vectors and depth).
* **Monitor position** is remembered by SecondScrn and restored on every
  attach, resolution change and rotation (Windows' own memory is per display
  combination and was lost whenever displays or modes changed). Verified by
  moving the monitor, restarting the session and checking it returned.

### High frame rates (0.1, fifth pass)

Measured with a fast animation on the tablet's monitor (release host):

| Setting | Before | After |
|---|---|---|
| native, 60 fps | 71 fps shown, 10 stutters/s | 59 fps, 0.2/s |
| native, 90 fps | — | 90 fps, 1.8/s |
| native, 120/144 fps | 44–52 fps, decoder backlog | automatically lowered to 120 → 90 |
| 50 %, 120/144 fps | 72–82 fps | 107–120 fps |

* The PC sends on a fixed clock (one frame per interval, newest picture)
  instead of following the capture rhythm, which is the compositor's (often
  the main display's 144+ Hz) and picked frames unevenly.
* The virtual monitor runs at the chosen frame rate.
* The tablet's decoder: low-latency mode (one picture at a time) only up to
  3200×2136 at 60 fps worth of pixels, pipelined above; brief input stalls
  are waited out instead of dropping a frame (which cost everything up to the
  next keyframe). At native resolution the decoder still manages only
  ~100–110 fps; when it falls behind twice in 10 s the tablet steps the frame
  rate down (144 → 120 → 90 → 60).
* The video codec can be chosen on the tablet (Automatic, H.264, HEVC, AV1);
  the session reconnects with it. Windows scaling and codec are drop-downs.
* SRTP on the tablet uses the ARMv8 AES/PMULL instructions
  (`.cargo/config.toml`); not a bottleneck before, but cheaper per packet.
* Note: the debug build of the host is too slow for 120 fps at native
  resolution (its software crypto and packetizing overflowed the send
  queue); measurements above use the release build, as shipped.

### Native 120 fps: what limits it (0.1, sixth pass)

Experiments at 3200×2136, 120 fps on the test tablet (Snapdragon, Adreno
735, 120/144 Hz panel):

| Path | Pictures/s |
|---|---|
| Decoder into the window (HEVC / H.264) | 73 / 82 |
| Decoding without displaying anything (H.264) | 116 |
| GL path (copy pass at native size) | 51 |
| Zero-copy `ASurfaceControl` presenter, with backpressure | 85–90 |

The decoder alone keeps up; displaying full-resolution pictures costs the
throughput (with either codec, direct or zero-copy). About 90 fps is what
this tablet sustains at native resolution; 120+ needs a lower resolution.

* **No freezes when behind:** the tablet asks the PC to slow down as soon as
  ~50 ms of frames queue up; frames are skipped at the source (the picture
  keeps moving) instead of a backlog that ended in a jump to the next
  keyframe (100+ ms freezes).
* **Warning instead of automatic switching:** an orange note in the
  session options (and an orange handle) recommends a lower frame rate or
  resolution when the decoder or the network can't keep up.
* **Zero-copy presentation** (Android 10+): the decoder renders into an
  image queue and never waits for the compositor; the newest picture's
  buffer is handed to a child surface layer without a GPU copy.

## Not yet verified (hardware-dependent claims)

* End-to-end latency, smoothness at 60 fps, battery, thermals, colour accuracy
  on the tablet panel, sleep/resume — see the procedure in
  [testing.md](testing.md#physical-tablet-validation).
* AMD/Intel hardware encoders (code path is vendor-neutral Media Foundation,
  only NVIDIA was available).
* Velopack update apply (download + restart) end to end.
* Physical stylus tilt direction and eraser behavior across devices. Pen and
  pressure were manually verified by the user on the test tablet.
* Three- and more-finger gestures on the physical tablet.
* Frame pacing of the Snapdragon GSR path at 120 fps (some unevenness
  measured: it presents without waiting for vsync).
* Snapdragon GSR on Adreno GPUs other than the test tablet's; image quality
  compared with the compositor's scaling (only checked by eye).

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
* Windows builds are not code-signed (SmartScreen may warn). Android releases
  use a dedicated signing key; development APKs use a different key.

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
