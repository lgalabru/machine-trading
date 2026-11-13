//! The RFQ dust check's bot: every step it answers every open request for
//! a quote with something that can't settle, in turn a price of no USDC
//! unit on both sides, one on the ask alone, and the mark for a size of no
//! base unit. The board refuses each, so its session must grade like any
//! other, with nothing filled.

use mtx_kit::{LineOptions, prelude::*};

const DUST: f64 = 1e-9;

#[derive(Default)]
struct Dust {
    answers: u64,
}

impl Strategy for Dust {
    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        let mark = market.mark;
        for rfq in &market.rfqs {
            let (bid, ask, size) = match self.answers % 3 {
                0 => (DUST, DUST, None),
                1 => (mark, DUST, None),
                _ => (mark, mark, Some(DUST)),
            };
            self.answers += 1;
            let answer = Intent::Respond {
                id: rfq.id.clone(),
                bid: Some(bid),
                ask: Some(ask),
                size,
            };
            orders.push(answer, LineOptions::default()).at(&rfq.venue);
        }
        Ok(())
    }
}

mtx_kit::export!(Dust);
