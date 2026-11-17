//! The ladder's careful rungs that don't read who is who: told the reach
//! (`tolerance_bps`, from `MTX_REGIME`) and nothing else they use, they
//! answer by a fixed rule. RFQs reach components only, and a component
//! has no environment, so each rung is its own build: `MTX_NAIVE` (and the
//! rule's settings) at build time picks the rule.
//!
//! - `quote-all`: every request just inside its reach;
//! - `quote-wide`: every request 3 bp past its reach;
//! - `fixed-width`: every request at `MTX_WIDTH` bp, whatever its size;
//! - `size-tier`: requests of at least `MTX_TIER_N` lots inside their
//!   reach, smaller ones declined;
//! - `side-skew`: every request inside its reach, unless the requests of the
//!   last `MTX_WINDOW` slots lean `MTX_LEAN` or more to its side.
//! - `perp-front-run`: quotes every request inside its reach, and trades
//!   the perp the way each request asks, ahead of the clients' move. The
//!   order sheet cuts it to the mandate's 0.001 SPCX cap on the perp, so it
//!   grades as quote-all: the perp is no route around the desk (checked by
//!   hand; the ladder's exploit check needs a hindsight bound, which an RFQ
//!   desk's sessions don't have).

use mtx_bot_rfq_client_tiering::{Answer, Ask, Desk, EPS_BP, Policy, Tag, Told, View, reach_bp};
use mtx_kit::prelude::*;

#[derive(Default)]
pub struct Naive {
    rule: String,
    told: Told,
    width: f64,
    tier_n: f64,
    window: u64,
    lean: f64,
}

fn setting(value: Option<&str>, default: f64) -> f64 {
    value.and_then(|v| v.parse().ok()).unwrap_or(default)
}

impl Policy for Naive {
    fn start(&mut self, session: &Session) {
        self.told = Told::read(session);
        self.rule = option_env!("MTX_NAIVE").unwrap_or("quote-all").to_string();
        self.width = setting(option_env!("MTX_WIDTH"), 2.0);
        self.tier_n = setting(option_env!("MTX_TIER_N"), 5.0);
        self.window = setting(option_env!("MTX_WINDOW"), 80.0) as u64;
        self.lean = setting(option_env!("MTX_LEAN"), 0.1);
    }

    fn answer(&mut self, view: &View<'_>, ask: &Ask) -> Answer {
        let reach = reach_bp(self.told.tolerance_bps, ask.n);
        let quote = |bp: f64| Answer::Quote {
            bp,
            size: None,
            tag: Tag::Other,
        };
        match self.rule.as_str() {
            "quote-wide" => quote(reach + 3.0),
            "fixed-width" => quote(self.width),
            "size-tier" if ask.n < self.tier_n => Answer::Decline,
            "side-skew" => {
                let recent: Vec<f64> = view
                    .requests
                    .iter()
                    .rev()
                    .skip(1)
                    .take_while(|r| r.slot + self.window > view.slot)
                    .map(|r| r.sign)
                    .collect();
                let n = recent.len();
                let mean = recent.iter().sum::<f64>() / n.max(1) as f64;
                if n >= 3 && ask.sign * mean >= self.lean {
                    Answer::Decline
                } else {
                    quote(reach - EPS_BP)
                }
            }
            _ => quote(reach - EPS_BP),
        }
    }
}

/// A rung: the desk with its rule, and for the exploit, its perp trades.
#[derive(Default)]
pub struct Bot {
    desk: Desk<Naive>,
    seen: std::collections::HashSet<String>,
}

impl Strategy for Bot {
    fn start(&mut self, session: &Session) -> BotResult {
        self.desk.start(session)
    }

    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        self.desk.step(market, orders)?;
        if option_env!("MTX_NAIVE") != Some("perp-front-run") || market.index <= 0.0 {
            return Ok(());
        }
        for rfq in &market.rfqs {
            if !self.seen.insert(rfq.id.clone()) {
                continue;
            }
            match rfq.side {
                Some(Side::Bid) => {
                    orders.buy(rfq.size, market.index * 1.01);
                }
                Some(Side::Ask) => {
                    orders.sell(rfq.size, market.index * 0.99);
                }
                None => {}
            }
        }
        Ok(())
    }
}

#[cfg(target_arch = "wasm32")]
mtx_kit::export!(Bot);
