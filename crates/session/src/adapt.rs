//! Bitrate and frame-rate adaptation.
//!
//! The media stream is sent without str0m's GCC pacer (see
//! `docs/architecture.md`): str0m paces at 1.1 × the bandwidth estimate, and
//! for desktop content, which is idle most of the time, that estimate
//! collapses to the idle send rate. The first large frame after a pause then
//! queued in the pacer for seconds. Instead, frames leave immediately and
//! this module reacts to the RTCP feedback WebRTC already provides:
//!
//! * **Loss** (receiver reports) and **queueing delay** (RTT above the recent
//!   minimum) cut a bandwidth budget multiplicatively; a clean link grows it
//!   by 15 % per second up to the preset's ceiling (AIMD).
//! * Resolution never changes: the Windows desktop layout must stay put.
//! * When even the floor bitrate doesn't fit, the frame rate drops (to ≥ 15)
//!   before per-frame quality, so text stays sharp.
//! * Idle periods are not congestion: without loss or delay the budget stays
//!   high, so the next scroll is encoded at full quality.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use secondscrn_protocol::{Codec, QualityPreset};

const ABSOLUTE_MIN_BPS: u64 = 1_000_000;
const ABSOLUTE_MAX_BPS: u64 = 150_000_000;
/// Lowest frame rate used when bandwidth is scarce.
const MIN_FPS: u32 = 15;
/// Loss / queueing delay that indicate moderate and severe congestion.
const SOFT_LOSS: f32 = 0.02;
const HARD_LOSS: f32 = 0.05;
/// Thresholds apply to the median of the last three RTT samples: Wi-Fi power
/// saving produces isolated RTT spikes that are not congestion.
const SOFT_DELAY: Duration = Duration::from_millis(40);
const HARD_DELAY: Duration = Duration::from_millis(100);
const RTT_MEDIAN_OF: usize = 3;
const SOFT_CUT: f64 = 0.9;
const HARD_CUT: f64 = 0.7;
const GROWTH: f64 = 1.15;
/// Clean feedback needed after a cut before growing again.
const GROWTH_HOLD: Duration = Duration::from_secs(2);
const MIN_RTT_WINDOW: Duration = Duration::from_secs(10);
/// Below this share of the budget the link is application-limited: radios
/// doze between sparse packets and RTT rises without any queueing, so delay
/// is ignored (loss still counts).
const DELAY_SIGNAL_MIN_LOAD: f64 = 0.25;
/// Encoder reconfigurations below this relative change are skipped.
const MIN_CHANGE_RATIO: f64 = 0.05;

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
        // Start at half the ceiling (capped): typical LAN Wi-Fi carries it,
        // and feedback corrects within a second or two if not.
        let initial = (ceiling / 2).clamp(floor, 25_000_000.max(floor));
        BitratePolicy { floor, ceiling, initial, max_fps: fps }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncoderTarget {
    pub bitrate_bps: u64,
    pub fps: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Congestion {
    None,
    Moderate,
    Severe,
}

#[derive(Debug)]
pub struct BitrateController {
    policy: BitratePolicy,
    /// Estimated usable bandwidth for video.
    budget: f64,
    current: EncoderTarget,
    rtts: VecDeque<(Instant, Duration)>,
    last_cut: Option<Instant>,
    last_growth: Option<Instant>,
    last_congestion: Congestion,
}

impl BitrateController {
    pub fn new(policy: BitratePolicy) -> Self {
        BitrateController {
            policy,
            budget: policy.initial as f64,
            current: EncoderTarget { bitrate_bps: policy.initial, fps: policy.max_fps },
            rtts: VecDeque::new(),
            last_cut: None,
            last_growth: None,
            last_congestion: Congestion::None,
        }
    }

    pub fn current(&self) -> EncoderTarget {
        self.current
    }

    pub fn policy(&self) -> BitratePolicy {
        self.policy
    }

    /// Current bandwidth budget (for diagnostics).
    pub fn budget_bps(&self) -> u64 {
        self.budget as u64
    }

    pub fn last_congestion(&self) -> Congestion {
        self.last_congestion
    }

    /// Replaces the policy (preset change). Returns the new target if it changed.
    pub fn set_policy(&mut self, policy: BitratePolicy) -> Option<EncoderTarget> {
        self.policy = policy;
        self.budget = self.budget.clamp(policy.floor as f64 * 0.25, policy.ceiling as f64);
        let target = self.target_for(self.budget);
        self.apply(target, true)
    }

    fn target_for(&self, budget: f64) -> EncoderTarget {
        let p = self.policy;
        let usable = budget as u64;
        // Scale the frame rate down when even the floor doesn't fit, so each
        // frame keeps its share of bits.
        let fps = if usable >= p.floor {
            p.max_fps
        } else {
            let ratio = usable as f64 / p.floor as f64;
            ((p.max_fps as f64 * ratio).round() as u32).clamp(MIN_FPS.min(p.max_fps), p.max_fps)
        };
        let fps_scale = fps as f64 / p.max_fps as f64;
        let bitrate = usable.clamp((p.floor as f64 * fps_scale) as u64, p.ceiling).max(ABSOLUTE_MIN_BPS / 2);
        EncoderTarget { bitrate_bps: bitrate, fps }
    }

    fn classify(&mut self, loss: Option<f32>, rtt: Option<Duration>, send_rate_bps: u64, now: Instant) -> Congestion {
        let mut delay = Duration::ZERO;
        if let Some(rtt) = rtt {
            self.rtts.push_back((now, rtt));
            while self.rtts.front().is_some_and(|(t, _)| now.duration_since(*t) > MIN_RTT_WINDOW) {
                self.rtts.pop_front();
            }
            let base = self.rtts.iter().map(|(_, r)| *r).min().unwrap_or(rtt);
            let mut recent: Vec<Duration> = self.rtts.iter().rev().take(RTT_MEDIAN_OF).map(|(_, r)| *r).collect();
            recent.sort();
            let typical = recent[recent.len() / 2];
            if send_rate_bps as f64 >= self.budget * DELAY_SIGNAL_MIN_LOAD {
                delay = typical.saturating_sub(base);
            }
        }
        let loss = loss.unwrap_or(0.0);
        if loss > HARD_LOSS || delay > HARD_DELAY {
            Congestion::Severe
        } else if loss > SOFT_LOSS || delay > SOFT_DELAY {
            Congestion::Moderate
        } else {
            Congestion::None
        }
    }

    /// Feeds one round of network feedback (about once per second): the loss
    /// fraction from receiver reports, the round-trip time and the measured
    /// send rate. Returns a new encoder target when it should be applied.
    pub fn on_feedback(
        &mut self,
        loss: Option<f32>,
        rtt: Option<Duration>,
        send_rate_bps: u64,
        now: Instant,
    ) -> Option<EncoderTarget> {
        let congestion = self.classify(loss, rtt, send_rate_bps, now);
        self.last_congestion = congestion;
        let p = self.policy;
        match congestion {
            Congestion::Severe | Congestion::Moderate => {
                let cut = if congestion == Congestion::Severe { HARD_CUT } else { SOFT_CUT };
                // Never cut below a quarter of the floor: fps scaling handles
                // the rest, and recovery must remain possible.
                self.budget = (self.budget * cut).max(p.floor as f64 * 0.25);
                self.last_cut = Some(now);
            }
            Congestion::None => {
                let held = self.last_cut.is_none_or(|t| now.duration_since(t) >= GROWTH_HOLD);
                let paced = self.last_growth.is_none_or(|t| now.duration_since(t) >= Duration::from_millis(900));
                if held && paced && self.budget < p.ceiling as f64 {
                    self.budget = (self.budget * GROWTH).min(p.ceiling as f64);
                    self.last_growth = Some(now);
                }
            }
        }
        let target = self.target_for(self.budget);
        self.apply(target, false)
    }

    fn apply(&mut self, target: EncoderTarget, force: bool) -> Option<EncoderTarget> {
        let cur = self.current;
        if target == cur {
            return None;
        }
        let ratio = (target.bitrate_bps as f64 - cur.bitrate_bps as f64).abs() / cur.bitrate_bps.max(1) as f64;
        if !force && target.fps == cur.fps && ratio < MIN_CHANGE_RATIO {
            return None;
        }
        self.current = target;
        Some(target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: fn(u64) -> Duration = Duration::from_millis;
    /// Send rates: actively streaming vs. an idle desktop.
    const BUSY: u64 = u64::MAX / 4;
    const IDLE: u64 = 50_000;

    fn controller() -> BitrateController {
        BitrateController::new(BitratePolicy::for_stream(Codec::H265, 2560, 1600, 60, QualityPreset::Automatic, None))
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
    fn clean_link_grows_to_ceiling_and_idle_never_reduces() {
        let mut c = controller();
        let t0 = Instant::now();
        let start = c.current().bitrate_bps;
        // An idle desktop gives clean feedback: quality only goes up.
        for i in 0..30 {
            c.on_feedback(Some(0.0), Some(MS(5)), IDLE, t0 + Duration::from_secs(i));
            assert!(c.current().bitrate_bps >= start);
        }
        assert_eq!(c.current().bitrate_bps, c.policy().ceiling);
        assert_eq!(c.current().fps, 60);
    }

    #[test]
    fn loss_cuts_multiplicatively_and_recovers_after_hold() {
        let mut c = controller();
        let t0 = Instant::now();
        let before = c.budget_bps();
        let t = c.on_feedback(Some(0.08), Some(MS(5)), BUSY, t0).expect("severe loss reduces");
        assert_eq!(c.last_congestion(), Congestion::Severe);
        assert!((c.budget_bps() as f64 - before as f64 * HARD_CUT).abs() < 2.0);
        assert!(t.bitrate_bps < before);
        // No growth during the hold time…
        assert!(c.on_feedback(Some(0.0), Some(MS(5)), BUSY, t0 + MS(1000)).is_none());
        // …then it grows.
        assert!(c.on_feedback(Some(0.0), Some(MS(5)), BUSY, t0 + MS(2500)).is_some());
        // Moderate loss: smaller cut.
        let b = c.budget_bps();
        c.on_feedback(Some(0.03), Some(MS(5)), BUSY, t0 + MS(3500));
        assert_eq!(c.last_congestion(), Congestion::Moderate);
        assert!((c.budget_bps() as f64 - b as f64 * SOFT_CUT).abs() < 2.0);
    }

    #[test]
    fn isolated_rtt_spikes_are_not_congestion() {
        let mut c = controller();
        let t0 = Instant::now();
        // Wi-Fi power save: 7 ms with an occasional 60 ms spike.
        for (i, rtt) in [7, 7, 60, 7, 8, 58, 7].into_iter().enumerate() {
            c.on_feedback(Some(0.0), Some(MS(rtt)), BUSY, t0 + Duration::from_secs(i as u64));
            assert_eq!(c.last_congestion(), Congestion::None, "sample {i}");
        }
    }

    #[test]
    fn idle_rtt_inflation_is_not_congestion() {
        // Seen on a real tablet: while the desktop is idle the radio dozes
        // between sparse packets and RTT jumps between 4 and 55 ms.
        let mut c = controller();
        let t0 = Instant::now();
        let before = c.budget_bps();
        for (i, rtt) in [4, 55, 57, 4, 56, 58, 55].into_iter().enumerate() {
            c.on_feedback(Some(0.0), Some(MS(rtt)), IDLE, t0 + Duration::from_secs(i as u64));
            assert_eq!(c.last_congestion(), Congestion::None, "sample {i}");
        }
        assert!(c.budget_bps() >= before);
        // Loss still counts when idle.
        c.on_feedback(Some(0.1), Some(MS(5)), IDLE, t0 + Duration::from_secs(10));
        assert_eq!(c.last_congestion(), Congestion::Severe);
    }

    #[test]
    fn queueing_delay_is_congestion_even_without_loss() {
        let mut c = controller();
        let t0 = Instant::now();
        c.on_feedback(Some(0.0), Some(MS(8)), BUSY, t0);
        c.on_feedback(Some(0.0), Some(MS(60)), BUSY, t0 + MS(1000));
        c.on_feedback(Some(0.0), Some(MS(60)), BUSY, t0 + MS(2000));
        assert_eq!(c.last_congestion(), Congestion::Moderate);
        c.on_feedback(Some(0.0), Some(MS(150)), BUSY, t0 + MS(3000));
        c.on_feedback(Some(0.0), Some(MS(150)), BUSY, t0 + MS(4000));
        assert_eq!(c.last_congestion(), Congestion::Severe);
        // A constant high RTT (e.g. a slow but uncongested path) becomes the
        // new baseline once the old minimum leaves the window.
        for i in 5..22 {
            c.on_feedback(Some(0.0), Some(MS(150)), BUSY, t0 + Duration::from_secs(i));
        }
        assert_eq!(c.last_congestion(), Congestion::None);
    }

    #[test]
    fn scarce_bandwidth_reduces_fps_not_resolution() {
        let mut c = controller();
        let t0 = Instant::now();
        let floor = c.policy().floor;
        for i in 0..20 {
            c.on_feedback(Some(0.2), None, BUSY, t0 + Duration::from_secs(i));
        }
        let t = c.current();
        assert!(t.fps < 60 && t.fps >= MIN_FPS, "{t:?}");
        // Per-frame bits stay close to the floor's per-frame budget.
        let per_frame = t.bitrate_bps / t.fps as u64;
        assert!(per_frame * 10 >= (floor / 60) * 8, "{per_frame}");
        // Budget never collapses to zero: recovery stays possible.
        assert!(c.budget_bps() >= floor / 4);
        for i in 20..60 {
            c.on_feedback(Some(0.0), None, BUSY, t0 + Duration::from_secs(i));
        }
        assert_eq!(c.current().fps, 60);
    }

    #[test]
    fn small_changes_do_not_reconfigure_the_encoder() {
        let mut c = controller();
        let t0 = Instant::now();
        for i in 0..40 {
            c.on_feedback(Some(0.0), Some(MS(5)), IDLE, t0 + Duration::from_secs(i));
        }
        // At the ceiling, clean feedback changes nothing.
        assert!(c.on_feedback(Some(0.0), Some(MS(5)), BUSY, t0 + Duration::from_secs(41)).is_none());
    }

    #[test]
    fn preset_change_reclamps() {
        let mut c = controller();
        let t0 = Instant::now();
        for i in 0..40 {
            c.on_feedback(Some(0.0), None, BUSY, t0 + Duration::from_secs(i));
        }
        let low = BitratePolicy::for_stream(Codec::H265, 2560, 1600, 30, QualityPreset::LowBandwidth, None);
        let t = c.set_policy(low).unwrap();
        assert_eq!(t.bitrate_bps, low.ceiling);
        assert_eq!(t.fps, 30);
    }
}
