# Testing

## Automated (run on every change)

`cargo test` runs without GPU, driver or devices:

| Area | Where | What |
|---|---|---|
| Protocol | `protocol` | message round trips, stable wire format, unknown fields, frame limits, EOF |
| Security | `security` | identity persistence/corruption, mTLS fingerprints + matching exporters, host pinning, SPAKE2 success/wrong PIN/MITM (different exporters)/identity binding/forged confirmation, trust store |
| Transport | `transport/tests/loopback.rs` | real ICE + DTLS-SRTP over loopback: H.264 frames byte-exact incl. fragmentation and RTP timing, HEVC keyframe requests, peer-loss detection |
| Negotiation, adaptation, backoff, settings | `session` unit tests | codec choice, size limits, fps limits, bitrate policy, idle (app-limited) handling, congestion, presets, persistence and corruption recovery |
| Engines | `session/tests/engines.rs` | host+client over loopback with fake media: pairing, streaming, ordered input, disconnect, PIN-less reconnect, wrong PIN, declined pairing, revocation, host restart → automatic reconnect, unpaired client rejected |
| Colour | `graphics` | BT.709/601 reference values, round-trip error bound, NV12 layout |
| Network impairment | `transport/tests/impairment.rs` | random loss, burst loss, delay/jitter, bandwidth cap, temporary outage (see below) |
| Diagnostics | `diagnostics` | redaction of fingerprints/PINs/base64, bounded logs |

Windows-only tests (`cargo test -p secondscrn-windows-host`): GPU NV12
conversion equals the CPU reference (±1), DPAPI round trip and tamper check,
VDD settings editing.

Hardware tests, opt-in (`-- --ignored`): `windows-host/tests/e2e.rs` creates
the virtual monitor, streams HEVC through real WebRTC to a recording client,
validates the bitstream with ffprobe (resolution, BT.709, limited range),
checks that no physical display was deactivated, and verifies input mapping on
the virtual monitor. `discovery` has a real multicast announce/browse test.

## Android emulator workflow

```powershell
# one-time: tablet AVD (2560x1600, 320 dpi) next to the stock images
emulator -avd SecondScrn_Tablet -gpu host
cd apps/android; ./gradlew installDebug
target/debug/secondscrn-host-cli serve --pin-file $env:TEMP\pin.txt
```

In the app: *Connect manually* → `10.0.2.2` (the emulator's alias for the PC).
Read the code (`adb shell uiautomator dump`), write it to the pin file.
Logs: `adb logcat -s SecondScrn`.

Emulator networking: user-mode NAT; the PC sees the tablet at `127.0.0.1`.
The host offers a loopback ICE candidate and the tablet adds the address it
used for TCP as a remote candidate, so media flows through the NAT. mDNS does
not cross the emulator NAT; use manual connection. The NAT drops bursts of
UDP, so loss figures in the emulator are pessimistic.

Verified in the emulator (2026-10-24): pairing (headless host and WinUI
dialog), 2560×1600 HEVC streaming from the virtual monitor with the goldfish
decoder, session overlay, touch → mouse input, automatic reconnection after
killing the host, cleanup of the orphaned monitor, host-identity-change
detection.

## Physical tablet validation (not yet performed)

Emulator results say nothing about hardware decoding performance. On a real
tablet, record for each item the device, Android version, Wi-Fi standard and
access point:

| Item | Procedure | Pass criteria |
|---|---|---|
| Hardware decoding | Troubleshooting card lists decoders; logcat `decoder started` | HEVC (or AV1) vendor decoder (not `c2.android.*`) |
| Codecs | force each codec in PC Streaming settings | each listed codec streams |
| Native resolution | compare PC Display settings with tablet panel | identical; text pixel-sharp at 100% zoom |
| Text clarity | open `docs/test-pattern.html` on the tablet monitor | 1-px black/white lines distinct; red/blue lines legible |
| Colour | test pattern colour bars vs. same pattern on a calibrated monitor | no visible shift; greys neutral; black 0/white 255 (no washed-out range error) |
| Smoothness | play 60 fps video, drag windows | ≥ 55 fps rendered (overlay), no stutter > 100 ms |
| Latency | high-speed camera: PC cursor vs. tablet | median < 50 ms on 5 GHz Wi-Fi |
| Wi-Fi | 2.4 / 5 / 6 GHz, 1 and 2 walls | stays connected; quality adapts |
| Battery | 1 h static desktop, 1 h video | report %/h |
| Thermal | 30 min video | no throttling warnings, fps stable |
| Sleep/resume | tablet screen off 30 s / 5 min; PC sleep | reconnects automatically |
| Surface lifecycle | rotate, split-screen, home → back | stream recovers within 1 s |

## Network impairment

`crates/transport/tests/impairment.rs` drives a sender/receiver pair through
an in-process impairment layer (deterministic seed) applied to outgoing UDP:
random loss (5 %), burst loss, added delay with jitter, a bandwidth cap, and a
complete outage of a few seconds. Assertions: frames keep arriving under loss
(NACK/RTX repair and keyframe recovery), the bandwidth estimate stays below a
cap, the connection reports *Interrupted* during an outage and *Resumed* after
it without a reconnect. For manual tests on real networks, run the host and
`secondscrn-dev-client` with `clumsy` (Windows) or `tc netem` (Linux) and watch
the Diagnostics page.
