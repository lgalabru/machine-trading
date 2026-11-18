//! The ladder's rungs that work the order without learning which maker
//! knows. A component has no environment, so each rung is its own build,
//! its rule set by `MTX_NAIVE` at build time:
//!
//! - `dump`: the whole order at once, crossing.
//! - `twap`: evenly over the session, crossing.
//! - `late`: holds off, and finishes late while the book beats the broker,
//!   as the reference does with no read.
//! - `passive`: rests the rest of the order at the touch on its side all
//!   session, and finishes late: good prices only.
//! - `mean-skew`: paces the order on the mean lean of every maker on the
//!   book, as the reference paces on the maker it chose.
//! - `most-skewed`: on whichever maker leans furthest at each step.
//! - `seat-a`: on one maker, the first on the book by its trader number:
//!   "always act as if this one knows".
//! - `probe`: trades nothing and logs the book's makers each step.

use mtx_bot_skew_maker_follow::{
    BookReader, IndexHistory, Leans, Order, Pace, Pacer, cross, finish_late, index_of, makers,
    round_down_cents, round_lots, round_up_cents,
};
use mtx_kit::{log, prelude::*};

#[derive(Default)]
pub struct Naive {
    mode: String,
    order: Order,
    book: BookReader,
    history: IndexHistory,
    leans: Leans,
    pacer: Pacer,
    seat: Option<u32>,
}

impl Strategy for Naive {
    fn start(&mut self, session: &Session) -> BotResult {
        self.mode = option_env!("MTX_NAIVE")
            .ok_or("built without MTX_NAIVE")?
            .to_string();
        self.order = Order::from_session(session)?;
        self.book = BookReader::new(session)?;
        // Three makers' mean is a third of one's lean, and two leaning
        // together reach the others' thresholds' half.
        let pace = if self.mode == "mean-skew" {
            Pace {
                now: 0.3,
                wait: 0.15,
            }
        } else {
            Pace::default()
        };
        self.pacer = Pacer::new(pace);
        Ok(())
    }

    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        let order = self.order;
        let left = order.left(market.position.size);
        match self.mode.as_str() {
            "dump" => {
                orders.cancel_all();
                cross(market, orders, &order, left);
                return Ok(());
            }
            "twap" => {
                orders.cancel_all();
                let due = order.quantity * order.progress(market.slot);
                let done = order.quantity - left;
                cross(market, orders, &order, round_lots(due - done));
                return Ok(());
            }
            "late" => {
                orders.cancel_all();
                finish_late(market, orders, &order);
                return Ok(());
            }
            "passive" => {
                orders.cancel_all();
                let size = round_lots(left);
                if size >= 0.001 {
                    if order.buy {
                        if let Some(bid) = market.best_bid {
                            orders.bid(round_down_cents(bid), size);
                        }
                    } else if let Some(ask) = market.best_ask {
                        orders.ask(round_up_cents(ask), size);
                    }
                }
                return Ok(());
            }
            _ => {}
        }
        let index = index_of(market);
        self.history.push(market.slot, index);
        let book = self.book.read();
        let makers = makers(&book, self.book.tick_usd);
        self.leans.update(&makers, &self.history);
        let skews = self.leans.all();
        let signal = match self.mode.as_str() {
            "mean-skew" if !skews.is_empty() => {
                skews.iter().map(|(_, s)| s).sum::<f64>() / skews.len() as f64
            }
            "mean-skew" => 0.0,
            "most-skewed" => skews
                .iter()
                .map(|(_, s)| *s)
                .max_by(|a, b| a.abs().total_cmp(&b.abs()))
                .unwrap_or(0.0),
            "seat-a" => {
                if self.seat.is_none() && skews.len() >= 3 {
                    self.seat = skews.first().map(|(trader, _)| *trader);
                }
                self.seat
                    .and_then(|seat| skews.iter().find(|(t, _)| *t == seat))
                    .map_or(0.0, |(_, s)| *s)
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
                orders.cancel_all();
                return Ok(());
            }
            other => return Err(format!("unknown MTX_NAIVE {other}").into()),
        };
        self.pacer.step(market, orders, &order, signal);
        Ok(())
    }
}

#[cfg(target_arch = "wasm32")]
mtx_kit::export!(Naive);
