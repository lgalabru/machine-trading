//! The tutorial's second strategy: buy a dip, sell the recovery.
//!
//! When the mark falls 1% below where the session started, it buys half the
//! cap; when the mark is back where it started, it sells it all. A
//! flash-crash session is made of such dips, and some of them don't come
//! back: watch what that costs.
//!
//! ```sh
//! mtx bot run --bot bots/first-strategy --debug
//! ```
//!
//! (`specs/guides/bots/first-strategy.md` walks through it.)

use mtx_kit::prelude::*;

/// How far below the start the mark must fall before buying, in percent.
const DIP_PCT: f64 = 1.0;

#[derive(Default)]
struct BuyTheDip {
    start_mark: f64,
}

impl Strategy for BuyTheDip {
    fn start(&mut self, session: &Session) -> BotResult {
        self.start_mark = session.start_mark;
        Ok(())
    }

    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        let change_pct = (market.mark / self.start_mark - 1.0) * 100.0;
        let holding = market.position.size;
        if holding == 0.0 && change_pct <= -DIP_PCT {
            println!("down {:.2}% from the start: buying", -change_pct);
            orders.target_position(market.cap / 2.0);
        } else if holding > 0.0 && change_pct >= 0.0 {
            println!("back to the start: selling");
            orders.target_position(0.0);
        }
        Ok(())
    }
}

mtx_kit::export!(BuyTheDip);
