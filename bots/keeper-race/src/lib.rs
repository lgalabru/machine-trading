//! The liquidation keeper's smoke bot: it buys the dip like `first-strategy`
//! and holds, but cuts to a quarter of its size once the mark is 3% under
//! the start, before a leveraged account reaches maintenance margin. Beside
//! a bot that holds, it shows what a keeper's liquidation cost and what
//! reading the margin early saves.

use mtx_kit::prelude::*;

/// How far below the start the mark must fall before buying, in percent.
const DIP_PCT: f64 = 1.0;
/// How far below the start the mark must fall before cutting, in percent.
const CUT_PCT: f64 = 3.0;

#[derive(Default)]
struct BuyAndCut {
    start_mark: f64,
    cut: bool,
}

impl Strategy for BuyAndCut {
    fn start(&mut self, session: &Session) -> BotResult {
        self.start_mark = session.start_mark;
        Ok(())
    }

    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        let change_pct = (market.mark / self.start_mark - 1.0) * 100.0;
        if self.cut {
            return Ok(());
        }
        if market.position.size == 0.0 && change_pct <= -DIP_PCT {
            orders.target_position(market.cap / 2.0);
        } else if market.position.size > 0.0 && change_pct <= -CUT_PCT {
            orders.target_position(market.cap / 8.0);
            self.cut = true;
        }
        Ok(())
    }
}

mtx_kit::export!(BuyAndCut);
