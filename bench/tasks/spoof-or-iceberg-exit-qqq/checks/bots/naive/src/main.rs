//! The ladder's rungs that trade without reading who owns a wall, picked by
//! `MTX_NAIVE`:
//!
//! - `dump`: sells the whole order at once.
//! - `twap`: sells evenly over the session, crossing.
//! - `late`: holds and sells late, as the reference does without a read.
//! - `trust-depth`: takes a large bid for support and a large ask for a
//!   seller ahead of it: holds on a bid wall, sells on an ask wall.
//! - `fade-walls`: takes every wall for a spoof: sells on a bid wall, holds
//!   on an ask wall.
//!
//! Each sells what is left late, as the reference does.

use mtx_bot_spoof_or_iceberg_exit::{
    BookReader, Order, WALL_MIN, round_lots, sell_late, sell_now,
};
use mtx_kit::prelude::*;

#[derive(Default)]
struct Naive {
    mode: String,
    order: Order,
    book: BookReader,
    selling: bool,
    decided: bool,
}

impl Strategy for Naive {
    fn start(&mut self, session: &Session) -> BotResult {
        self.mode = std::env::var("MTX_NAIVE").map_err(|_| "set MTX_NAIVE")?;
        self.order = Order::from_session(session)?;
        self.book = BookReader::new(session)?;
        Ok(())
    }

    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        orders.cancel_all();
        let held = market.position.size;
        match self.mode.as_str() {
            "dump" => self.selling = true,
            "twap" => {
                let due = self.order.quantity * (1.0 - self.order.progress(market.slot));
                let size = round_lots((held - due).max(0.0));
                if size >= 0.01 {
                    sell_now(market, orders, size);
                }
                return Ok(());
            }
            "late" => {}
            "trust-depth" | "fade-walls" if !self.decided => {
                let wall = self.book.read().into_iter().find(|r| r.initial >= WALL_MIN);
                if let Some(wall) = wall {
                    self.decided = true;
                    let fade = self.mode == "fade-walls";
                    self.selling = wall.bid == fade;
                }
            }
            "trust-depth" | "fade-walls" => {}
            other => return Err(format!("unknown MTX_NAIVE {other}").into()),
        }
        if self.selling {
            sell_now(market, orders, held);
        } else {
            sell_late(market, orders, &self.order);
        }
        Ok(())
    }
}

fn main() -> BotResult {
    mtx_kit::run(Naive::default())
}
