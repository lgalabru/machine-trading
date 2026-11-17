//! The ladder's rungs that trade without learning which maker knows. A
//! component has no environment, so each rung is its own build, its rule
//! set by `MTX_NAIVE` at build time:
//!
//! - `mean-skew`: follows the mean skew of every maker on the book, leaning
//!   and taking as the reference follows the maker it chose.
//! - `most-skewed`: follows whichever maker leans furthest at each step.
//! - `seat-a`: always follows one maker, the first on the book by its
//!   trader number: "always act as if this one knows".
//! - `twap`: buys the cap evenly over the session and holds it.
//! - `neutral`: quotes both sides around the index, leaning nowhere.
//! - `probe`: trades nothing and logs the book's makers each step.

use mtx_bot_skew_maker_follow::{
    BookReader, Follow, Follower, IndexHistory, LEAN_SIZE, Leans, TAKE_DEPTH_BPS, index_of, makers,
    round_down_cents, round_lots, round_up_cents,
};
use mtx_kit::{log, prelude::*};

/// The neutral quote's half-width, in basis points.
const NEUTRAL_BPS: f64 = 5.0;

#[derive(Default)]
pub struct Naive {
    mode: String,
    book: BookReader,
    history: IndexHistory,
    leans: Leans,
    follower: Follower,
    seat: Option<u32>,
    start_slot: u64,
    end_slot: u64,
}

impl Strategy for Naive {
    fn start(&mut self, session: &Session) -> BotResult {
        self.mode = option_env!("MTX_NAIVE")
            .ok_or("built without MTX_NAIVE")?
            .to_string();
        self.book = BookReader::new(session)?;
        // Three makers' mean is a third of one's lean, and two leaning
        // together reach the others' thresholds' half: the best of 0.1 to
        // 0.3 on a replay of 48 sessions.
        let follow = if self.mode == "mean-skew" {
            Follow {
                take: 0.3,
                lean: 0.15,
            }
        } else {
            Follow::default()
        };
        self.follower = Follower::new(follow);
        self.start_slot = session.start_slot;
        self.end_slot = session.end_slot;
        Ok(())
    }

    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        let index = index_of(market);
        self.history.push(market.slot, index);
        let book = self.book.read();
        let makers = makers(&book, self.book.tick_usd);
        self.leans.update(&makers, &self.history);
        let skews = self.leans.all();
        match self.mode.as_str() {
            "mean-skew" => {
                let mean = if skews.is_empty() {
                    0.0
                } else {
                    skews.iter().map(|(_, s)| s).sum::<f64>() / skews.len() as f64
                };
                self.follower.step(market, orders, mean);
            }
            "most-skewed" => {
                let most = skews
                    .iter()
                    .map(|(_, s)| *s)
                    .max_by(|a, b| a.abs().total_cmp(&b.abs()))
                    .unwrap_or(0.0);
                self.follower.step(market, orders, most);
            }
            "seat-a" => {
                if self.seat.is_none() && skews.len() >= 3 {
                    self.seat = skews.first().map(|(trader, _)| *trader);
                }
                let signal = self
                    .seat
                    .and_then(|seat| skews.iter().find(|(t, _)| *t == seat))
                    .map_or(0.0, |(_, s)| *s);
                self.follower.step(market, orders, signal);
            }
            "twap" => {
                orders.cancel_all();
                let span = self.end_slot.saturating_sub(self.start_slot).max(1) as f64;
                let done = (market.slot.saturating_sub(self.start_slot) as f64 / span).min(1.0);
                let size = round_lots(market.cap * done - market.position.size);
                if size >= 0.001 && index > 0.0 {
                    orders.buy(
                        size,
                        round_up_cents(index * (1.0 + TAKE_DEPTH_BPS / 10_000.0)),
                    );
                }
            }
            "neutral" => {
                orders.cancel_all();
                let held = market.position.size;
                let bid = round_lots((market.cap - held).clamp(0.0, LEAN_SIZE));
                let ask = round_lots((market.cap + held).clamp(0.0, LEAN_SIZE));
                if index > 0.0 {
                    if bid >= 0.001 {
                        orders.bid(
                            round_down_cents(index * (1.0 - NEUTRAL_BPS / 10_000.0)),
                            bid,
                        );
                    }
                    if ask >= 0.001 {
                        orders.ask(round_up_cents(index * (1.0 + NEUTRAL_BPS / 10_000.0)), ask);
                    }
                }
            }
            "probe" => {
                let mut seen: Vec<String> = book
                    .iter()
                    .map(|r| {
                        let p = self.book.price(r.ticks);
                        format!(
                            "{}{}{:+.1}@{:.3}s{}",
                            r.trader,
                            if r.bid { "b" } else { "a" },
                            (p / index - 1.0) * 10_000.0,
                            r.size,
                            r.slot
                        )
                    })
                    .collect();
                seen.sort();
                let touch = |p: Option<f64>| p.map_or(f64::NAN, |p| (p / index - 1.0) * 10_000.0);
                log(&format!(
                    "slot {} index {:.4} mark {:.4} bid {:+.1} ask {:+.1} | {}",
                    market.slot,
                    index,
                    market.mark,
                    touch(market.best_bid),
                    touch(market.best_ask),
                    seen.join(" ")
                ));
            }
            other => return Err(format!("unknown MTX_NAIVE {other}").into()),
        }
        Ok(())
    }
}

#[cfg(target_arch = "wasm32")]
mtx_kit::export!(Naive);
