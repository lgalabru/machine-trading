//! Off hours: a perp whose underlying trades on an external market that is
//! closed for part of the session (a US-listed share over a weekend).
//!
//! While the external market is closed there is no external price, and
//! Phoenix prices the index from its own book: it starts at the last
//! external price and follows the perp, smoothed, inside a band of 1 / max
//! leverage around that price. When the external window reopens, the index
//! moves to wherever the underlying opens. A scenario with [`OffHours`]
//! carries that index and the reopen next to its path, and the taker orders
//! the grader sends (see [`crate::flow`]); the session gives the bot the
//! reopen's slot as `MTX_REOPEN_SLOT`. What the closed period's flow is, and
//! how sessions are drawn, is a task family's business.

use serde::{Deserialize, Serialize};

use crate::{
    making::{TAKER_ACCOUNTS, TakerOrder, level_at},
    scenario::Waypoint,
};

/// What an off-hours scenario adds to its path: the closed period, the
/// index through it, and the taker flow.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct OffHours {
    /// Slot, relative to the session start, at which the external window
    /// reopens: the index and the makers move to the underlying's open from here.
    pub reopen_slot: u64,
    /// Phoenix's closed-period band around the last external price (the
    /// session's starting mark), in percent: 1 / max leverage.
    pub band_pct: f64,
    /// The half-spread the snapshot's makers quote at, in basis points.
    pub half_spread_bps: f64,
    /// Phoenix's index through the closed period, as levels relative to the
    /// starting mark. Before its first waypoint it's the starting mark; from
    /// the reopen, the path's own levels.
    pub index: Vec<Waypoint>,
    /// The taker orders the grader sends, in slot order.
    pub takers: Vec<TakerOrder>,
}

/// The most the index may trail the perp, in percent: Phoenix refuses an
/// oracle report more than 5% from its last, and each move of the perp
/// passes through it.
pub const MAX_INDEX_GAP_PCT: f64 = 3.0;

impl OffHours {
    pub fn validate(&self, path: &[Waypoint], duration_slots: u64) -> Result<(), String> {
        if self.reopen_slot == 0 || self.reopen_slot + 4 > duration_slots {
            return Err(format!(
                "reopen_slot {} must fall inside the session ({duration_slots} slots)",
                self.reopen_slot
            ));
        }
        if !(self.band_pct > 0.0 && self.band_pct < 25.0) {
            return Err(format!("band_pct {} is out of range", self.band_pct));
        }
        if !(self.half_spread_bps > 0.0 && self.half_spread_bps < 200.0) {
            return Err(format!(
                "half_spread_bps {} is out of range",
                self.half_spread_bps
            ));
        }
        let mut last = 0;
        for (i, w) in self.index.iter().enumerate() {
            if w.slot < last || w.slot >= self.reopen_slot {
                return Err(format!(
                    "index waypoint {i} is out of order or past the reopen"
                ));
            }
            if w.level_pct.abs() > self.band_pct {
                return Err(format!("index waypoint {i} is outside the band"));
            }
            last = w.slot;
        }
        for (i, w) in path.iter().enumerate() {
            if w.slot >= self.reopen_slot {
                break;
            }
            if w.level_pct.abs() > self.band_pct {
                return Err(format!("waypoint {i} is outside the closed-period band"));
            }
            let gap = (w.level_pct - level_at(&self.index, w.slot)).abs();
            if gap > MAX_INDEX_GAP_PCT + 0.5 {
                return Err(format!(
                    "waypoint {i} is {gap:.2}% from the index; Phoenix wouldn't take it"
                ));
            }
        }
        let mut last = 0;
        for (i, order) in self.takers.iter().enumerate() {
            if order.slot < last || order.slot >= duration_slots {
                return Err(format!(
                    "taker order {i} is out of order or past the session"
                ));
            }
            if !(order.size.is_finite() && order.size > 0.0)
                || !(order.limit_bps.is_finite() && order.limit_bps > 0.0)
                || order.account >= TAKER_ACCOUNTS
            {
                return Err(format!("taker order {i} is malformed"));
            }
            last = order.slot;
        }
        Ok(())
    }

    /// The index level at `slot`: the closed period's own until the
    /// reopen, then the path's.
    pub fn index_at(&self, path: &[Waypoint], slot: u64) -> f64 {
        if slot >= self.reopen_slot {
            level_at(path, slot)
        } else {
            level_at(&self.index, slot)
        }
    }
}
