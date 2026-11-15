//! A market-making session's PnL, taken apart the way a desk reads a maker.
//!
//! A scenario that opts in (`close_out` in its making block, see
//! [`crate::making::Making`]) reports, beside its PnL, what the PnL is made
//! of, from the bot's fills ([`MakingFill`]: role, counterparty, fee) and
//! the scenario's level, the oracle the account is valued at. With `L(t)`
//! the level `t` slots in, and each fill's size `q`, price `p` and sign `s`
//! (+1 a buy, -1 a sell):
//!
//! ```text
//! spread capture  S    = sum over passive fills  q s (L(t) - p)
//! markout at h    M_h  = sum over passive fills  q s (L(t + h) - L(t))   (<= 0: adversely selected)
//! realised spread RS_h = S + M_h
//! taking cost     T    = sum over taking fills   q s (L(t) - p) - taker fees   (the close-out included)
//! inventory       V    = X - S - T + maker fees                          (position times the oracle's moves)
//! ```
//!
//! `X` is the session's PnL, after the close-out. Horizons are 2, 20 and 120
//! slots and the session's end; a horizon past the end is cut at it. `V` is
//! what the fills don't explain, so the parts add up to `X` by construction.
//! The check that `V` is what it says counts the same inventory from the
//! other side: the position held across each move of the level
//! (`inventory_jump_usd` and `inventory_walk_usd`). `unexplained_usd` is
//! what is left between the two: whatever the fills' logged prices and fees
//! and the level don't account for in the account's value. A
//! move is a jump if it sits [`JUMP_SIGMAS`] robust standard deviations from
//! the session's typical move; the rest is the walk.
//!
//! Reported, not scored.

use mtx_core::Side;
use serde::{Deserialize, Serialize};

use crate::{making::MakingFill, scenario::Waypoint};

/// The markout horizons, in slots; `None` is the session's end.
pub const HORIZONS: [Option<u64>; 4] = [Some(2), Some(20), Some(120), None];

/// How many robust standard deviations (1.4826 times the median absolute
/// deviation) from the median move a move of the level must sit to count as
/// a jump.
pub const JUMP_SIGMAS: f64 = 4.0;

/// The smallest spread of moves the jump test trusts, in percent of the
/// mark: the level moves in steps of 0.001%, so a path whose moves barely
/// vary would otherwise make every move a jump.
pub const MIN_MOVE_SCALE_PCT: f64 = 0.005;

/// How a market-making session's PnL splits.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Decomposition {
    /// The session's PnL, after the close-out: what the parts add up to.
    pub pnl_usd: f64,
    /// `S`: what the bot's resting orders earned against the level.
    pub spread_capture_usd: f64,
    /// How many passive fills there were, and their size and notional.
    pub passive_fills: u64,
    pub passive_base: f64,
    pub passive_notional_usd: f64,
    /// `M_h` and `RS_h` at each horizon.
    pub markouts: Vec<Markout>,
    /// `T`: what the bot's taking cost against the level, with taker fees.
    pub taking_cost_usd: f64,
    /// The fees the bot paid as a maker (a rebate is negative).
    pub maker_fees_usd: f64,
    /// `V`: `pnl_usd - S - T + maker_fees_usd`.
    pub inventory_usd: f64,
    /// `V` counted from the position held across the level's moves, split
    /// into the jumps and the walk.
    pub inventory_jump_usd: f64,
    pub inventory_walk_usd: f64,
    /// `inventory_usd` less those two: what the fills' logged prices and
    /// fees and the level don't account for.
    pub unexplained_usd: f64,
    /// The share of steps the bot quoted both sides within its obligations'
    /// band, when the scenario has obligations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uptime: Option<f64>,
    /// The close-out after the bot's last turn.
    pub close_out: CloseOutCost,
}

/// One horizon's markout.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Markout {
    /// Slots after each fill; absent for the session's end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub horizon_slots: Option<u64>,
    pub markout_usd: f64,
    /// `S + markout_usd`.
    pub realized_spread_usd: f64,
}

/// What the forced close after the bot's last turn cost.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CloseOutCost {
    /// Its fills, and the base units they closed.
    pub fills: u64,
    pub base: f64,
    /// What it paid against the level beyond the fees (the book's impact),
    /// the taker fees, and both together.
    pub slippage_usd: f64,
    pub fees_usd: f64,
    pub cost_usd: f64,
    /// The position the close left, in base units (valued at the level).
    pub residual_base: f64,
}

/// What the decomposition needs of a session.
pub struct Session<'a> {
    pub fills: &'a [MakingFill],
    pub start_position: f64,
    pub end_position: f64,
    pub pnl_usd: f64,
    /// The scenario's level path, and the mark it is a percentage of.
    pub path: &'a [Waypoint],
    pub start_mark_usd: f64,
    /// The session's length in slots.
    pub end_at: u64,
    pub uptime: Option<f64>,
    /// The slot the close-out was sent at.
    pub close_out_at: u64,
}

fn sign(side: Side) -> f64 {
    match side {
        Side::Buy => 1.0,
        Side::Sell => -1.0,
    }
}

fn median(values: &mut [f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(f64::total_cmp);
    let mid = values.len() / 2;
    if values.len() % 2 == 1 {
        values[mid]
    } else {
        (values[mid - 1] + values[mid]) / 2.0
    }
}

/// Split a session's PnL.
pub fn decompose(session: &Session) -> Decomposition {
    let level = |at: u64| {
        session.start_mark_usd * (1.0 + crate::making::level_at(session.path, at) / 100.0)
    };
    let end_at = session.end_at;
    let mut spread = 0.0;
    let mut passive = (0u64, 0.0, 0.0);
    let mut markouts = [0.0; HORIZONS.len()];
    let mut taking_gross = 0.0;
    let (mut taker_fees, mut maker_fees) = (0.0, 0.0);
    let mut close_out = CloseOutCost {
        fills: 0,
        base: 0.0,
        slippage_usd: 0.0,
        fees_usd: 0.0,
        cost_usd: 0.0,
        residual_base: session.end_position,
    };
    for fill in session.fills {
        if fill.role == "maker" {
            maker_fees += fill.fee_usd;
        } else {
            taker_fees += fill.fee_usd;
        }
        let forced_close = fill.forced && fill.at >= session.close_out_at;
        if forced_close {
            close_out.fees_usd += fill.fee_usd;
        }
        // A match between the bot's own accounts moves nothing: its two
        // records cancel, and only their fees are left.
        if fill.self_match {
            continue;
        }
        let at = fill.at.min(end_at);
        let (signed, here) = (fill.base * sign(fill.side), level(at));
        let edge = signed * (here - fill.price);
        if fill.role == "maker" {
            spread += edge;
            passive.0 += 1;
            passive.1 += fill.base;
            passive.2 += fill.base * fill.price;
            for (sum, horizon) in markouts.iter_mut().zip(HORIZONS) {
                let later = horizon.map_or(end_at, |h| (at + h).min(end_at));
                *sum += signed * (level(later) - here);
            }
        } else {
            taking_gross += edge;
            if forced_close {
                close_out.fills += 1;
                close_out.base += fill.base;
                close_out.slippage_usd -= edge;
            }
        }
    }
    close_out.cost_usd = close_out.slippage_usd + close_out.fees_usd;
    let taking = taking_gross - taker_fees;
    let inventory = session.pnl_usd - spread - taking + maker_fees;
    let (jump, walk) = held_through_moves(session);
    Decomposition {
        pnl_usd: session.pnl_usd,
        spread_capture_usd: spread,
        passive_fills: passive.0,
        passive_base: passive.1,
        passive_notional_usd: passive.2,
        markouts: markouts
            .into_iter()
            .zip(HORIZONS)
            .map(|(markout, horizon_slots)| Markout {
                horizon_slots,
                markout_usd: markout,
                realized_spread_usd: spread + markout,
            })
            .collect(),
        taking_cost_usd: taking,
        maker_fees_usd: maker_fees,
        inventory_usd: inventory,
        inventory_jump_usd: jump,
        inventory_walk_usd: walk,
        unexplained_usd: inventory - jump - walk,
        uptime: session.uptime,
        close_out,
    }
}

/// The position the bot held just before each move of the level, times the
/// move, summed over the jumps and over the rest (the walk).
fn held_through_moves(session: &Session) -> (f64, f64) {
    let moves: Vec<&Waypoint> = session
        .path
        .iter()
        .filter(|w| w.slot <= session.end_at)
        .collect();
    let mut previous = 0.0;
    let changes: Vec<f64> = moves
        .iter()
        .map(|w| {
            let change = w.level_pct - previous;
            previous = w.level_pct;
            change
        })
        .collect();
    let center = median(&mut changes.clone());
    let spread = median(
        &mut changes
            .iter()
            .map(|c| (c - center).abs())
            .collect::<Vec<_>>(),
    );
    let scale = (1.4826 * spread).max(MIN_MOVE_SCALE_PCT);
    let mut ordered: Vec<&MakingFill> = session.fills.iter().collect();
    ordered.sort_by_key(|f| f.at.min(session.end_at));
    let mut upcoming = ordered.into_iter().peekable();
    let mut position = session.start_position;
    let (mut jump, mut walk) = (0.0, 0.0);
    for (waypoint, change) in moves.iter().zip(&changes) {
        while let Some(fill) = upcoming.next_if(|f| f.at.min(session.end_at) < waypoint.slot) {
            position += fill.base * sign(fill.side);
        }
        let moved = position * session.start_mark_usd * change / 100.0;
        if (change - center).abs() > JUMP_SIGMAS * scale {
            jump += moved;
        } else {
            walk += moved;
        }
    }
    (jump, walk)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fill(
        at: u64,
        side: Side,
        role: &'static str,
        price: f64,
        base: f64,
        fee: f64,
    ) -> MakingFill {
        MakingFill {
            at,
            side,
            role,
            counterparty: "taker",
            price,
            base,
            fee_usd: fee,
            forced: false,
            self_match: false,
        }
    }

    fn path() -> Vec<Waypoint> {
        // A walk of tiny moves, with one jump of 2% at slot 60.
        [
            (0, 0.01),
            (10, 0.02),
            (20, 0.015),
            (30, 0.01),
            (40, 0.012),
            (50, 0.02),
            (60, 2.02),
            (70, 2.015),
            (80, 2.01),
            (90, 2.02),
        ]
        .into_iter()
        .map(|(slot, pct)| Waypoint::at(slot, pct))
        .collect()
    }

    fn level(path: &[Waypoint], at: u64) -> f64 {
        100.0 * (1.0 + crate::making::level_at(path, at) / 100.0)
    }

    /// The PnL a set of fills leaves from `start_position`, valued at the
    /// level at the session's end, slot 100, from a start mark of 100.
    fn pnl(fills: &[MakingFill], path: &[Waypoint], start_position: f64) -> f64 {
        fills
            .iter()
            .map(|f| sign(f.side) * f.base * (level(path, 100) - f.price) - f.fee_usd)
            .sum::<f64>()
            + start_position * (level(path, 100) - 100.0)
    }

    fn session<'a>(fills: &'a [MakingFill], path: &'a [Waypoint], pnl_usd: f64) -> Session<'a> {
        Session {
            fills,
            start_position: 0.0,
            end_position: fills.iter().map(|f| sign(f.side) * f.base).sum(),
            pnl_usd,
            path,
            start_mark_usd: 100.0,
            end_at: 100,
            uptime: Some(0.9),
            close_out_at: 100,
        }
    }

    #[test]
    fn spread_and_markouts_are_signed_by_the_side() {
        let path = path();
        let fills = [
            fill(5, Side::Buy, "maker", 99.9, 2.0, 0.0),
            fill(55, Side::Sell, "maker", 100.6, 1.0, 0.0),
        ];
        let d = decompose(&session(&fills, &path, 0.0));
        let expect_s = 2.0 * (level(&path, 5) - 99.9) - (level(&path, 55) - 100.6);
        assert!((d.spread_capture_usd - expect_s).abs() < 1e-9);
        assert_eq!(d.passive_fills, 2);
        assert!((d.passive_base - 3.0).abs() < 1e-12);
        assert!((d.passive_notional_usd - (2.0 * 99.9 + 100.6)).abs() < 1e-9);
        let labels: Vec<_> = d.markouts.iter().map(|m| m.horizon_slots).collect();
        assert_eq!(labels, [Some(2), Some(20), Some(120), None]);
        // The sale at 55 is run over by the jump at 60: its 20-slot markout
        // is negative; the buy at 5 sees only the walk.
        let sale = -(level(&path, 75) - level(&path, 55));
        let buy = 2.0 * (level(&path, 25) - level(&path, 5));
        assert!((d.markouts[1].markout_usd - (sale + buy)).abs() < 1e-9);
        assert!(sale < -1.0);
        // A horizon past the end is cut at it.
        assert!((d.markouts[2].markout_usd - d.markouts[3].markout_usd).abs() < 1e-9);
        for m in &d.markouts {
            assert!((m.realized_spread_usd - (d.spread_capture_usd + m.markout_usd)).abs() < 1e-12);
        }
    }

    #[test]
    fn the_parts_reconcile_to_the_pnl() {
        let path = path();
        let fills = [
            fill(5, Side::Buy, "maker", 99.9, 2.0, 0.01),
            fill(12, Side::Sell, "maker", 100.4, 1.0, 0.01),
            fill(33, Side::Buy, "taker", 100.2, 0.5, 0.05),
            fill(55, Side::Sell, "maker", 100.6, 3.0, -0.02),
            fill(100, Side::Buy, "taker", 102.2, 0.5, 0.05),
        ];
        for start_position in [0.0, 1.5] {
            let pnl_usd = pnl(&fills, &path, start_position);
            let mut s = session(&fills, &path, pnl_usd);
            s.start_position = start_position;
            let d = decompose(&s);
            let total =
                d.spread_capture_usd + d.taking_cost_usd - d.maker_fees_usd + d.inventory_usd;
            assert!((total - pnl_usd).abs() < 1e-9, "{total} vs {pnl_usd}");
            assert!(d.unexplained_usd.abs() < 1e-9, "{}", d.unexplained_usd);
            // Net of the fills before the jump at slot 60, the bot held
            // this much through it.
            let held = start_position + 2.0 - 1.0 + 0.5 - 3.0;
            assert!((d.inventory_jump_usd - held * 100.0 * 2.0 / 100.0).abs() < 1e-9);
        }
    }

    #[test]
    fn a_forced_close_out_is_a_taking_cost_and_its_own_line() {
        let path = path();
        let mut close = fill(100, Side::Sell, "taker", 101.9, 2.0, 0.07);
        close.forced = true;
        let fills = [fill(5, Side::Buy, "maker", 99.9, 2.0, 0.0), close];
        let pnl_usd = pnl(&fills, &path, 0.0);
        let mut s = session(&fills, &path, pnl_usd);
        s.end_position = 0.0;
        let d = decompose(&s);
        let slippage = 2.0 * (level(&path, 100) - 101.9);
        assert_eq!(d.close_out.fills, 1);
        assert!((d.close_out.base - 2.0).abs() < 1e-12);
        assert!((d.close_out.slippage_usd - slippage).abs() < 1e-9);
        assert!((d.close_out.fees_usd - 0.07).abs() < 1e-12);
        assert!((d.close_out.cost_usd - slippage - 0.07).abs() < 1e-9);
        assert!((d.taking_cost_usd + slippage + 0.07).abs() < 1e-9);
        assert!(d.unexplained_usd.abs() < 1e-9);
    }

    #[test]
    fn a_self_match_moves_nothing_and_leaves_its_fees() {
        let path = path();
        let mut a = fill(20, Side::Buy, "taker", 100.0, 1.0, 0.04);
        let mut b = fill(20, Side::Sell, "maker", 100.0, 1.0, 0.0);
        a.self_match = true;
        b.self_match = true;
        let fills = [a, b];
        let d = decompose(&session(&fills, &path, -0.04));
        assert_eq!(d.passive_fills, 0);
        assert_eq!(d.spread_capture_usd, 0.0);
        assert!((d.taking_cost_usd + 0.04).abs() < 1e-12);
        assert!(d.inventory_usd.abs() < 1e-12 && d.unexplained_usd.abs() < 1e-12);
    }

    #[test]
    fn no_fills_is_all_zero() {
        let path = path();
        let d = decompose(&session(&[], &path, 0.0));
        assert_eq!(d.spread_capture_usd, 0.0);
        assert_eq!(d.close_out.fills, 0);
        assert!(d.markouts.iter().all(|m| m.markout_usd == 0.0));
        assert_eq!(d.uptime, Some(0.9));
    }

    #[test]
    fn it_round_trips_through_json() {
        let path = path();
        let fills = [fill(5, Side::Buy, "maker", 99.9, 2.0, 0.01)];
        let d = decompose(&session(&fills, &path, 0.3));
        let json = serde_json::to_string(&d).unwrap();
        assert_eq!(serde_json::from_str::<Decomposition>(&json).unwrap(), d);
    }
}
