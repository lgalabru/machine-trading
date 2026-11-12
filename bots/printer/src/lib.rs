//! The tutorial's first strategy: it trades nothing, and prints what it is
//! handed, so you can see what a strategy works with. Run it step by step:
//!
//! ```sh
//! mtx bot run --bot bots/printer --debug
//! ```
//!
//! (`specs/guides/bots/first-strategy.md` walks through it.)

use mtx_kit::prelude::*;

#[derive(Default)]
struct Printer;

impl Strategy for Printer {
    /// Once, before the first step: what doesn't change in the session.
    fn start(&mut self, session: &Session) -> BotResult {
        println!(
            "{} for {} steps, from slot {} to {}",
            session.symbol, session.steps, session.start_slot, session.end_slot
        );
        println!(
            "the mark starts at {:.2}; I may hold up to {} {} either way",
            session.start_mark, session.cap, session.symbol
        );
        Ok(())
    }

    /// Every step: the market as it is now, and an empty order sheet.
    fn step(&mut self, market: &Market, _orders: &mut Orders) -> BotResult {
        println!(
            "mark {:.2}, oracle {:.2}, holding {}",
            market.mark, market.oracle, market.position.size
        );
        if market.last {
            println!("the last step: the account is valued after it");
        }
        Ok(())
    }
}

mtx_kit::export!(Printer);
