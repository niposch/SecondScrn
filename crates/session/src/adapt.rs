//! Bitrate and frame-rate adaptation.
//!
//! Congestion control itself (GCC over TWCC) lives in str0m. This module maps
//! its estimate onto encoder settings with desktop-specific priorities:
//!
//! 1. Never change the resolution: the Windows desktop layout must stay put.
//! 2. Keep per-frame quality (text sharpness) high; when bandwidth runs out,
//!    reduce the frame rate before starving each frame of bits.
//! 3. Back off quickly, recover slowly (hysteresis), so the encoder isn't
//!    reconfigured on every estimate.
//! 4. Desktop traffic is bursty: an idle screen sends almost nothing, and
//!    the estimate then follows the tiny send rate. Such an application-
//!    limited estimate is not a congestion signal, so it never lowers the
//!    target; only estimates below what is actually being sent, or packet
//!    loss, do. Otherwise the first scroll after a pause would be encoded at
//!    an idle-level bitrate.

use std::time::{Duration, Instant};

use secondscrn_protocol::{Codec, QualityPreset};

/// Share of the estimate given to the encoder; the rest is headroom for
/// retransmissions, RTCP and estimate noise.
const ESTIMATE_HEADROOM: f64 = 0.85;
const MIN_INCREASE_INTERVAL: Duration = Duration::from_secs(2);
const MIN_CHANGE_RATIO: f64 = 0.10;
const ABSOLUTE_MIN_BPS: u64 = 1_000_000;
const ABSOLUTE_MAX_BPS: u64 = 150_000_000;
/// Lowest frame rate used when bandwidth is scarce.
const MIN_FPS: u32 = 15;
/// Sending below this share of the target counts as application-limited.
const APP_LIMITED_RATIO: f64 = 0.5;
/// Loss above this is treated as congestion even when application-limited.
const CONGESTION_LOSS: f32 = 0.02;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BitratePolicy {
    pub floor: u64,
    pub ceiling: u64,
    pub initial: u64,
    pub max_fps: u32,
}

impl BitratePolicy {
    /// Derives limits from the pixel rate. Bits-per-pixel values are tuned for
    /// desktop content, where most frames are small updates and text needs
    /// generous bits on keyframes and scrolls.
    pub fn for_stream(
        codec: Codec,
        width: u32,
        height: u32,
        fps: u32,
        preset: QualityPreset,
        user_max_bps: Option<u64>,
    ) -> Self {
        let pixel_rate = width as f64 * height as f64 * fps.max(1) as f64;
        let (bpp, floor_ratio) = match preset {
            QualityPreset::Sharp => (0.16, 0.35),
            QualityPreset::Automatic => (0.14, 0.15),
            QualityPreset::Balanced => (0.10, 0.15),
            QualityPreset::LowBandwidth => (0.05, 0.10),
        };
        let codec_factor = match codec {
            Codec::H264 => 1.4,
            Codec::H265 | Codec::Av1 => 1.0,
        };
        let mut ceiling = ((pixel_rate * bpp * codec_factor) as u64).clamp(ABSOLUTE_MIN_BPS * 2, ABSOLUTE_MAX_BPS);
        if let Some(max) = user_max_bps.filter(|m| *m > 0) {
            ceiling = ceiling.min(max.max(ABSOLUTE_MIN_BPS));
        }
        let floor = ((ceiling as f64 * floor_ratio) as u64).max(ABSOLUTE_MIN_BPS).min(ceiling);
        // Start conservatively; BWE ramps up within a few seconds on a LAN.
        let initial = (ceiling / 2).clamp(floor, 12_000_000.max(floor));
        BitratePolicy { floor, ceiling, initial, max_fps: fps }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncoderTarget {
    pub bitrate_bps: u64,
    pub fps: u32,
}

#[derive(Debug)]
pub struct BitrateController {
    policy: BitratePolicy,
    current: EncoderTarget,
    last_increase: Option<Instant>,
    send_rate_bps: u64,
    loss: f32,
}

impl BitrateController {
    pub fn new(policy: BitratePolicy) -> Self {
        BitrateController {
            policy,
            current: EncoderTarget { bitrate_bps: policy.initial, fps: policy.max_fps },
            last_increase: None,
            send_rate_bps: 0,
            loss: 0.0,
        }
    }

    pub fn current(&self) -> EncoderTarget {
        self.current
    }

    pub fn policy(&self) -> BitratePolicy {
        self.policy
    }

    /// Replaces the policy (preset change). Returns the new target if it changed.
    pub fn set_policy(&mut self, policy: BitratePolicy) -> Option<EncoderTarget> {
        self.policy = policy;
        let target = EncoderTarget {
            bitrate_bps: self.current.bitrate_bps.clamp(policy.floor, policy.ceiling),
            fps: self.current.fps.min(policy.max_fps).max(MIN_FPS.min(policy.max_fps)),
        };
        self.apply(target)
    }

    /// The bitrate str0m's BWE should probe towards.
    pub fn desired_bitrate(&self) -> u64 {
        self.policy.ceiling
    }

    /// Updates the measured send rate and loss (about once per second).
    pub fn on_network(&mut self, send_rate_bps: u64, loss: Option<f32>) {
        self.send_rate_bps = send_rate_bps;
        self.loss = loss.unwrap_or(0.0);
    }

    fn congested(&self) -> bool {
        let app_limited = (self.send_rate_bps as f64) < self.current.bitrate_bps as f64 * APP_LIMITED_RATIO;
        self.loss > CONGESTION_LOSS || !app_limited
    }

    /// Feeds a new bandwidth estimate. Returns a new encoder target when it
    /// should be applied.
    pub fn on_estimate(&mut self, estimate_bps: u64, now: Instant) -> Option<EncoderTarget> {
        let usable = (estimate_bps as f64 * ESTIMATE_HEADROOM) as u64;
        let p = self.policy;

        // Frame rate: scale down proportionally when even the floor bitrate
        // doesn't fit, so each frame keeps its share of bits.
        let fps = if usable >= p.floor {
            p.max_fps
        } else {
            let ratio = usable as f64 / p.floor as f64;
            ((p.max_fps as f64 * ratio).round() as u32).clamp(MIN_FPS.min(p.max_fps), p.max_fps)
        };
        let fps_scale = fps as f64 / p.max_fps as f64;
        let bitrate = usable.clamp((p.floor as f64 * fps_scale) as u64, p.ceiling).max(ABSOLUTE_MIN_BPS / 2);

        let cur = self.current;
        let increasing = bitrate > cur.bitrate_bps || fps > cur.fps;
        if increasing {
            let significant = bitrate as f64 > cur.bitrate_bps as f64 * (1.0 + MIN_CHANGE_RATIO) || fps > cur.fps;
            let rate_limited = self.last_increase.is_some_and(|t| now.duration_since(t) < MIN_INCREASE_INTERVAL);
            if !significant || rate_limited {
                return None;
            }
            self.last_increase = Some(now);
        } else {
            let significant =
                (bitrate as f64) < cur.bitrate_bps as f64 * (1.0 - MIN_CHANGE_RATIO / 2.0) || fps < cur.fps;
            if !significant || !self.congested() {
                return None;
            }
        }
        self.apply(EncoderTarget { bitrate_bps: bitrate, fps })
    }

    fn apply(&mut self, target: EncoderTarget) -> Option<EncoderTarget> {
        if target == self.current {
            return None;
        }
        self.current = target;
        Some(target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A controller that is actively sending at its target (not app-limited).
    fn controller() -> BitrateController {
        let mut c = BitrateController::new(BitratePolicy::for_stream(
            Codec::H265,
            2560,
            1600,
            60,
            QualityPreset::Automatic,
            None,
        ));
        c.on_network(u64::MAX / 2, Some(0.0));
        c
    }

    #[test]
    fn idle_low_estimates_do_not_reduce_quality() {
        let mut c = BitrateController::new(BitratePolicy::for_stream(
            Codec::H265,
            2560,
            1600,
            60,
            QualityPreset::Automatic,
            None,
        ));
        let initial = c.current();
        // Static desktop: little is sent and the estimate follows it down.
        c.on_network(700_000, Some(0.0));
        assert!(c.on_estimate(780_000, Instant::now()).is_none());
        assert_eq!(c.current(), initial);
        // Loss is congestion even when idle.
        c.on_network(700_000, Some(0.08));
        assert!(c.on_estimate(780_000, Instant::now()).is_some());
    }

    #[test]
    fn policy_scales_with_resolution_preset_and_codec() {
        let hd = BitratePolicy::for_stream(Codec::H265, 1920, 1080, 60, QualityPreset::Balanced, None);
        let qhd = BitratePolicy::for_stream(Codec::H265, 2560, 1600, 60, QualityPreset::Balanced, None);
        let sharp = BitratePolicy::for_stream(Codec::H265, 2560, 1600, 60, QualityPreset::Sharp, None);
        let h264 = BitratePolicy::for_stream(Codec::H264, 2560, 1600, 60, QualityPreset::Balanced, None);
        assert!(hd.ceiling < qhd.ceiling && qhd.ceiling < sharp.ceiling && qhd.ceiling < h264.ceiling);
        assert!(sharp.floor > qhd.floor);
        for p in [hd, qhd, sharp, h264] {
            assert!(p.floor <= p.initial && p.initial <= p.ceiling, "{p:?}");
        }
        let capped = BitratePolicy::for_stream(Codec::H265, 2560, 1600, 60, QualityPreset::Sharp, Some(10_000_000));
        assert_eq!(capped.ceiling, 10_000_000);
    }

    #[test]
    fn decreases_are_immediate_increases_are_rate_limited() {
        let mut c = controller();
        let t0 = Instant::now();
        let low = c.on_estimate(10_000_000, t0).expect("drop applies");
        assert_eq!(low.bitrate_bps, 8_500_000);
        // Increase allowed once...
        assert!(c.on_estimate(30_000_000, t0 + Duration::from_millis(100)).is_some());
        // ...but not again within the interval.
        assert!(c.on_estimate(40_000_000, t0 + Duration::from_millis(500)).is_none());
        assert!(c.on_estimate(40_000_000, t0 + Duration::from_secs(3)).is_some());
        // Another drop is immediate.
        assert!(c.on_estimate(5_000_000, t0 + Duration::from_secs(3) + Duration::from_millis(10)).is_some());
    }

    #[test]
    fn small_changes_are_ignored() {
        let mut c = controller();
        let t0 = Instant::now();
        c.on_estimate(20_000_000, t0);
        assert!(c.on_estimate(20_500_000, t0 + Duration::from_secs(10)).is_none());
        assert!(c.on_estimate(19_800_000, t0 + Duration::from_secs(10)).is_none());
    }

    #[test]
    fn scarce_bandwidth_reduces_fps_not_resolution() {
        let mut c = controller();
        let floor = c.policy().floor;
        let t = c.on_estimate(floor / 2, Instant::now()).unwrap();
        assert!(t.fps < 60 && t.fps >= MIN_FPS, "{t:?}");
        // Per-frame bits stay close to the floor's per-frame budget.
        let per_frame = t.bitrate_bps / t.fps as u64;
        let floor_per_frame = floor / 60;
        assert!(per_frame * 10 >= floor_per_frame * 8, "{per_frame} vs {floor_per_frame}");

        let t2 = c.on_estimate(floor * 4, Instant::now() + Duration::from_secs(5)).unwrap();
        assert_eq!(t2.fps, 60);
    }

    #[test]
    fn never_exceeds_ceiling() {
        let mut c = controller();
        let t = c.on_estimate(10_000_000_000, Instant::now()).unwrap();
        assert_eq!(t.bitrate_bps, c.policy().ceiling);
    }

    #[test]
    fn preset_change_reclamps() {
        let mut c = controller();
        c.on_estimate(100_000_000, Instant::now());
        let low = BitratePolicy::for_stream(Codec::H265, 2560, 1600, 30, QualityPreset::LowBandwidth, None);
        let t = c.set_policy(low).unwrap();
        assert_eq!(t.bitrate_bps, low.ceiling);
        assert_eq!(t.fps, 30);
    }
}
