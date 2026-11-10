//! SPCX shock bot: fades departures from a quiet-level anchor.
//!
//! Most shocks and all fizzles revert, so the bot sells squeezes and buys
//! crashes, sized by how far the mark sits from the level it last held.
//! Small excursions that don't revert quickly are abandoned (small shocks
//! aren't worth fading), and excursions that hold past the longest pause
//! are accepted as the new level. Size is capped by the drawdown room so
//! one more worst-case leg can't breach the mandate.

use mtx_kit::prelude::*;
use tracing::info;

fn envf(name: &str, default: f64) -> f64 {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

struct Params {
    d0: f64,          // deviation where a fade starts
    d1: f64,          // deviation where the fade is full size
    d_exit: f64,      // deviation below which the excursion is over
    small: f64,       // excursions below this are treated as fizzles
    small_timeout: f64, // seconds before a small excursion is abandoned
    stall_timeout: f64, // seconds without a new extreme before re-anchoring
    new_leg: f64,     // growth of the extreme that counts as a new leg
    ramp_guard: f64,  // step move above which the market counts as moving
    calm_steps: u64,  // steps without such a move before adding size
    ride: f64,        // 1 to ride a move that keeps going (2 moving steps in 3), 0 to not
    ride1: f64,       // a single step move this big also starts a ride (0 = never)
    anchor_tau: f64,  // seconds, anchor tracking in quiet
    dd_margin: f64,   // USD kept back from the drawdown room
    worst_leg: f64,   // fraction, worst adverse leg assumed for sizing
    min_trade: f64,
}

impl Params {
    fn load() -> Self {
        Params {
            d0: envf("B_D0", 0.0045),
            d1: envf("B_D1", 0.015),
            d_exit: envf("B_DEXIT", 0.003),
            small: envf("B_SMALL", 0.018),
            small_timeout: envf("B_SMALL_TO", 13.0),
            stall_timeout: envf("B_STALL_TO", 40.0),
            new_leg: envf("B_NEWLEG", 0.0025),
            ramp_guard: envf("B_RAMP", 0.003),
            calm_steps: envf("B_CALM", 3.0) as u64,
            ride: envf("B_RIDE", 1.0),
            ride1: envf("B_RIDE1", 0.010),
            anchor_tau: envf("B_TAU", 8.0),
            dd_margin: envf("B_DDM", 130.0),
            worst_leg: envf("B_WORST", 0.09),
            min_trade: envf("B_MINTRADE", 5.0),
        }
    }
}

#[derive(Default)]
struct Exc {
    dir: f64,       // +1 if the mark went up (we're short), -1 if down
    extreme: f64,   // largest |dev| seen in this excursion
    extreme_t: f64, // market time of the last new extreme
    parked: bool,   // small excursion abandoned: flat until a new leg
}

struct Bot {
    p: Params,
    start_slot: Option<u64>,
    anchor: f64,
    prev_mark: f64,
    exc: Option<Exc>,
    fees: f64,
    last_pos: f64,
    last_move_k: u64,
    moves: Vec<(u64, f64)>, // (step, signed move) of recent moving steps
}

impl Bot {
    fn new() -> Self {
        Bot {
            p: Params::load(),
            start_slot: None,
            anchor: 0.0,
            prev_mark: 0.0,
            exc: None,
            fees: 0.0,
            last_pos: 0.0,
            last_move_k: 0,
            moves: Vec::new(),
        }
    }
}

impl Strategy for Bot {
    async fn start(&mut self, session: &Session) -> BotResult {
        info!(symbol = session.env.symbol, steps = session.hello.steps, cap = session.env.max_position, "started");
        Ok(())
    }

    async fn step(&mut self, m: &Market, orders: &mut Orders<'_>) -> BotResult {
        let p = &self.p;
        let start = *self.start_slot.get_or_insert(m.slot);
        let t = (m.slot - start) as f64 * 0.25;
        if self.anchor == 0.0 {
            self.anchor = m.mark;
            self.prev_mark = m.mark;
        }
        // Fee estimate from position changes since the last step.
        let traded = (m.position.size - self.last_pos).abs();
        self.fees += traded * m.mark * 0.00035;
        self.last_pos = m.position.size;

        let dev = (m.mark - self.anchor) / self.anchor;
        let last_move = ((m.mark - self.prev_mark) / self.prev_mark).abs();
        let signed_move = (m.mark - self.prev_mark) / self.prev_mark;
        if last_move > p.ramp_guard {
            self.last_move_k = m.k;
            self.moves.push((m.k, signed_move));
        }
        self.moves.retain(|(k, _)| m.k - k < p.calm_steps);
        self.prev_mark = m.mark;
        let moving = m.k - self.last_move_k < p.calm_steps;
        // A move that keeps going: two moving steps the same way within the
        // calm window, none against. Ride it until it stops.
        let ride_dir = if p.ride > 0.0 && self.moves.len() >= 2
            && self.moves.iter().all(|(_, mv)| mv.signum() == self.moves[0].1.signum())
        {
            self.moves[0].1.signum()
        } else if p.ride1 > 0.0 && last_move >= p.ride1 {
            signed_move.signum()
        } else {
            0.0
        };

        let mut target: f64 = 0.0;
        let mut state = "quiet";
        match self.exc.as_mut() {
            None => {
                if dev.abs() >= p.d0 {
                    self.exc = Some(Exc { dir: dev.signum(), extreme: dev.abs(), extreme_t: t, parked: false });
                    state = "enter";
                } else {
                    let alpha = 0.5 / p.anchor_tau;
                    self.anchor += alpha * (m.mark - self.anchor);
                }
            }
            Some(e) => {
                if dev * e.dir < 0.0 || dev.abs() < p.d_exit {
                    // Reverted to the anchor: excursion over, keep the anchor.
                    self.exc = None;
                    state = "reverted";
                } else {
                    if dev.abs() > e.extreme + p.new_leg {
                        e.extreme = dev.abs();
                        e.extreme_t = t;
                        e.parked = false;
                    }
                    let since = t - e.extreme_t;
                    if since > p.stall_timeout {
                        self.anchor = m.mark;
                        self.exc = None;
                        state = "stalled";
                    } else if e.extreme < p.small && since > p.small_timeout {
                        e.parked = true;
                        state = "parked";
                    } else if e.parked {
                        state = "parked";
                    } else {
                        state = "fade";
                        let f = ((dev.abs() - p.d0) / (p.d1 - p.d0)).clamp(0.0, 1.0);
                        target = -e.dir * m.cap * f;
                    }
                }
            }
        }

        // Drawdown-aware size cap: one more worst-case leg must fit in the room.
        let room = (m.max_drawdown - m.drawdown - self.fees - p.dd_margin).max(0.0);
        let max_abs = (room / (p.worst_leg * m.mark)).min(m.cap);
        target = target.clamp(-max_abs, max_abs);

        // Don't add size into a move that may still be running; don't open
        // new risk in the last couple of seconds.
        let pos = m.position.size;
        if ride_dir != 0.0 && m.steps_left >= 4 && state != "reverted" {
            state = "ride";
            target = ride_dir * max_abs;
        } else if moving && state != "reverted" && state != "stalled" {
            // Still moving: hold what we have rather than trade into it.
            target = pos;
        } else {
            let adding = target.abs() > pos.abs() + 0.5 && target * pos >= 0.0;
            if adding && m.steps_left < 4 {
                target = pos;
            }
        }

        let diff = (target - pos).abs();
        let min_trade = if target == 0.0 { 1.0 } else { p.min_trade };
        if diff >= min_trade {
            orders.target_position_with(target, Execution { min_trade: 1.0, ..Execution::default() });
        }
        info!(k = m.k, t, mark = m.mark, anchor = self.anchor, dev = dev * 100.0, state, pos, target, pnl = m.pnl, dd = m.drawdown, fees = self.fees, "step");
        Ok(())
    }
}

fn main() -> BotResult {
    mtx_kit::run(Bot::new())
}
