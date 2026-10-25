# Local patch to str0m 0.24.1

Upstream: https://github.com/algesten/str0m (MIT OR Apache-2.0), unmodified
except for `src/packet/h265.rs`, `H265Depacketizer::is_partition_tail`:

* upstream: a fragmentation unit with the E (end) flag ends a partition;
* patched: only the RTP marker bit does, as for H.264.

Why: hardware encoders split pictures into several slices (NVENC at faster
presets: two per picture at 3200x2136). Each slice is fragmented, so upstream
delivered every picture as two "frames"; the decoder treated each half as a
picture (ghost cursors, smeared blocks, low frame rate). Regression test:
`crates/transport/tests/h265_frames.rs` (multi-slice access units).

Remove this patch once upstream fixes it.
