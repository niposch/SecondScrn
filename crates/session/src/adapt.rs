//! Bitrate and frame-rate adaptation.
//!
//! The media stream is sent without str0m's GCC pacer (see
//! `docs/architecture.md`): str0m paces at 1.1 × the bandwidth estimate, and
//! for desktop content, which is idle most of the time, that estimate
//! collapses to the idle send rate. The first large frame after a pause then
//! queued in the pacer for seconds. Instead, frames leave immediately and
//! this module reacts to the RTCP feedback WebRTC already provides:
//!
//! * **Queueing delay**, measured per frame from the tablet's arrival reports
//!   ([`DelayMeter`], ~20 per second), is the main signal: once frames wait
//!   on the network, the budget drops to what the link actually delivered,
//!   below it while a queue drains, so the picture stays recent. While the
//!   link carries the stream without queueing, the budget grows (~30 % per
//!   second) up to the preset's ceiling.
//! * **Loss** (receiver reports, about once per second) cuts the budget as
//!   well; RTT (also once per second) only counts when no arrival reports
//!   come in.
//! * Resolution never changes: the Windows desktop layout must stay put.
//! * When even the floor bitrate doesn't fit, the frame rate drops (to ≥ 15)
//!   before per-frame quality, so text stays sharp.
//! * Idle periods are not congestion: without loss or delay the budget stays
//!   high, so the next scroll is encoded at full quality.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use secondscrn_protocol::{Codec, FrameTiming, QualityPreset};

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
/// Queueing delay (from arrival reports) above which the link is overused,
/// for at least `OVERUSE_FOR`, and below which it has room to grow.
const OVERUSE_DELAY: Duration = Duration::from_millis(30);
const OVERUSE_FOR: Duration = Duration::from_millis(50);
const UNDERUSE_DELAY: Duration = Duration::from_millis(10);
/// Raised delay that doesn't grow is probed once per this interval; above
/// the limit it is cut like a growing queue.
const STANDING_PROBE: Duration = Duration::from_secs(1);
const STANDING_LIMIT: Duration = Duration::from_millis(150);
/// Minimum time between delay-based cuts, unless the queue keeps growing.
const CUT_SPACING: Duration = Duration::from_millis(200);
/// After a delay-based cut, growth resumes this soon once the queue is gone
/// (the delay signal is precise; no need for the RTT path's caution).
const DELAY_GROWTH_HOLD: Duration = Duration::from_secs(1);
/// Share of the budget the stream must send for delay to mean congestion,
/// and how long it must have done so (the radio wakes up meanwhile).
const BUSY_SHARE: f64 = 0.4;
const WAKE_GRACE: Duration = Duration::from_millis(300);
/// Budget growth per second while the link is little used and nothing
/// indicates congestion.
const IDLE_GROWTH_PER_S: f64 = 0.15;
/// Budget growth per second of busy, unqueued link.
const DELAY_GROWTH_PER_S: f64 = 0.3;
/// Loss that cuts the budget even when delay shows no congestion, at most
/// this often.
const HEAVY_LOSS: f64 = 0.10;
const LOSS_CUT_SPACING: Duration = Duration::from_secs(2);
/// Without arrival reports for this long the RTT-based fallback applies.
const DELAY_FEEDBACK_STALE: Duration = Duration::from_secs(2);

/// Queueing delay and delivery rate on the media path, from the tablet's
/// per-frame arrival reports. The one-way delay of a frame (arrival on the
/// tablet's clock minus capture on the PC's) includes an unknown clock
/// offset; its minimum over [`BASE_WINDOW_S`] seconds is the delay of an
/// empty network, so the excess is time spent queued.
#[derive(Debug, Default)]
pub struct DelayMeter {
    /// Minimum one-way delay (µs, with the clock offset) per arrival second.
    mins: VecDeque<(u64, i64)>,
    /// Recent frames: arrival (µs), one-way delay (µs), bytes, capture (µs).
    recent: VecDeque<(u64, i64, u32, i64)>,
    /// A path delay accepted as the base (see [`Self::rebase`]) until the
    /// arrival time given.
    floor: Option<(i64, u64)>,
    /// After a pause in arrivals (an outage), frames captured before this
    /// time (µs) arrive late because they were retransmitted, not queued:
    /// they don't count as queueing delay.
    resumed_capture_us: i64,
}

/// Seconds over which the base (empty-network) delay is the minimum. Long
/// enough that a queue standing for many seconds still counts as one; clock
/// drift between the devices adds at most a millisecond or two.
const BASE_WINDOW_S: u64 = 30;
/// The queueing delay is the smallest of the frames arriving this recently
/// (a single large frame's own transmission time is not a queue).
const DELAY_WINDOW_US: u64 = 100_000;
/// Delivery rate is measured over this much of the recent arrivals.
const RATE_WINDOW_US: u64 = 500_000;
/// A pause in arrivals this long is an outage (frames are sent at least
/// every 200 ms, as keep-alives on a static screen).
const OUTAGE_GAP_US: u64 = 120_000;
/// Longest pause between two arrivals that counts as link time (a frame
/// interval at 30 fps, with margin).
const ARRIVAL_GAP_CAP_US: u64 = 40_000;

impl DelayMeter {
    pub fn add(&mut self, t: &FrameTiming) {
        let capture_us = (t.rtp_time as i64).saturating_mul(100) / 9; // 90 kHz -> µs
        let delay = t.arrival_us as i64 - capture_us;
        if let (Some(&(prev, ..)), Some(base)) = (self.recent.back(), self.base())
            && t.arrival_us.saturating_sub(prev) > OUTAGE_GAP_US
        {
            // Frames sent from now on show the queue again.
            self.resumed_capture_us = t.arrival_us as i64 - base;
        }
        let second = t.arrival_us / 1_000_000;
        match self.mins.back_mut() {
            Some((s, m)) if *s == second => *m = (*m).min(delay),
            _ => self.mins.push_back((second, delay)),
        }
        while self.mins.front().is_some_and(|(s, _)| s + BASE_WINDOW_S < second) {
            self.mins.pop_front();
        }
        self.recent.push_back((t.arrival_us, delay, t.bytes, capture_us));
        while self.recent.front().is_some_and(|(a, ..)| a + RATE_WINDOW_US < t.arrival_us) {
            self.recent.pop_front();
        }
        if self.floor.is_some_and(|(_, until)| t.arrival_us > until) {
            self.floor = None;
        }
    }

    fn base(&self) -> Option<i64> {
        let min = self.mins.iter().map(|(_, m)| *m).min()?;
        Some(self.floor.map_or(min, |(f, _)| f.max(min)))
    }

    /// How long the latest frames waited in network queues.
    pub fn queue_delay(&self) -> Option<Duration> {
        let base = self.base()?;
        let last = self.recent.back()?.0;
        let queued = self
            .recent
            .iter()
            .filter(|(a, _, _, c)| a + DELAY_WINDOW_US >= last && *c >= self.resumed_capture_us)
            .map(|(_, d, ..)| d - base)
            .min()?;
        Some(Duration::from_micros(queued.max(0) as u64))
    }

    /// The current delay is the path's, not a queue (it stayed although the
    /// stream sent less): measure queues against it from now on, for as long
    /// as the base window.
    pub fn rebase(&mut self) {
        let Some(&(last, ..)) = self.recent.back() else { return };
        let current = self.recent.iter().filter(|(a, ..)| a + DELAY_WINDOW_US >= last).map(|(_, d, ..)| *d).min();
        if let Some(d) = current {
            self.floor = Some((d, last + BASE_WINDOW_S * 1_000_000));
        }
    }

    /// Bits per second sent, for the recently arrived frames (by their
    /// capture times). Arrivals slower than this mean a queue is growing.
    pub fn send_bps(&self) -> Option<u64> {
        let (first, last) = (self.recent.front()?.3, self.recent.back()?.3);
        let span = last.saturating_sub(first);
        if span < 100_000 {
            return None;
        }
        let bytes: u64 = self.recent.iter().skip(1).map(|(_, _, b, _)| *b as u64).sum();
        Some(bytes * 8 * 1_000_000 / span as u64)
    }

    /// Bits per second that arrived recently. While a queue stands, this is
    /// what the link can carry. Pauses between arrivals (an idle desktop, an
    /// outage) count only up to [`ARRIVAL_GAP_CAP_US`]: the link was not
    /// carrying anything then, which says nothing about its capacity.
    pub fn delivery_bps(&self) -> Option<u64> {
        let mut arrivals = self.recent.iter().map(|(a, ..)| *a);
        let mut prev = arrivals.next()?;
        let mut busy = 0;
        for a in arrivals {
            busy += a.saturating_sub(prev).min(ARRIVAL_GAP_CAP_US);
            prev = a;
        }
        if busy < 50_000 {
            return None;
        }
        // The first frame's bytes arrived before the window began.
        let bytes: u64 = self.recent.iter().skip(1).map(|(_, _, b, _)| *b as u64).sum();
        Some(bytes * 8 * 1_000_000 / busy)
    }
}

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
            // Ceilings, not targets: the budget only grows this far while
            // the network reports neither loss nor queueing delay. Larger
            // frames also take longer to cross Wi-Fi, so a higher ceiling
            // trades latency for picture quality; Automatic stays at a level
            // measured to feel immediate on a real tablet, High quality
            // (`Sharp`) goes about twice as far.
            QualityPreset::Sharp => (0.30, 0.35),
            QualityPreset::Automatic => (0.14, 0.15),
            // Higher frame rate: a little less per pixel and frame.
            QualityPreset::Smooth => (0.10, 0.15),
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
    /// Delay-based control: since when the delay is above the threshold,
    /// the latest arrival report, the queue delay at the last cut.
    overuse_since: Option<Instant>,
    last_delay_report: Option<Instant>,
    cut_delay: Duration,
    /// Raised delay that doesn't grow: since when, and probes (small cuts)
    /// made to tell a standing queue (drains) from a longer path (doesn't).
    standing_since: Option<Instant>,
    standing_probes: u32,
    rebase: bool,
    last_loss_cut: Option<Instant>,
    /// Since when the stream uses the link (delay counts after a moment).
    busy_since: Option<Instant>,
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
            overuse_since: None,
            last_delay_report: None,
            cut_delay: Duration::ZERO,
            standing_since: None,
            standing_probes: 0,
            rebase: false,
            last_loss_cut: None,
            busy_since: None,
        }
    }

    /// The raised delay is the path's: the meter should take it as base.
    pub fn take_rebase(&mut self) -> bool {
        std::mem::take(&mut self.rebase)
    }

    fn delay_feedback(&self, now: Instant) -> bool {
        self.last_delay_report.is_some_and(|t| now.duration_since(t) < DELAY_FEEDBACK_STALE)
    }

    /// Feeds the queueing delay and the delivery and send rates of the
    /// recently arrived frames, from arrival reports (about 20 per second
    /// while frames flow). Returns a new encoder target when it should be
    /// applied.
    pub fn on_delay(
        &mut self,
        queued: Duration,
        delivery_bps: Option<u64>,
        send_bps: Option<u64>,
        now: Instant,
    ) -> Option<EncoderTarget> {
        self.last_delay_report = Some(now);
        let p = self.policy;
        // Delay only means congestion while the stream uses the link: frames
        // on an idle desktop (keep-alives, a moving cursor) meet a dozing
        // radio and arrive late without any queue, and so do the first large
        // frames when motion starts. Cutting then starved the stream (fewer
        // frames at full resolution). So: no delay judgement while the link
        // is little used, and only after it has been busy for a moment;
        // clearly bad delay always counts.
        let busy = send_bps.is_some_and(|s| s as f64 >= self.budget * BUSY_SHARE);
        if !busy {
            self.busy_since = None;
        } else if self.busy_since.is_none() {
            self.busy_since = Some(now);
        }
        let settled = self.busy_since.is_some_and(|t| now.duration_since(t) >= WAKE_GRACE);
        if !settled && queued <= HARD_DELAY {
            self.overuse_since = None;
            self.standing_since = None;
            // Idle periods are not congestion: the budget recovers (slowly)
            // so the next scroll starts at full quality.
            if !busy {
                self.grow(IDLE_GROWTH_PER_S, now);
            }
            let target = self.target_for(self.budget);
            return self.apply(target, false);
        }
        // Frames arriving slower than they were sent: the queue grows.
        // Faster: it drains (after a cut, or retransmitted frames catching
        // up after an outage); cutting more would only starve the stream.
        let (filling, draining) = match (delivery_bps, send_bps) {
            (Some(d), Some(s)) => ((d as f64) < s as f64 * 0.9, (d as f64) > s as f64 * 1.1),
            _ => (true, false),
        };
        if queued > OVERUSE_DELAY && draining {
            self.overuse_since = None;
            self.standing_since = None;
        } else if queued > OVERUSE_DELAY && !filling && queued <= STANDING_LIMIT {
            // Raised but stable delay: a standing queue, or a longer path
            // (another access point, a power-saving radio). A small cut
            // drains a queue; if the delay stays, it is the path's.
            self.overuse_since = None;
            let since = *self.standing_since.get_or_insert(now);
            if now.duration_since(since) >= STANDING_PROBE {
                self.standing_since = Some(now);
                if self.standing_probes < 2 {
                    self.standing_probes += 1;
                    self.budget = (self.budget * SOFT_CUT).max(p.floor as f64 * 0.25);
                    self.last_cut = Some(now);
                } else {
                    self.standing_probes = 0;
                    self.rebase = true;
                }
            }
        } else if queued > OVERUSE_DELAY {
            self.standing_since = None;
            let since = *self.overuse_since.get_or_insert(now);
            let spaced = self.last_cut.is_none_or(|t| now.duration_since(t) >= CUT_SPACING);
            // A queue still growing after a cut needs another one now.
            let worse = queued > self.cut_delay * 2;
            // A large queue is unambiguous: no need to confirm it.
            let confirmed = now.duration_since(since) >= OVERUSE_FOR || queued > OVERUSE_DELAY * 2;
            if confirmed && (spaced || worse) {
                // What the link carried, and less while the queue drains:
                // the longer it is, the faster.
                // (Measured over at least `WAKE_GRACE` of busy link, so the
                // delivery rate is the link's, not an idle desktop's.)
                let capacity = delivery_bps.map_or(self.budget, |d| d as f64).min(self.budget);
                let drain = 1.0 - queued.as_secs_f64().min(0.25) / 0.5;
                self.budget = (capacity * 0.9 * drain).max(p.floor as f64 * 0.25);
                self.last_cut = Some(now);
                self.cut_delay = queued;
                self.last_congestion = if queued > HARD_DELAY { Congestion::Severe } else { Congestion::Moderate };
            }
        } else {
            self.overuse_since = None;
            self.standing_since = None;
            self.standing_probes = 0;
            if queued < UNDERUSE_DELAY {
                self.grow(DELAY_GROWTH_PER_S, now);
            } else {
                self.last_growth = Some(now);
            }
        }
        let target = self.target_for(self.budget);
        self.apply(target, false)
    }

    /// Grows the budget by `per_s` per second since the last call (not
    /// within `DELAY_GROWTH_HOLD` of a cut).
    fn grow(&mut self, per_s: f64, now: Instant) {
        let held = self.last_cut.is_none_or(|t| now.duration_since(t) >= DELAY_GROWTH_HOLD);
        let ceiling = self.policy.ceiling as f64;
        if held && self.budget < ceiling {
            let dt = self.last_growth.map_or(0.05, |t| now.duration_since(t).as_secs_f64().min(0.25));
            self.budget = (self.budget * (1.0 + per_s * dt)).min(ceiling);
            self.last_congestion = Congestion::None;
        }
        self.last_growth = Some(now);
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
        let p = self.policy;
        // With arrival reports, congestion shows as queueing delay (handled
        // by `on_delay`, which also grows the budget). Loss alone is mostly
        // Wi-Fi noise, repaired by retransmission or recovery frames:
        // cutting for it starved the stream at 1-2 % random loss. Only heavy
        // loss cuts, in proportion (as WebRTC's loss controller does).
        if self.delay_feedback(now) {
            let loss = loss.unwrap_or(0.0) as f64;
            // At most every other report, and without holding back growth:
            // outages (Wi-Fi scans, interference) also show as heavy loss,
            // and starving the stream between them helps nothing.
            let spaced = self.last_loss_cut.is_none_or(|t| now.duration_since(t) >= LOSS_CUT_SPACING);
            if loss > HEAVY_LOSS && spaced {
                self.budget = (self.budget * (1.0 - 0.5 * loss)).max(p.floor as f64 * 0.25);
                self.last_loss_cut = Some(now);
                self.last_congestion = Congestion::Severe;
            }
            let target = self.target_for(self.budget);
            return self.apply(target, false);
        }
        let congestion = self.classify(loss, rtt, send_rate_bps, now);
        self.last_congestion = congestion;
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

    /// Feeds arrival reports every 50 ms for `ms`, from `t`.
    fn reports(c: &mut BitrateController, t: &mut Instant, ms: u64, queued: u64, sent: u64, delivered: u64) {
        for _ in 0..ms / 50 {
            *t += MS(50);
            c.on_delay(MS(queued), Some(delivered), Some(sent), *t);
        }
    }

    #[test]
    fn a_waking_radio_is_not_congestion() {
        let mut c = controller();
        let mut t = Instant::now();
        let start = c.budget_bps();
        // Still screen: keep-alives arrive late (dozing radio).
        reports(&mut c, &mut t, 2000, 60, IDLE, IDLE);
        assert!(c.budget_bps() > start, "idle must not cut; it recovers instead");
        let before = c.budget_bps();
        // Motion starts: the first large frames meet the waking radio.
        let busy = before * 9 / 10;
        reports(&mut c, &mut t, 250, 60, busy, busy * 7 / 10);
        assert_eq!(c.budget_bps(), before, "no cut while the radio wakes up");
        // It stays slow: real congestion, cut to about what the link carried.
        reports(&mut c, &mut t, 300, 60, busy, busy * 7 / 10);
        assert!(c.budget_bps() < before * 7 / 10 && c.budget_bps() >= before / 4, "{} of {before}", c.budget_bps());
    }

    #[test]
    fn clearly_bad_delay_cuts_at_once() {
        let mut c = controller();
        let mut t = Instant::now();
        let before = c.budget_bps();
        let busy = before * 9 / 10;
        reports(&mut c, &mut t, 100, 150, busy, busy / 3);
        assert!(c.budget_bps() < before / 2, "{} of {before}", c.budget_bps());
    }

    #[test]
    fn policy_scales_with_resolution_preset_and_codec() {
        let hd = BitratePolicy::for_stream(Codec::H265, 1920, 1080, 60, QualityPreset::Smooth, None);
        let qhd = BitratePolicy::for_stream(Codec::H265, 2560, 1600, 60, QualityPreset::Smooth, None);
        let sharp = BitratePolicy::for_stream(Codec::H265, 2560, 1600, 60, QualityPreset::Sharp, None);
        let h264 = BitratePolicy::for_stream(Codec::H264, 2560, 1600, 60, QualityPreset::Smooth, None);
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
