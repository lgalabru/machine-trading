//! SPCX market maker: quotes one tick inside the book around the oracle,
//! reads its own fills as the takers' flow, and goes with a one-sided burst
//! (informed flow before a jump) until the jump lands or the burst fades.

use mtx_kit::prelude::*;
use mtx_kit::{Keypair, PhoenixDriver};
use rust_decimal::prelude::ToPrimitive;

const BP: f64 = 1e-4;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Mode {
    Neutral,
    /// Riding a burst: direction, step it gives up at.
    Ride { dir: f64, until: u64 },
}

struct Bot {
    driver: Option<PhoenixDriver<Keypair>>,
    tick: f64,
    comp: f64, // competitor half-spread, fraction
    prev_pos: f64,
    sent: f64, // position change our market orders aimed at last step
    flow: f64, // signed taker volume over the window, + = takers buying
    hist: std::collections::VecDeque<f64>,
    prev_oracle: f64,
    start_oracle: f64,
    mode: Mode,
    cooldown: u64,
    p: Params,
}

#[derive(Clone, Copy)]
struct Params {
    decay: f64,
    trigger: f64,
    ride_size: f64,
    ride_steps: u64,
    jump: f64,
    qsize: f64,
    min_half: f64,
    skew: f64,
    mm_max: f64,
}

impl Default for Bot {
    fn default() -> Self {
        let env = |k: &str, d: f64| std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d);
        Bot {
            driver: None,
            tick: 0.01,
            comp: 0.0,
            prev_pos: 0.0,
            sent: 0.0,
            flow: 0.0,
            hist: Default::default(),
            prev_oracle: 0.0,
            start_oracle: 0.0,
            mode: Mode::Neutral,
            cooldown: 0,
            p: Params {
                decay: env("P_DECAY", 8.0),
                trigger: env("P_TRIGGER", 1e9),
                ride_size: env("P_RIDE", 18.0),
                ride_steps: env("P_RIDE_STEPS", 12.0) as u64,
                jump: env("P_JUMP", 0.008),
                qsize: env("P_QSIZE", 1.0),
                min_half: env("P_MIN_HALF", 1.5) * BP,
                skew: env("P_SKEW", 0.4) * BP,
                mm_max: env("P_MM_MAX", 2.0),
            },
        }
    }
}

impl Bot {
    /// Best bid and ask of everyone but us (call after cancelling ours).
    async fn others(&self) -> Option<(Option<f64>, Option<f64>)> {
        let t = self.driver.as_ref()?.trader();
        let bbo = t.bbo().await.ok()?;
        let px = |ticks: u64| t.market().ticks_to_price(ticks).to_f64();
        Some((
            bbo.best_bid_ticks().and_then(px),
            bbo.best_ask_ticks().and_then(px),
        ))
    }
}

impl Strategy for Bot {
    async fn start(&mut self, session: &Session) -> BotResult {
        if let Ok(d) = session.trader.driver() {
            if let Some(t) = d.trader().market().ticks_to_price(1).to_f64() {
                self.tick = t;
            }
            self.driver = Some(d);
        }
        eprintln!("START tick={}", self.tick);
        Ok(())
    }

    async fn step(&mut self, m: &Market, orders: &mut Orders<'_>) -> BotResult {
        let o = m.oracle;
        let pos = m.position.size;
        if self.start_oracle == 0.0 {
            self.start_oracle = o;
            self.prev_oracle = o;
            self.prev_pos = pos;
        }
        let fills = pos - self.prev_pos - self.sent;
        self.sent = 0.0;
        self.prev_pos = pos;
        let ret = o / self.prev_oracle - 1.0;
        self.prev_oracle = o;
        if !matches!(self.mode, Mode::Ride { .. }) {
            self.hist.push_back((-fills).clamp(-self.p.qsize, self.p.qsize));
            while self.hist.len() > self.p.decay as usize {
                self.hist.pop_front();
            }
            self.flow = self.hist.iter().sum();
        }
        if self.cooldown > 0 {
            self.cooldown -= 1;
        }

        // Clear our quotes, then see where everyone else stands.
        if let Some(t) = orders.trader() {
            let _ = t.cancel_all();
        }
        let (ob, oa) = self.others().await.unwrap_or((None, None));
        if self.comp == 0.0 {
            if let (Some(b), Some(a)) = (ob, oa) {
                self.comp = ((a - b) / 2.0 / o).clamp(5.0 * BP, 40.0 * BP);
            } else {
                self.comp = 15.0 * BP;
            }
        }

        if ret.abs() > self.p.jump {
            // A jump landed: whatever we rode, it's over.
            self.mode = Mode::Neutral;
            self.flow = 0.0;
                self.hist.clear();
            self.cooldown = 4;
        }
        if let Mode::Ride { until, .. } = self.mode {
            if m.k >= until {
                self.mode = Mode::Neutral;
                self.flow = 0.0;
                self.hist.clear();
                self.cooldown = 4;
            }
        }
        if self.mode == Mode::Neutral
            && self.cooldown == 0
            && self.flow.abs() > self.p.trigger
            && m.steps_left > 6
        {
            self.mode = Mode::Ride {
                dir: self.flow.signum(),
                until: m.k + self.p.ride_steps,
            };
        }

        eprintln!(
            "STEP k={} o={:.4} pos={:.3} fills={:.3} flow={:.2} ret_bp={:.1} ob={:.4} oa={:.4} mode={:?} pnl={:.2} budget={:?}",
            m.k, o, pos, fills, self.flow, ret / BP, ob.unwrap_or(0.0), oa.unwrap_or(0.0), self.mode, m.pnl, m.budget_left
        );

        let slip = self.comp + 15.0 * BP;
        match self.mode {
            Mode::Ride { dir, .. } => {
                let want = dir * self.p.ride_size;
                let diff = want - pos;
                if diff.abs() >= 0.1 {
                    if diff > 0.0 {
                        orders.buy(diff, o * (1.0 + slip));
                    } else {
                        orders.sell(-diff, o * (1.0 - slip));
                    }
                    self.sent = diff.clamp(-m.cap - pos, m.cap - pos);
                }
            }
            Mode::Neutral => {
                // Flatten anything big left from a ride with a market order.
                if pos.abs() > self.p.mm_max * 1.5 {
                    let diff = -pos.signum() * (pos.abs() - self.p.mm_max);
                    if diff > 0.0 {
                        orders.buy(diff, o * (1.0 + slip));
                    } else {
                        orders.sell(-diff, o * (1.0 - slip));
                    }
                    self.sent = diff;
                }
                let fair = o * (1.0 - self.p.skew * pos);
                let mut bid = fair * (1.0 - self.p.min_half);
                let mut ask = fair * (1.0 + self.p.min_half);
                if let Some(b) = ob {
                    bid = bid.min(b + self.tick);
                }
                if let Some(a) = oa {
                    ask = ask.max(a - self.tick);
                }
                bid = bid.min(o * (1.0 - self.p.min_half));
                ask = ask.max(o * (1.0 + self.p.min_half));
                let bsz = self.p.qsize.min(self.p.mm_max * 2.0 - pos).max(0.0);
                let asz = self.p.qsize.min(self.p.mm_max * 2.0 + pos).max(0.0);
                let pull = 2.5 * self.p.qsize;
                let bsz = if self.flow < -pull { 0.0 } else { bsz };
                let asz = if self.flow > pull { 0.0 } else { asz };
                if bsz >= 0.05 {
                    orders.bid(bid, bsz);
                }
                if asz >= 0.05 {
                    orders.ask(ask, asz);
                }
            }
        }
        Ok(())
    }
}

fn main() -> BotResult {
    mtx_kit::run(Bot::default())
}
