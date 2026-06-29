//! Adaptive bitrate control for the WebRTC media channel.
//!
//! Data channels are SCTP (no RTP REMB/transport-cc), so the usable congestion
//! signal is the send buffer (`RTCDataChannel::buffered_amount`). A rising
//! buffer means we're encoding faster than the link drains → back off;
//! a consistently low buffer means there's headroom → climb back toward the
//! configured ceiling. Classic AIMD: multiplicative decrease, additive
//! increase. The decision is a pure function of (current target, sampled
//! buffer), so it's unit-tested without any timers or sockets.

/// Buffer above this (bytes) → we're congested, decrease.
pub const HIGH_WATER: usize = 256 * 1024;
/// Buffer below this (bytes) for `INCREASE_AFTER` samples → headroom, increase.
pub const LOW_WATER: usize = 64 * 1024;
const INCREASE_AFTER: u32 = 3;
const DECREASE_FACTOR: f64 = 0.7;
const INCREASE_STEP_KBPS: u32 = 500;
/// How often the control loop samples the buffer.
pub const SAMPLE_INTERVAL_MS: u64 = 300;

#[derive(Debug)]
pub struct BitrateController {
    target_kbps: u32,
    ceiling_kbps: u32,
    floor_kbps: u32,
    low_streak: u32,
}

impl BitrateController {
    /// Start at the ceiling (the configured `--bitrate-kbps`) and adapt down
    /// from there. `floor` bounds how far quality can drop.
    pub fn new(ceiling_kbps: u32, floor_kbps: u32) -> Self {
        BitrateController {
            target_kbps: ceiling_kbps,
            ceiling_kbps,
            floor_kbps: floor_kbps.min(ceiling_kbps),
            low_streak: 0,
        }
    }

    pub fn target_kbps(&self) -> u32 {
        self.target_kbps
    }

    /// Feed one buffered-amount sample; returns the new target bitrate (kbps).
    pub fn sample(&mut self, buffered_bytes: usize) -> u32 {
        if buffered_bytes > HIGH_WATER {
            // multiplicative decrease, bounded by the floor
            let next = (self.target_kbps as f64 * DECREASE_FACTOR).round() as u32;
            self.target_kbps = next.max(self.floor_kbps);
            self.low_streak = 0;
        } else if buffered_bytes < LOW_WATER {
            self.low_streak += 1;
            if self.low_streak >= INCREASE_AFTER {
                self.target_kbps = (self.target_kbps + INCREASE_STEP_KBPS).min(self.ceiling_kbps);
                self.low_streak = 0;
            }
        } else {
            // in the comfortable band — hold, but don't count toward an increase
            self.low_streak = 0;
        }
        self.target_kbps
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_at_ceiling() {
        let c = BitrateController::new(4000, 600);
        assert_eq!(c.target_kbps(), 4000);
    }

    #[test]
    fn congestion_decreases_multiplicatively_to_the_floor() {
        let mut c = BitrateController::new(4000, 600);
        let a = c.sample(HIGH_WATER + 1); // 4000*0.7 = 2800
        assert_eq!(a, 2800);
        let b = c.sample(HIGH_WATER + 1); // 1960
        assert_eq!(b, 1960);
        // keep congesting → clamps at floor, never below
        for _ in 0..20 {
            c.sample(HIGH_WATER + 1);
        }
        assert_eq!(c.target_kbps(), 600);
    }

    #[test]
    fn headroom_increases_additively_after_a_streak_to_the_ceiling() {
        let mut c = BitrateController::new(4000, 600);
        // drop first
        c.sample(HIGH_WATER + 1); // 2800
        assert_eq!(c.target_kbps(), 2800);
        // a low-buffer streak: no change until INCREASE_AFTER samples
        assert_eq!(c.sample(0), 2800);
        assert_eq!(c.sample(0), 2800);
        assert_eq!(c.sample(0), 3300); // +500 on the 3rd
        // climbs by steps, clamped at the ceiling
        for _ in 0..20 {
            c.sample(0);
        }
        assert_eq!(c.target_kbps(), 4000);
    }

    #[test]
    fn mid_band_holds_and_resets_the_streak() {
        let mut c = BitrateController::new(4000, 600);
        c.sample(HIGH_WATER + 1); // 2800
        c.sample(0); // streak 1
        c.sample(0); // streak 2
        c.sample(LOW_WATER + (HIGH_WATER - LOW_WATER) / 2); // mid band → reset streak, hold
        assert_eq!(c.target_kbps(), 2800);
        c.sample(0); // streak restarts at 1
        c.sample(0); // 2
        assert_eq!(c.target_kbps(), 2800); // not yet
        assert_eq!(c.sample(0), 3300); // 3 → increase
    }

    #[test]
    fn floor_never_exceeds_ceiling() {
        let c = BitrateController::new(500, 600); // floor clamped to ceiling
        assert_eq!(c.target_kbps(), 500);
    }
}
