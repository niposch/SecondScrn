//! Loss recovery with long-term references instead of keyframes.
//!
//! A keyframe at tablet resolution is large: after a loss it takes a while
//! to cross the network (the picture freezes meanwhile) and on a struggling
//! link it causes the next loss. Encoders that support long-term references
//! keep a few marked pictures; after a loss the next frame predicts only
//! from one the tablet confirmed it has, so it is small and decodes although
//! the frames in between never arrived.
//!
//! The host marks a picture every [`MARK_INTERVAL`] in alternating slots.
//! The tablet reports the range of frames it decoded with intact references;
//! a marked picture in that range is confirmed. On a loss report the newest
//! confirmed picture is used; without one, or when a recovery didn't take,
//! a keyframe.

use std::time::{Duration, Instant};

use crate::platform::FrameReference;

/// Reference slots used (the encoder keeps this many marked pictures).
pub const SLOTS: usize = 2;
/// How often a new reference picture is marked: the older the reference,
/// the larger the recovery frame.
const MARK_INTERVAL: Duration = Duration::from_millis(500);
/// Loss reports this soon after a recovery describe the same loss (the
/// tablet signals it several ways); later ones mean it didn't take.
const SETTLE: Duration = Duration::from_millis(250);
/// Recovery frames tried for one loss before a keyframe.
const RECOVERY_ATTEMPTS: u32 = 2;
/// A recovery frame still on its way (queued behind retransmissions after an
/// outage, say) is waited for this long: a keyframe would only arrive later.
const RECOVERY_IN_FLIGHT_MAX: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Encode the next frame from this reference slot.
    Recover(u32),
    Keyframe,
}

#[derive(Debug, Clone, Copy)]
struct Slot {
    rtp: u64,
    confirmed: bool,
}

#[derive(Debug)]
pub struct References {
    enabled: bool,
    slots: [Option<Slot>; SLOTS],
    /// The slot marked most recently (the next mark goes to another one).
    newest: Option<usize>,
    last_mark_request: Option<Instant>,
    /// The latest recovery: when it was requested, its frame once sent, and
    /// how many attempts were made for this loss.
    recovery: Option<(Instant, Option<u64>, u32)>,
    last_keyframe_request: Option<Instant>,
    /// Latest RTP time the tablet reported as arrived.
    arrived: Option<u64>,
}

impl References {
    pub fn new(enabled: bool) -> Self {
        References {
            enabled,
            slots: [None; SLOTS],
            newest: None,
            last_mark_request: None,
            recovery: None,
            last_keyframe_request: None,
            arrived: None,
        }
    }

    /// The tablet received the frames up to `rtp`.
    pub fn on_arrived(&mut self, rtp: u64) {
        self.arrived = Some(self.arrived.map_or(rtp, |a| a.max(rtp)));
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// The slot to mark the next frame in, when a new reference is due.
    pub fn due_mark(&mut self, now: Instant) -> Option<u32> {
        if !self.enabled || self.last_mark_request.is_some_and(|t| now.duration_since(t) < MARK_INTERVAL) {
            return None;
        }
        self.last_mark_request = Some(now);
        // An empty slot, otherwise any but the newest confirmed picture (the
        // one a recovery would use).
        let keep = self.newest_confirmed();
        let slot = (0..SLOTS)
            .find(|i| self.slots[*i].is_none())
            .or_else(|| (0..SLOTS).find(|i| Some(*i) != keep && Some(*i) != self.newest.filter(|_| keep.is_none())))
            .unwrap_or(0);
        Some(slot as u32)
    }

    fn newest_confirmed(&self) -> Option<usize> {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(i, s)| s.filter(|s| s.confirmed).map(|s| (i, s.rtp)))
            .max_by_key(|(_, rtp)| *rtp)
            .map(|(i, _)| i)
    }

    /// The pipeline sent a frame that uses the references.
    pub fn on_encoded(&mut self, rtp: u64, reference: FrameReference) {
        match reference {
            FrameReference::Keyframe => {
                // A keyframe empties the decoder's reference buffer.
                self.slots = [None; SLOTS];
                self.newest = None;
                self.recovery = None;
            }
            FrameReference::Marked { slot, keyframe } => {
                if keyframe {
                    self.slots = [None; SLOTS];
                    self.recovery = None;
                }
                let i = slot as usize % SLOTS;
                self.slots[i] = Some(Slot { rtp, confirmed: false });
                self.newest = Some(i);
            }
            FrameReference::Recovery { slot } => {
                // The other references may have been dropped from the
                // decoder's buffer by the recovery frame.
                for (i, s) in self.slots.iter_mut().enumerate() {
                    if i != slot as usize % SLOTS {
                        *s = None;
                    }
                }
                if let Some((t, None, attempts)) = self.recovery {
                    self.recovery = Some((t, Some(rtp), attempts));
                }
            }
        }
    }

    /// The tablet decoded the frames `from..=to` with intact references.
    pub fn on_intact(&mut self, from: u64, to: u64) {
        for s in self.slots.iter_mut().flatten() {
            if (from..=to).contains(&s.rtp) {
                s.confirmed = true;
            }
        }
        if let Some((_, Some(rtp), _)) = self.recovery
            && (from..=to).contains(&rtp)
        {
            self.recovery = None; // it worked
        }
    }

    /// The tablet lost frames. `None`: already being handled.
    pub fn on_loss(&mut self, now: Instant) -> Option<Action> {
        if self.last_keyframe_request.is_some_and(|t| now.duration_since(t) < SETTLE) {
            return None;
        }
        let mut attempts = 0;
        if let Some((t, sent, n)) = self.recovery {
            let age = now.duration_since(t);
            if age < SETTLE {
                return None;
            }
            let in_flight = sent.is_some_and(|rtp| self.arrived.is_none_or(|a| a < rtp));
            if in_flight && age < RECOVERY_IN_FLIGHT_MAX {
                return None; // the recovery frame hasn't arrived yet
            }
            // Still unconfirmed (a confirmation clears it) and the tablet
            // still lost: the recovery frame was lost (another hiccup) or
            // didn't decode. Try once more, then a keyframe is certain.
            if n + 1 >= RECOVERY_ATTEMPTS {
                return Some(self.keyframe(now));
            }
            attempts = n + 1;
        }
        match self.newest_confirmed() {
            Some(slot) if self.enabled => {
                self.recovery = Some((now, None, attempts));
                Some(Action::Recover(slot as u32))
            }
            _ => Some(self.keyframe(now)),
        }
    }

    fn keyframe(&mut self, now: Instant) -> Action {
        self.last_keyframe_request = Some(now);
        self.recovery = None;
        Action::Keyframe
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: fn(u64) -> Duration = Duration::from_millis;

    #[test]
    fn confirmed_reference_recovers_without_keyframe() {
        let t0 = Instant::now();
        let mut r = References::new(true);
        // Start: keyframe kept as slot 0, confirmed by the tablet.
        r.on_encoded(0, FrameReference::Marked { slot: 0, keyframe: true });
        assert_eq!(r.on_loss(t0), Some(Action::Keyframe), "nothing confirmed yet");
        r.on_encoded(100, FrameReference::Keyframe);
        r.on_encoded(100, FrameReference::Marked { slot: 0, keyframe: true });
        r.on_intact(100, 4000);
        let t1 = t0 + Duration::from_secs(1);
        assert_eq!(r.due_mark(t1), Some(1));
        assert_eq!(r.due_mark(t1 + MS(100)), None, "one mark per interval");
        r.on_encoded(4500, FrameReference::Marked { slot: 1, keyframe: false });
        r.on_intact(100, 5000);

        // A loss: recover from the newest confirmed picture.
        assert_eq!(r.on_loss(t1 + MS(200)), Some(Action::Recover(1)));
        // The same loss reported again: ignored.
        assert_eq!(r.on_loss(t1 + MS(250)), None);
        r.on_encoded(6000, FrameReference::Recovery { slot: 1 });
        r.on_arrived(6100);
        r.on_intact(6000, 6100);
        // Slot 0 may be gone from the decoder: only 1 is usable.
        assert_eq!(r.on_loss(t1 + Duration::from_secs(2)), Some(Action::Recover(1)));
    }

    #[test]
    fn unconfirmed_recovery_escalates_to_keyframe() {
        let t0 = Instant::now();
        let mut r = References::new(true);
        r.on_encoded(0, FrameReference::Marked { slot: 0, keyframe: true });
        r.on_intact(0, 1000);
        assert_eq!(r.on_loss(t0), Some(Action::Recover(0)));
        r.on_encoded(2000, FrameReference::Recovery { slot: 0 });
        r.on_arrived(2000);
        // Still losing after the settle time, recovery not confirmed: once
        // more, then a keyframe.
        assert_eq!(r.on_loss(t0 + MS(300)), Some(Action::Recover(0)));
        r.on_encoded(3000, FrameReference::Recovery { slot: 0 });
        r.on_arrived(3000);
        assert_eq!(r.on_loss(t0 + MS(600)), Some(Action::Keyframe));
        assert_eq!(r.on_loss(t0 + MS(650)), None, "keyframe already requested");
    }

    #[test]
    fn a_recovery_frame_on_its_way_is_waited_for() {
        let t0 = Instant::now();
        let mut r = References::new(true);
        r.on_encoded(0, FrameReference::Marked { slot: 0, keyframe: true });
        r.on_intact(0, 1000);
        r.on_arrived(1000);
        assert_eq!(r.on_loss(t0), Some(Action::Recover(0)));
        r.on_encoded(2000, FrameReference::Recovery { slot: 0 });
        // Queued behind retransmissions: not there yet after 400 ms.
        assert_eq!(r.on_loss(t0 + MS(400)), None);
        r.on_arrived(1900);
        assert_eq!(r.on_loss(t0 + MS(800)), None);
        // Arrived, but the tablet still reports loss: the recovery failed.
        r.on_arrived(2000);
        assert_eq!(r.on_loss(t0 + MS(900)), Some(Action::Recover(0)), "second attempt");
    }

    #[test]
    fn marks_never_overwrite_the_newest_reference() {
        let t0 = Instant::now();
        let mut r = References::new(true);
        r.on_encoded(0, FrameReference::Marked { slot: 0, keyframe: true });
        let s = r.due_mark(t0).unwrap();
        assert_eq!(s, 1);
        r.on_encoded(10, FrameReference::Marked { slot: s, keyframe: false });
        assert_eq!(r.due_mark(t0 + MS(600)), Some(0));
    }

    #[test]
    fn the_reference_used_for_recovery_is_kept() {
        let t0 = Instant::now();
        let mut r = References::new(true);
        r.on_encoded(0, FrameReference::Marked { slot: 0, keyframe: true });
        r.on_intact(0, 10);
        r.due_mark(t0);
        r.on_encoded(20, FrameReference::Marked { slot: 1, keyframe: false });
        r.on_intact(0, 30);
        assert_eq!(r.on_loss(t0 + MS(100)), Some(Action::Recover(1)));
        r.on_encoded(40, FrameReference::Recovery { slot: 1 });
        r.on_intact(40, 50);
        // Slot 0 was dropped with the recovery: marks go there, slot 1 stays.
        assert_eq!(r.due_mark(t0 + MS(600)), Some(0));
        r.on_encoded(60, FrameReference::Marked { slot: 0, keyframe: false });
        assert_eq!(r.due_mark(t0 + MS(1200)), Some(0), "slot 0 not confirmed yet: slot 1 is the only usable one");
        r.on_intact(40, 70);
        assert_eq!(r.due_mark(t0 + MS(1800)), Some(1));
    }

    #[test]
    fn disabled_always_uses_keyframes() {
        let mut r = References::new(false);
        r.on_encoded(0, FrameReference::Marked { slot: 0, keyframe: true });
        r.on_intact(0, 100);
        assert_eq!(r.due_mark(Instant::now()), None);
        assert_eq!(r.on_loss(Instant::now()), Some(Action::Keyframe));
    }
}
