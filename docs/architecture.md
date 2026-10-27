# Architecture

SecondScrn turns an Android tablet into an extended Windows monitor. Windows
sees a real additional display (an IddCx virtual monitor), its content is
captured and hardware-encoded on the GPU, sent over an authenticated WebRTC
connection, and hardware-decoded straight onto a `Surface` on the tablet.

```
 Windows PC                                                    Android tablet
┌──────────────────────────────────────────────┐            ┌───────────────────────────────┐
│ WinUI 3 app (C#)  ── UniFFI ──┐              │            │ Compose app (Kotlin) ─ UniFFI ┐│
│  tray, popup, settings        │              │            │  hosts, pairing, gestures     ││
│                               ▼              │            │                               ▼│
│ secondscrn-windows-host (Rust)               │            │ secondscrn-android-client      │
│  VDD control ─ CCD attach/detach             │            │  MediaCodec (NDK) → Surface    │
│  WGC capture → D3D11 BGRA→NV12 shader        │            │        ▲ JNI: setSurface()     │
│  → Media Foundation HW encoder (NVENC/AMF/QSV)│           │        │                       │
│  SendInput (authenticated input)             │            │        │                       │
│                 ▲ HostPlatform trait         │            │        │ ClientPlatform trait  │
│ secondscrn-session: HostEngine ──────────────┼─ TLS 1.3 ──┼─ ClientEngine                  │
│   pairing, negotiation, adaptation           │  (mTLS,    │   pairing, reconnect, decode   │
│ secondscrn-transport (str0m WebRTC) ─────────┼─ pinned) ──┼─ secondscrn-transport          │
│                                              │  DTLS-SRTP │                                │
└──────────────────────────────────────────────┘  over UDP  └───────────────────────────────┘
```

## Repository layout

| Path | What |
|---|---|
| `crates/protocol` | Control-channel messages (length-prefixed JSON), shared constants. |
| `crates/security` | Device identity (ECDSA P-256 + self-signed cert), pinned mutual TLS, SPAKE2 PIN pairing, trust store. |
| `crates/discovery` | mDNS/DNS-SD announcement and browsing (`_secondscrn._tcp`). |
| `crates/transport` | WebRTC media transport on [str0m](https://github.com/algesten/str0m): ICE, DTLS-SRTP, NACK/RTX, TWCC/GCC, bounded queues. |
| `crates/session` | Platform-independent `HostEngine`/`ClientEngine`, codec negotiation, bitrate/fps adaptation, reconnect backoff, settings, UI snapshots, platform traits, test fakes. |
| `crates/graphics` | BT.709/601 colour reference, test patterns, PSNR, Annex-B helpers. |
| `crates/diagnostics` | Log ring buffer, rate/EWMA helpers, redacted diagnostic export. |
| `crates/windows-host` | Windows platform: VDD, display configuration, WGC capture, GPU conversion, MF encoders, input, DPAPI, UniFFI API, dev CLI. |
| `crates/android-client` | Android platform: MediaCodec decoder, JNI surface hand-over, UniFFI API. |
| `crates/devtools` | Headless recording client for tests and manual network runs. |
| `crates/uniffi-bindgen` | UniFFI Kotlin generator (C# uses `uniffi-bindgen-cs`). |
| `apps/windows` | WinUI 3 application (Windows App SDK, H.NotifyIcon, Velopack). |
| `apps/android` | Jetpack Compose application (Gradle builds the Rust library and bindings). |
| `packaging` | Driver setup scripts, Windows release script. |

The engines contain all protocol, security and recovery logic once; the
platform crates only implement narrow traits (`HostPlatform`,
`ClientPlatform`). The same engines run in unit/integration tests with fake
media (`session::test_support`), which is why most of the system is testable
without Windows or Android.

## Connection lifecycle

1. **Discovery.** The host announces itself via mDNS with its name, port and
   certificate fingerprint. Discovery is untrusted convenience; manual
   connection (address + port) always works.
2. **Control channel.** The tablet opens TCP to the host (port 47800) and runs
   TLS 1.3 with client certificates. Neither side uses a CA: the handshake
   proves key possession; authorisation is by fingerprint. A paired tablet
   pins the host fingerprint at handshake time.
3. **Pairing** (first time). See [security.md](security.md): the tablet shows
   a 6-digit code, the PC user types it, SPAKE2 bound to the TLS exporter.
4. **Session request.** The tablet sends its surface size, refresh rate, DPI
   and verified decoders. The host negotiates the codec (HEVC > AV1 > H.264,
   hardware on both ends first), acquires the virtual monitor at the tablet's
   native mode, and creates the WebRTC offer.
5. **Media.** SDP is exchanged over the authenticated channel, so the DTLS
   fingerprints in it are authenticated too. str0m runs ICE over host
   candidates (one UDP socket per interface, plus the address the tablet used
   for TCP as an extra remote candidate, which makes NAT setups such as the
   Android emulator work).
6. **Streaming.** Input (mouse, touch frames, pen), statistics, keyframe
   requests, rotation, preset, resolution and frame-rate changes travel on the
   control channel; video on SRTP. Touch and pen are injected on Windows as
   synthetic pointer devices (real touch and pen), mouse with `SendInput`.
7. **End / recovery.** Either side pings every 2 s; 8 s of silence ends the
   session. The tablet reconnects with backoff (0.5 s … 8 s). The host keeps
   the virtual monitor for a linger period (default 30 s) so windows don't
   jump on short drops. A host shutdown tells the tablet to reconnect (app
   restarts/updates are seamless); a user disconnect does not.

## Video path (Windows)

```
VDD monitor ─(DWM)─► WGC frame (BGRA texture)
     ─► pixel shaders: Y plane + 2×2 box-filtered CbCr plane into NV12 render targets
     ─► Media Foundation hardware MFT (async, D3D11 device manager, low-latency CBR)
     ─► Annex-B / OBU access unit ─► str0m packetiser ─► SRTP/UDP
```

* Zero CPU copies of pixels; only the bitstream reaches the CPU.
* Frames are produced only when the screen changes (WGC semantics), paced to
  the target fps; while the encoder is busy only the newest frame is kept.
* After the screen settles, the last frame is re-encoded twice so static text
  converges to full quality; a 5 fps keepalive of repeated frames (~100 bytes
  each) follows, which keeps the Wi-Fi link awake for the next change.
* Keyframe requests within 500 ms of a keyframe are ignored (PLI, gap
  detection and the control channel usually report the same loss).
* UDP sockets use 8 MB buffers: a keyframe at tablet resolution leaves as one
  burst of several hundred kilobytes, and the Windows default (64 KB) dropped
  its tail locally. The bitrate control keeps them from filling (see
  Adaptation).
* Encoders are probed by configuring them (up to 3840×2160); encoders are bound
  to their GPU by PCI vendor ID. IddCx "shadow" adapters are ignored.
* GPUs count as integrated by D3D11's `UnifiedMemoryArchitecture`; dedicated
  memory size misleads (AMD APUs report 2 GB and more).
* A GPU's MFT only activates in a process Windows runs on that GPU, which it
  decides at start from the per-app graphics preference
  (`HKCU\Software\Microsoft\DirectX\UserGpuPreferences`): on hybrid laptops
  the integrated GPU, on desktops the GPU driving the screens; the other GPU's
  encoder then fails to start (E_UNEXPECTED, E_OUTOFMEMORY). By default
  (Automatic) the encoder of the GPU Windows runs SecondScrn on (DXGI's first
  adapter) is used, and Windows' graphics setting is the override. Choosing
  integrated or dedicated (Settings → Advanced) sets the preference (power
  saving / high performance) and asks for a restart when needed. The
  integrated GPU's encoder was about three times slower in a process set to
  the dedicated GPU.
* The encoder benchmark (Diagnostics → Performance test, `secondscrn-host-cli bench`) runs in
  the helper executable shipped next to the app, once per GPU kind with the
  matching graphics preference, on synthetic full-screen scrolling content.
* The stream never promises more frames than the monitor refreshes: if
  Windows resets the virtual monitor's refresh rate (it re-applies remembered
  modes on topology changes), the session restores it once, then follows it.
* Colour: BT.709 primaries/transfer/matrix, limited range, signalled in the
  bitstream (VUI/sequence header) and in the Android `MediaFormat`. The shader
  matrix is generated by `graphics::color`, which is unit-tested against
  BT.709 reference values and used to verify the GPU output (±1 code value).

## Video path (Android)

When the stream is smaller than the panel and Snapdragon GSR is on (Adreno
GPUs), the decoder renders into an `AImageReader` instead; a GL thread takes
the newest picture (`AHardwareBuffer` → EGLImage → external texture),
converts it to RGB and runs the SGSR 1 shader at panel resolution into the
`SurfaceView` (`android-client/src/upscale.rs`). Otherwise:

`TransportEvent::Frame` → decode queue → decode thread (`AMediaCodec`
input) → render thread releases output buffers to the `SurfaceView`'s
`ANativeWindow` immediately. Every received frame is decoded (dropping one
breaks the reference chain and costs a keyframe); when a burst makes several
pictures ready at once, only the newest is rendered. The `SurfaceHolder` is sized to
the stream resolution so pixels map 1:1. Surface changes (rotation,
background) bump a generation counter; the decoder is re-created and a
keyframe requested. Frames never pass through Kotlin.

## Adaptation

Priorities: low latency, no freezes, and adapting to whatever the network
allows in service of both. Media is sent **without str0m's GCC
estimator/pacer**; `session::adapt` sets the encoder target from the
tablet's own measurements instead:

* **Latency feedback.** Every 50 ms the tablet reports when each frame
  completed (`FrameTimings`: RTP time, arrival on its clock, size). The PC's
  `DelayMeter` turns that into queueing delay (one-way delay above its 30 s
  minimum, clock offset cancels out; the smallest over the last 100 ms, so one
  large frame isn't a queue) and into delivery and send rates over 500 ms
  (pauses between arrivals don't count as link time).
* **Control to a latency target.** A growing queue (delay > 30 ms and frames
  arriving slower than they were sent) cuts the budget within ~100 ms to what
  the link delivered, lower the longer the queue (it drains). A draining
  queue (after a cut, or retransmissions catching up after an outage) is not
  cut further. A raised but stable delay is probed with small cuts; if it
  stays, it is the path's (another access point, a dozing radio) and becomes
  the base. While the link is busy and unqueued the budget grows ~30 %/s.
  Loss alone is Wi-Fi noise (repaired by retransmission or recovery frames)
  and only cuts above 10 %. Without arrival reports (old tablet app) the
  previous RTT/loss AIMD applies.
* **No late frames.** The PC counts frames sent but not yet reported as
  arrived. When the oldest has waited 100 ms longer than reports usually
  take, the network is behind and the encoder skips captures (the next one
  shows the newest picture) instead of queueing frames that could only arrive
  late; keyframes and recovery frames are never held back. While reports stop
  (an outage) nothing is held back.
* Ceiling/floor derive from pixel rate × bits-per-pixel per preset and codec;
  the session starts at half the ceiling (≤ 25 Mbit/s). When even the floor
  doesn't fit, fps drops (to ≥ 15) before per-frame quality, and after 5 s
  both apps say that the network can't carry the chosen resolution and frame
  rate (`NetworkLimited`). Resolution and frame rate are never changed
  automatically; the user picks them (tablet, PC app or Windows settings) and
  the session follows: the monitor mode changes, the encoder restarts at the
  new size and the WebRTC session and tablet surface are kept.

Measured with `crates/session/tests/latency.rs` (loopback, simulated
bottleneck): a link dropping from 8 to 3 Mbit/s behind a 1 s buffer settled
at ~35 ms p95 latency (previously ~950 ms with a 6 s freeze) and recovered
full quality within 10 s of clearing (previously 2.2 of 6.5 Mbit/s).

Why not GCC: str0m paces at 1.1 × the GCC estimate. A desktop is idle most of
the time, so the estimate follows the idle send rate (~1 Mbit/s) and every
large frame after a pause then sat in the pacer for seconds. Measured on a
real tablet: 4–6 fps and ~1 Mbit/s before, 56–57 fps after removing the pacer.

**Loss recovery without keyframes** (`session::recovery`). A keyframe at
tablet resolution is large: it freezes the picture while it crosses the
network and on a struggling link causes the next loss. Encoders with
verified long-term reference support (`secondscrn-host-cli ltr-check`:
NVIDIA's H.264 and AV1; NVIDIA's HEVC accepts the settings but keeps
predicting from the previous frame) mark a reference picture every 500 ms;
the tablet reports which frames it decoded intact, confirming them. After a
loss the next frame predicts only from the newest confirmed reference: one
small frame, announced to the tablet (`Recovery`), which holds the frames
after the loss until it arrives. A recovery that doesn't take is retried
once, then a keyframe follows. Other encoders and codecs use keyframes.

Receive side: a 2048-packet reordering window (keyframes at tablet resolution
span hundreds of packets) and a 100 ms hole timeout; the sender keeps packets
for retransmission only that long (older ones could only arrive after the
receiver gave up, and after an outage would queue ahead of fresh frames).
RTP sequence gaps mark frames non-contiguous; the receiver asks for recovery
at once and the decoder holds the last good picture until a recovery frame or
keyframe arrives. The Windows host requests 1 ms timer
resolution (the default 15.6 ms capped frame pacing near 32 fps); the tablet
holds a low-latency Wi-Fi lock while streaming.

## Threads and FFI

* Rust owns two small tokio runtimes (host, client). Blocking platform work
  (display configuration, codec setup) runs on blocking threads; capture/
  encode, decode, render and input injection have dedicated threads.
* UIs receive immutable snapshots via a callback (host ≤ 4/s, client ≤ 10/s)
  and send commands. No video crosses the managed boundary.
* Bindings are generated: UniFFI → Kotlin (JNA) and UniFFI → C#
  (`uniffi-bindgen-cs`). The only hand-written FFI is one JNI function that
  hands the Android `Surface` to Rust.

## Decisions and deviations from the preferred stack

| Area | Preferred | Chosen | Why |
|---|---|---|---|
| Media transport | GStreamer `webrtcbin` | **str0m** (Rust WebRTC) on both ends | One WebRTC implementation shared by host and tablet, testable in-process (loopback tests run real ICE/DTLS-SRTP/NACK/TWCC in `cargo test`); pure-Rust crypto cross-compiles to Android without OpenSSL. GStreamer on Android would mean a large static build with poor Rust integration. |
| Windows capture/encode | GStreamer `d3d11screencapturesrc` + encoders | **WGC + D3D11 shader + Media Foundation** via `windows-rs` | Media Foundation exposes all vendors' hardware encoders through one API with D3D11 zero-copy input and runtime bitrate control, ships with Windows, and avoids bundling and licensing a ~100 MB LGPL runtime in the installer. The colour conversion is our own small shader so it can be verified bit-exactly against the reference. |
| Android decoding | MediaCodec | MediaCodec via the **NDK from Rust** (`ndk` crate) | As preferred; GStreamer on Android was evaluated and rejected (deployment size/complexity, no benefit with str0m). |
| Windows bindings | C ABI + generated P/Invoke | **UniFFI + uniffi-bindgen-cs** | One binding technology for both platforms; generated records, enums, errors and callbacks. |
| Android bindings | UniFFI | UniFFI (+1 JNI function) | As preferred. |
| Congestion control | WebRTC's (GCC/TWCC) | **Own latency-target control** from per-frame arrival reports on the control channel; NACK/RTX (age-limited) and PLI from str0m | str0m's GCC-coupled pacer adds seconds of latency for bursty desktop content (measured), and its TWCC feedback is only available through it. Frame-level reports measure exactly what matters (how late each picture is) and are testable end to end. |
| Virtual monitor removal | — | CCD path attach/detach | VDD keeps ≥1 monitor even with count 0; detaching the path from the desktop works without elevation. |
| WebRTC library | upstream crate | **str0m 0.24.1 vendored with one fix** (`vendor/str0m/SECONDSCRN-PATCH.md`) | Its HEVC depacketizer split multi-slice pictures into several frames. |

## Diagnostics

* Structured logs: host → `%LOCALAPPDATA%\SecondScrnData\logs` (daily, 7 kept) and
  an in-memory ring shown in the app; tablet → logcat (`SecondScrn` tag).
* Metrics distinguish capture (capture fps), encoder (encode fps/ms, skipped
  frames), network (send rate, estimate, RTT, loss, NACK/PLI), decoder
  (decode ms, dropped/late frames) and rendering (render fps), plus driver
  and pairing state.
* Diagnostic export is JSON with fingerprints, base64 blobs and PINs redacted.
