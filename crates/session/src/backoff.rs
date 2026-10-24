//! Reconnection backoff.

use std::time::Duration;

const INITIAL: Duration = Duration::from_millis(500);
const MAX: Duration = Duration::from_secs(8);

/// Exponential backoff: 0.5 s, 1 s, 2 s, 4 s, 8 s, 8 s, ...
/// Short initial delays matter here: most drops are brief Wi-Fi hiccups.
#[derive(Debug, Default, Clone)]
pub struct Backoff {
    attempt: u32,
}

impl Backoff {
    pub fn next_delay(&mut self) -> Duration {
        let d = INITIAL.saturating_mul(1u32 << self.attempt.min(16)).min(MAX);
        self.attempt = self.attempt.saturating_add(1);
        d
    }

    pub fn attempt(&self) -> u32 {
        self.attempt
    }

    pub fn reset(&mut self) {
        self.attempt = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grows_then_caps_then_resets() {
        let mut b = Backoff::default();
        let delays: Vec<u64> = (0..7).map(|_| b.next_delay().as_millis() as u64).collect();
        assert_eq!(delays, vec![500, 1000, 2000, 4000, 8000, 8000, 8000]);
        assert_eq!(b.attempt(), 7);
        b.reset();
        assert_eq!(b.next_delay(), INITIAL);
        for _ in 0..100 {
            b.next_delay();
        }
        assert_eq!(b.next_delay(), MAX);
    }
}
