//! Curator guardrails — SPEC-DERIVED-§7 (CURATOR_PAIR_PROTOCOL.md).
//!
//! Three of the nine anti-collusion guardrails live here as schedulers
//! (the rest are structural: P19 token exclusions, the disagreement quota
//! in the ledger, adjudicator prior-blindness, weight pinning, boundary
//! probes in the Warden audit path, and Trinity governance of curator
//! writes):
//!
//! - [`ProbeScheduler`]: at a seeded rate (default 0.1%; ×10 under
//!   suspicious agreement) the Router injects an **adversarial probe** — a
//!   synthetic Librarian output citing a fabricated handle. The Warden MUST
//!   flag it; a pass is a `curator.probe_missed` incident.
//! - [`BlindReauditScheduler`]: ~1% of passed audits are re-run blind
//!   (evidence order permuted); verdict mismatch trips
//!   `curator.blind_reaudit_mismatch`.
//! - [`CalibrationTracker`]: per-confidence-band audit outcomes. A High
//!   band that keeps failing audits (accuracy < 0.85 over its last 50)
//!   puts the Librarian in **degraded mode** — the Router then forces
//!   `semantic_check` on all its outputs until calibration recovers.

use super::ConfidenceBand;
use crate::core::ulid::DetRng;
use std::collections::VecDeque;

// ---------------------------------------------------------------------------
// ProbeScheduler
// ---------------------------------------------------------------------------

pub struct ProbeScheduler {
    rng: DetRng,
    pub base_rate: f64,
    counter: u64,
}

impl ProbeScheduler {
    pub fn new(seed: u64, base_rate: f64) -> Self {
        ProbeScheduler {
            rng: DetRng::new(seed ^ 0x9B0B),
            base_rate,
            counter: 0,
        }
    }

    /// Decide whether the next audit cycle carries a probe. `boost` comes
    /// from the ledger (`probe_boost()` is 10.0 under suspicious
    /// agreement). Deterministic given (seed, call sequence, boost history).
    pub fn should_probe(&mut self, boost: f64) -> bool {
        self.counter += 1;
        let threshold = (self.base_rate * boost).min(1.0);
        self.rng.next_f64() < threshold
    }

    /// A fabricated handle for the probe body — deliberately well-formed so
    /// only existence-checking (not pattern-matching) catches it.
    pub fn fabricated_handle(&mut self, logical_at: u64) -> String {
        let ulid = crate::core::ulid::Ulid::from_parts(logical_at, &mut self.rng);
        format!("fact/{ulid}")
    }

    pub fn probes_scheduled(&self) -> u64 {
        self.counter
    }
}

// ---------------------------------------------------------------------------
// BlindReauditScheduler
// ---------------------------------------------------------------------------

pub struct BlindReauditScheduler {
    rng: DetRng,
    pub rate: f64,
}

impl BlindReauditScheduler {
    pub fn new(seed: u64, rate: f64) -> Self {
        BlindReauditScheduler {
            rng: DetRng::new(seed ^ 0xB1AD),
            rate,
        }
    }

    pub fn should_reaudit(&mut self) -> bool {
        self.rng.next_f64() < self.rate
    }
}

// ---------------------------------------------------------------------------
// CalibrationTracker
// ---------------------------------------------------------------------------

pub const CALIBRATION_WINDOW: usize = 50;
pub const HIGH_BAND_FLOOR: f64 = 0.85;
pub const MEDIUM_BAND_FLOOR: f64 = 0.60;

pub struct CalibrationTracker {
    high: VecDeque<bool>,
    medium: VecDeque<bool>,
    low: VecDeque<bool>,
}

impl Default for CalibrationTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl CalibrationTracker {
    pub fn new() -> Self {
        CalibrationTracker {
            high: VecDeque::with_capacity(CALIBRATION_WINDOW),
            medium: VecDeque::with_capacity(CALIBRATION_WINDOW),
            low: VecDeque::with_capacity(CALIBRATION_WINDOW),
        }
    }

    pub fn record(&mut self, band: ConfidenceBand, audit_passed: bool) {
        let q = match band {
            ConfidenceBand::High => &mut self.high,
            ConfidenceBand::Medium => &mut self.medium,
            ConfidenceBand::Low => &mut self.low,
        };
        if q.len() == CALIBRATION_WINDOW {
            q.pop_front();
        }
        q.push_back(audit_passed);
    }

    fn accuracy(q: &VecDeque<bool>) -> Option<f64> {
        if q.len() < 10 {
            return None;
        }
        Some(q.iter().filter(|b| **b).count() as f64 / q.len() as f64)
    }

    pub fn high_accuracy(&self) -> Option<f64> {
        Self::accuracy(&self.high)
    }
    pub fn medium_accuracy(&self) -> Option<f64> {
        Self::accuracy(&self.medium)
    }

    /// Degraded mode: high-confidence outputs failing at a rate that
    /// invalidates the band's meaning (CURATOR_PAIR_PROTOCOL.md §7.5).
    pub fn degraded(&self) -> bool {
        matches!(self.high_accuracy(), Some(a) if a < HIGH_BAND_FLOOR)
            || matches!(self.medium_accuracy(), Some(a) if a < MEDIUM_BAND_FLOOR)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_rate_is_deterministic_and_boostable() {
        let run = |boost: f64| -> Vec<bool> {
            let mut p = ProbeScheduler::new(42, 0.01);
            (0..1000).map(|_| p.should_probe(boost)).collect()
        };
        let a = run(1.0);
        let b = run(1.0);
        assert_eq!(a, b); // deterministic
        let base_hits = a.iter().filter(|x| **x).count();
        let boosted_hits = run(10.0).iter().filter(|x| **x).count();
        assert!((1..=40).contains(&base_hits), "base_hits={base_hits}");
        assert!(
            boosted_hits > base_hits * 3,
            "boosted={boosted_hits} base={base_hits}"
        );
    }

    #[test]
    fn fabricated_handles_look_real() {
        let mut p = ProbeScheduler::new(7, 0.001);
        let h = p.fabricated_handle(500);
        assert!(h.starts_with("fact/"));
        assert_eq!(h.len(), "fact/".len() + 26); // ULID base32 length
                                                 // Distinct across calls.
        assert_ne!(h, p.fabricated_handle(500));
    }

    #[test]
    fn calibration_degrades_and_recovers() {
        let mut c = CalibrationTracker::new();
        // 20 high-band passes: healthy.
        for _ in 0..20 {
            c.record(ConfidenceBand::High, true);
        }
        assert!(!c.degraded());
        // A run of failures drags accuracy below the floor.
        for _ in 0..10 {
            c.record(ConfidenceBand::High, false);
        }
        assert!(c.degraded());
        // Sustained passes roll the failures out of the window.
        for _ in 0..CALIBRATION_WINDOW {
            c.record(ConfidenceBand::High, true);
        }
        assert!(!c.degraded());
    }

    #[test]
    fn blind_reaudit_rate() {
        let mut b = BlindReauditScheduler::new(1, 0.01);
        let hits = (0..10_000).filter(|_| b.should_reaudit()).count();
        assert!(hits > 40 && hits < 220, "hits={hits}"); // ~1% ± noise
    }
}
