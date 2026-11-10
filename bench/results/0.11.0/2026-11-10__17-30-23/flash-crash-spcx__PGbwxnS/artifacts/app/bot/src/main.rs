//! Shock trader for SPCX: rides each move while it runs, fades it once it
//! stops, and sizes every position so one more adverse step can't take the
//! account past the mandate's drawdown limit.

use mtx_kit::prelude::*;

/// A step move (in % of the first price) at least this big is an event, not wobble.
const EVENT_PCT: f64 = 0.22;
/// Steps without a new event after which a move is taken as finished.
const RIDE_STEPS: i64 = 4;
/// A sequence that gave back this share of its peak deviation is over.
const REVERT_SHARE: f64 = 0.25;
/// Steps to hold a fade with nothing happening before giving up.
const FADE_STEPS: i64 = 80;
/// Past this many steps in a fade the move wasn't a fizzle: a shock smaller
/// than `LATE_FADE_MIN_PCT` is then as likely to grow another leg as to
/// revert, and isn't worth fading until its third leg.
const FIZZLE_STEPS: i64 = 22;
const LATE_FADE_MIN_PCT: f64 = 2.0;
/// A deviation below this, in %, counts as back at the anchor.
const SMALL_PCT: f64 = 0.3;
/// Drawdown budget the sizing works with (the mandate allows 1000), and the
/// worst single-step move, in %, it assumes.
const DD_BUDGET_SHARE: f64 = 0.80;
const MAX_STEP_PCT: f64 = 4.5;
/// Estimated cost of trading, as a share of notional.
const FEE: f64 = 0.0008;

#[derive(PartialEq, Clone, Copy, Debug)]
enum Mode {
    Idle,
    Ride,
    Fade,
}

struct Shock {
    k: i64,
    first: Option<f64>,
    prev: f64,
    mode: Mode,
    anchor: f64,
    seq: f64,
    ride_dir: f64,
    last_event: i64,
    peak: f64,
    legs: u32,
    fade_start: i64,
    // own estimate of equity, net of trading costs
    fees: f64,
    peak_eq: f64,
    last_pos: Option<f64>,
}

impl Default for Shock {
    fn default() -> Self {
        Shock {
            k: -1,
            first: None,
            prev: 0.0,
            mode: Mode::Idle,
            anchor: 0.0,
            seq: 0.0,
            ride_dir: 0.0,
            last_event: -999,
            peak: 0.0,
            legs: 0,
            fade_start: 0,
            fees: 0.0,
            peak_eq: 0.0,
            last_pos: None,
        }
    }
}

impl Shock {
    /// The wanted position as a share of the cap, from this step's level (% from the first price).
    fn signal(&mut self, x: f64) -> f64 {
        self.k += 1;
        let k = self.k;
        if k == 0 {
            self.prev = x;
            self.anchor = x;
            return 0.0;
        }
        let r = x - self.prev;
        if r.abs() >= EVENT_PCT {
            let sg = r.signum();
            if self.mode == Mode::Idle {
                self.anchor = self.prev;
                self.seq = sg;
                self.peak = 0.0;
                self.legs = 0;
            }
            self.last_event = k;
            self.ride_dir = sg;
            self.mode = Mode::Ride;
        }
        self.prev = x;
        let mut target = 0.0;
        if self.mode == Mode::Ride {
            if k - self.last_event >= RIDE_STEPS {
                let dev = x - self.anchor;
                if dev * self.seq <= SMALL_PCT {
                    self.mode = Mode::Idle;
                    self.anchor = x;
                } else {
                    if self.ride_dir == self.seq {
                        self.legs += 1;
                    }
                    self.peak = self.peak.max(dev.abs());
                    if dev.abs() <= (1.0 - REVERT_SHARE) * self.peak {
                        self.mode = Mode::Idle;
                        self.anchor = x;
                    } else {
                        self.mode = Mode::Fade;
                        self.fade_start = k;
                    }
                }
            } else {
                target = self.ride_dir;
            }
        }
        if self.mode == Mode::Fade {
            let dev = (x - self.anchor).abs();
            target = -self.seq;
            if k - self.fade_start > FIZZLE_STEPS && self.legs < 3 && dev < LATE_FADE_MIN_PCT {
                target = 0.0;
            }
            if k - self.fade_start > FADE_STEPS {
                self.mode = Mode::Idle;
                self.anchor = x;
            }
        }
        target
    }
}

impl Strategy for Shock {
    async fn step(&mut self, market: &Market, orders: &mut Orders<'_>) -> BotResult {
        let price = if market.oracle.is_finite() && market.oracle > 0.0 {
            market.oracle
        } else {
            market.mark
        };
        if !(price.is_finite() && price > 0.0 && market.mark.is_finite() && market.mark > 0.0) {
            return Ok(());
        }
        let pos = market.position.size;
        if let Some(last) = self.last_pos {
            self.fees += (pos - last).abs() * market.mark * FEE;
        }
        self.last_pos = Some(pos);
        let eq = market.pnl - self.fees;
        self.peak_eq = self.peak_eq.max(eq);
        let dd = self.peak_eq - eq;

        let first = *self.first.get_or_insert(price);
        let x = (price / first - 1.0) * 100.0;
        let want = self.signal(x) * market.cap;

        let budget = market.max_drawdown * DD_BUDGET_SHARE;
        let room = ((budget - dd).max(0.0) / (market.mark * MAX_STEP_PCT / 100.0))
            .min(market.cap)
            .floor();
        let target = want.clamp(-room, room).round();
        eprintln!(
            "k={} mark={:.2} oracle={:.2} x={:.3} mode={:?} pos={} entry={:.3} target={} eq={:.1} dd={:.1}",
            market.k, market.mark, market.oracle, x, self.mode, pos, market.position.entry_usd, target, eq, dd
        );
        if market.last {
            return Ok(());
        }
        // A ride is urgent; a fade isn't, and crossing less of the book per
        // step fills closer to the oracle.
        let max_per_step = if self.mode == Mode::Ride {
            f64::INFINITY
        } else {
            market.cap
        };
        orders.target_position_with(
            target,
            Execution {
                max_per_step,
                ..Execution::default()
            },
        );
        Ok(())
    }
}

fn main() -> BotResult {
    mtx_kit::run(Shock::default())
}
