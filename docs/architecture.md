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
6. **Streaming.** Input, statistics, keyframe requests, rotation and preset
   changes travel on the control channel; video on SRTP.
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
  converges to full quality; a 1 fps keepalive frame follows.
* Encoders are probed by configuring them (up to 3840×2160); encoders are bound
  to their GPU by PCI vendor ID. IddCx "shadow" adapters are ignored.
* Colour: BT.709 primaries/transfer/matrix, limited range, signalled in the
  bitstream (VUI/sequence header) and in the Android `MediaFormat`. The shader
  matrix is generated by `graphics::color`, which is unit-tested against
  BT.709 reference values and used to verify the GPU output (±1 code value).

## Video path (Android)

`TransportEvent::Frame` → bounded decode queue (3) → decode thread
(`AMediaCodec` input) → render thread releases output buffers to the
`SurfaceView`'s `ANativeWindow` immediately. The `SurfaceHolder` is sized to
the stream resolution so pixels map 1:1. Surface changes (rotation,
background) bump a generation counter; the decoder is re-created and a
keyframe requested. Frames never pass through Kotlin.

## Adaptation

str0m's GCC estimate is mapped onto encoder bitrate and frame rate:

* Ceiling/floor derive from pixel rate × bits-per-pixel per preset and codec.
* Decreases only on congestion evidence (sending near the target, or loss
  > 2 %). An idle desktop produces an *application-limited* estimate that
  tracks the tiny send rate; reacting to it would make the next scroll blurry.
* When even the floor doesn't fit, fps drops (to ≥ 15) before per-frame
  quality; resolution never changes during a session.
* Increases are rate-limited (2 s) and need a ≥ 10 % change.

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
| Virtual monitor removal | — | CCD path attach/detach | VDD keeps ≥1 monitor even with count 0; detaching the path from the desktop works without elevation. |

## Diagnostics

* Structured logs: host → `%LOCALAPPDATA%\SecondScrnData\logs` (daily, 7 kept) and
  an in-memory ring shown in the app; tablet → logcat (`SecondScrn` tag).
* Metrics distinguish capture (capture fps), encoder (encode fps/ms, skipped
  frames), network (send rate, estimate, RTT, loss, NACK/PLI), decoder
  (decode ms, dropped/late frames) and rendering (render fps), plus driver
  and pairing state.
* Diagnostic export is JSON with fingerprints, base64 blobs and PINs redacted.
