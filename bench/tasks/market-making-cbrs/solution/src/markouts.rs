//! What its own fills earned: each step's change of position is a fill (the
//! bot never takes), marked out `horizon_slots` later against the oracle.
//! A maker whose fills haven't been run over can show more size; one whose
//! fills are, keeps it small.

use std::collections::VecDeque;

/// The most fills waiting for their markout: a few a step at most.
const PENDING_MAX: usize = 1_024;

#[derive(Default)]
pub struct Markouts {
    horizon_slots: u64,
    /// (slot, signed size, oracle) of fills not yet marked out.
    pending: VecDeque<(u64, f64, f64)>,
    /// Marked-out fills: their count, and their size-weighted markout sum
    /// (bp times base units) and size.
    count: u32,
    markout_bps_size: f64,
    size: f64,
    last_position: Option<f64>,
}

impl Markouts {
    pub fn new(horizon_slots: u64) -> Self {
        assert!(horizon_slots > 0);
        Self {
            horizon_slots,
            ..Self::default()
        }
    }

    /// Record this step's fill, if the position moved, and mark out the
    /// fills `horizon_slots` old.
    pub fn step(&mut self, slot: u64, position: f64, oracle: f64) {
        assert!(oracle > 0.0);
        let filled = position - self.last_position.unwrap_or(position);
        self.last_position = Some(position);
        if filled.abs() > 1e-9 && self.pending.len() < PENDING_MAX {
            self.pending.push_back((slot, filled, oracle));
        }
        while let Some(&(at, size, then)) = self.pending.front() {
            if at + self.horizon_slots > slot {
                break;
            }
            self.pending.pop_front();
            let markout_bps = (oracle / then - 1.0) * 10_000.0 * size.signum();
            self.count += 1;
            self.markout_bps_size += markout_bps * size.abs();
            self.size += size.abs();
        }
    }

    /// Its fills' size-weighted markout, bp, once `at_least` are marked out.
    pub fn mean_bps(&self, at_least: u32) -> Option<f64> {
        (self.count >= at_least && self.size > 0.0).then(|| self.markout_bps_size / self.size)
    }
}
