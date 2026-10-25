# Performance validation for 0.0.1

Run on 2026-10-25 with scrcpy stopped: Windows 11, NVIDIA RTX 4080 SUPER,
Xiaomi Pad 7 Pro (3200×2136, Android 16 / API 36), local Wi-Fi, HEVC.
The Windows host and Android Rust core used optimized release builds; the
physical tablet used the debug UI APK to retain its existing debug-signed pairings.

The workload is the same 1400×900 moving-box animation used in the earlier
local tests. Each run animates for 20 seconds after mode changes settle.
The table uses the steady portion of the reported two-second metrics.
One 50 % / 60 fps sample overlapped a decoder restart and was repeated with
a longer settling period; only the valid repeat is included.

| Resolution | Requested fps | Reported tablet fps | Encode ms | RTT ms | Loss | Stutters in sample |
|---|---:|---:|---:|---:|---:|---:|
| 100 % (3200x2136) | 60 | 59.5 | 5.8 | 15.4 | 0.0 % | 0 / ~14 s |
| 100 % (3200x2136) | 90 | 90.0 | 5.3 | 14.4 | 0.0 % | 2 / ~16 s |
| 100 % (3200x2136) | 120 | 117.5 | 4.9 | 12.5 | 0.0 % | 5 / ~14 s |
| 100 % (3200x2136) | 144 | 141.1 | 5.0 | 10.2 | 0.0 % | 8 / ~14 s |
| 75 % (2400x1602) | 60 | 60.0 | 6.2 | 15.4 | 0.0 % | 1 / ~8 s |
| 75 % (2400x1602) | 120 | 118.5 | 5.3 | 10.0 | 0.0 % | 0 / ~14 s |
| 50 % (1600x1068) | 60 | 59.9 | 3.0 | 13.5 | 0.0 % | 0 / ~14 s |
| 50 % (1600x1068) | 120 | 119.9 | 2.8 | 11.0 | 0.0 % | 2 / ~14 s |
| 50 % (1600x1068) | 144 | 143.0 | 2.8 | 9.2 | 0.0 % | 4 / ~14 s |

All nine cases reported 0 % loss. Native resolution reached about 118 fps
at a 120 fps setting and 141 fps at 144; the earlier ~90 fps result with
scrcpy running must not be treated as this tablet's throughput ceiling.
At 75 % / 120 fps, the reported rate was 118.5 fps with no recorded stutters
in the steady sample. Results vary with workload, Wi-Fi and device temperature.

These are short animation samples and app diagnostics, not camera-measured
end-to-end latency, a full-screen video stress test, or thermal/battery validation.
RTT is network round-trip time, not screen-to-screen latency. Stutters are
decoder diagnostics, not a subjective guarantee of perfect visual pacing.

Raw summarized data: [performance-0.0.1.json](performance-0.0.1.json).
Reproduction: start the headless release host with `--pin-file`, connect the
paired tablet, then run `python tools/performance-matrix.py --pin-file PATH --log HOST_LOG`.
The tool shows a temporary animation on the tablet display and restores the
configured resolution/FPS afterwards. Confirm its placement check matches
your monitor arrangement before running; change restore arguments for your setup.
