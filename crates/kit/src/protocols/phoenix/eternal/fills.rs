//! Phoenix Eternal's fills and resting orders, from its own records: the
//! market events its `Log` instructions carry ([`phoenix_rise::events`]),
//! as the Phoenix venue profile (Perpetual markets, Measurements) defines
//! them.
//!
//! An instruction that trades a market logs a `Header` (the market's
//! sequence number, its asset, the acting trader account), then an
//! `OrderFilled` for each resting order it took from and a `SplineFilled`
//! for each spline, each naming the maker's trader account and the maker's
//! side, and an `OrderPlaced` for an order it left resting. Positions live
//! in Phoenix's market-wide state, so a maker's account is in none of the
//! transactions that fill it: an enforcer reads the market's transactions,
//! and every trade names the market's order book.
//!
//! Each run of fills is closed by a `TradeSummary` whose `trader` is the
//! run's taker, on the side opposite the makers', and which carries the fee
//! it paid. That need not be the header's trader: an instruction can match
//! orders between other parties (a spline that re-centres crosses a stale
//! resting order) and logs that under the same header. Only a run no
//! summary closes takes the header's trader as its taker.
//!
//! Fees are Phoenix's, in whole quote lots: the summary's fee split across
//! its run by quote lots filled, the remainder to the last fill; and each
//! maker's fee from its own rate on the fill's quote lots, rounded as
//! Phoenix rounds it (a fee up, a rebate down). An `AdditionalFeeAccrued`
//! event, an extra fee on a resting order, is not added to the maker's.

use phoenix_rise::{
    events::{
        MarketEvent, PhoenixLogInstruction, market_events::MarketEventHeader, parse as parse_events,
    },
    ix::PHOENIX_PROGRAM_ID,
    math::{QuoteLots, SignedFeeRateMicro},
};
use rust_decimal::Decimal;
use solana_pubkey::Pubkey;
use uuid::Uuid;

use super::PhoenixMarket;
use crate::{
    chain::TransactionSummary,
    core::{Fill, FillId, InstrumentId, OrderId, RecordedFill, Side, VenueId},
};

/// A match one of Phoenix's perp markets recorded, between any two trader
/// accounts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PerpFill {
    /// The slot it landed in.
    pub slot: u64,
    /// The market, as the mandate names it.
    pub instrument: InstrumentId,
    /// The market's sequence number for the instruction that logged it, and
    /// the event's place among that instruction's events: never the same
    /// twice on one market.
    pub sequence: (u64, u32),
    /// The taker's and the maker's trader accounts.
    pub taker: Pubkey,
    pub maker: Pubkey,
    /// The taker's side.
    pub taker_side: Side,
    /// Price, in USDC per base unit, before fees.
    pub price: Decimal,
    /// Size, in base units.
    pub quantity: Decimal,
    /// When Phoenix says it happened, in Unix seconds.
    pub unix_timestamp: i64,
    /// The fee the taker paid on this fill, in USDC: its share of the
    /// trade summary's fee, by quote lots filled, the last fill of the run
    /// taking the remainder. Zero where no summary closes the run.
    pub taker_fee: Decimal,
    /// The fee the maker paid on this fill, in USDC: its fee rate applied
    /// to the fill's quote lots as Phoenix applies it. Negative is a rebate.
    pub maker_fee: Decimal,
}

impl PerpFill {
    /// The fill as one party traded it, on `side`. Its id comes from
    /// [`PerpFill::sequence`] (the maker's and the taker's differ by the
    /// side), so the same fill read twice has the same id. Its fees are
    /// left at zero: [`PerpMarketRecords::fills_for`] sets each party's.
    pub fn recorded(&self, side: Side) -> RecordedFill {
        let (sequence, event) = self.sequence;
        let party = (u64::from(event) << 1) | u64::from(side == Side::Sell);
        RecordedFill {
            slot: self.slot,
            fill: Fill {
                id: FillId(Uuid::from_u64_pair(sequence, party)),
                order_id: OrderId::new(),
                instrument: self.instrument.clone(),
                venue: VenueId::new(super::VENUE),
                side,
                price: self.price,
                quantity: self.quantity,
                fees: Decimal::ZERO,
                timestamp: chrono::DateTime::from_timestamp(self.unix_timestamp, 0)
                    .unwrap_or_default(),
                latency: None,
                strategy: None,
            },
        }
    }
}

/// An order a trader account left resting on a perp market.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PerpOrderPlaced {
    /// The slot it landed in.
    pub slot: u64,
    /// The market's sequence number for the instruction that placed it, and
    /// the event's place among that instruction's events.
    pub sequence: (u64, u32),
    /// The trader account it rests for.
    pub trader: Pubkey,
    pub side: Side,
    /// Price, in USDC per base unit.
    pub price: Decimal,
    /// What rested, in base units.
    pub quantity: Decimal,
}

/// What some trader accounts did on a market: their fills, each on the
/// side they traded, and the orders they left resting, oldest first.
#[derive(Debug, Clone, Default)]
pub struct TraderRecords {
    pub fills: Vec<RecordedFill>,
    pub placed: Vec<PerpOrderPlaced>,
}

/// What one market recorded: its fills and the orders left resting, each
/// oldest first.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PerpMarketRecords {
    pub fills: Vec<PerpFill>,
    pub placed: Vec<PerpOrderPlaced>,
}

impl PerpMarketRecords {
    /// Adds what a landed transaction recorded on `market`. A failed
    /// transaction recorded nothing.
    pub fn add_transaction(&mut self, summary: &TransactionSummary, market: &PhoenixMarket) {
        if summary.error.is_some() {
            return;
        }
        // Each of the transaction's instructions logs its own batches.
        let mut outer: Vec<u8> = summary
            .inner_instructions
            .iter()
            .map(|ix| ix.outer)
            .collect();
        outer.dedup();
        for index in outer {
            let logs = summary
                .inner_instructions
                .iter()
                .filter(|ix| ix.outer == index && ix.program == *PHOENIX_PROGRAM_ID)
                .filter_map(|ix| PhoenixLogInstruction::from_instruction_data(&ix.data));
            self.add_events(&parse_events(logs), market, summary);
        }
    }

    fn add_events(
        &mut self,
        events: &[MarketEvent],
        market: &PhoenixMarket,
        summary: &TransactionSummary,
    ) {
        let mut header: Option<&MarketEventHeader> = None;
        // The fills since the last trade summary, with their quote lots: its
        // trader took them.
        let mut run: Vec<(usize, u64)> = Vec::new();
        let quote = |lots: i64| Decimal::new(lots, u32::from(market.quote_lot_decimals));
        let mut unix_timestamp = summary.block_time.map_or(0, |t| t.timestamp());
        for (i, event) in events.iter().enumerate() {
            let on_market = header.filter(|h| h.asset_id == market.asset_id);
            let fill =
                move |maker: [u8; 32], maker_side, ticks: u64, lots: u64, h: &MarketEventHeader| {
                    PerpFill {
                        slot: summary.slot,
                        instrument: market.id().clone(),
                        sequence: (h.sequence_number, i as u32),
                        taker: Pubkey::new_from_array(h.trader_account),
                        maker: Pubkey::new_from_array(maker),
                        taker_side: opposite(side(maker_side)),
                        price: market.ticks_to_price(ticks),
                        quantity: market.base_lots_to_quantity(lots as i64),
                        unix_timestamp,
                        taker_fee: Decimal::ZERO,
                        maker_fee: Decimal::ZERO,
                    }
                };
            match (event, on_market) {
                (MarketEvent::SlotContext(context), _) => {
                    unix_timestamp = context.timestamp as i64;
                }
                (MarketEvent::Header(next), _) => {
                    header = Some(next);
                    run.clear();
                }
                (MarketEvent::OrderFilled(f), Some(h)) => {
                    let mut taken = fill(
                        f.maker,
                        f.side,
                        f.price.as_inner(),
                        f.base_lots_filled.as_inner(),
                        h,
                    );
                    taken.maker_fee = quote(maker_fee_lots(f.maker_fee_rate, f.quote_lots_filled));
                    run.push((self.fills.len(), f.quote_lots_filled.as_inner()));
                    self.fills.push(taken);
                }
                (MarketEvent::SplineFilled(f), Some(h)) => {
                    let mut taken = fill(
                        f.maker,
                        f.side,
                        f.price.as_inner(),
                        f.base_lots_filled.as_inner(),
                        h,
                    );
                    taken.maker_fee = quote(maker_fee_lots(f.maker_fee_rate, f.quote_lots_filled));
                    run.push((self.fills.len(), f.quote_lots_filled.as_inner()));
                    self.fills.push(taken);
                }
                (MarketEvent::TradeSummary(trade), Some(_)) => {
                    let taker = Pubkey::new_from_array(trade.trader);
                    let shares = split(trade.fee_in_quote_lots.as_inner(), &run);
                    for (&(i, _), share) in run.iter().zip(shares) {
                        let fill = &mut self.fills[i];
                        fill.taker = taker;
                        fill.taker_fee = quote(share);
                    }
                    run.clear();
                }
                (MarketEvent::OrderPlaced(p), Some(h)) => {
                    let lots = p.quantity.as_inner();
                    self.placed.push(PerpOrderPlaced {
                        slot: summary.slot,
                        sequence: (h.sequence_number, i as u32),
                        trader: Pubkey::new_from_array(h.trader_account),
                        side: if lots < 0 { Side::Sell } else { Side::Buy },
                        price: market.ticks_to_price(p.price.as_inner()),
                        quantity: market.base_lots_to_quantity(lots.abs()),
                    });
                }
                _ => {}
            }
        }
    }

    /// Puts the records in the market's own order: by slot, then sequence.
    pub fn sort(&mut self) {
        self.fills.sort_by_key(|f| (f.slot, f.sequence));
        self.placed.sort_by_key(|p| (p.slot, p.sequence));
    }

    /// The fills `accounts` took part in, each as they traded it, with the
    /// fee it paid in that role: a trade between two of them counts once
    /// for each side.
    pub fn fills_for(&self, accounts: &[Pubkey]) -> Vec<RecordedFill> {
        let mut own = Vec::new();
        for fill in &self.fills {
            if accounts.contains(&fill.taker) {
                let mut taken = fill.recorded(fill.taker_side);
                taken.fill.fees = fill.taker_fee;
                own.push(taken);
            }
            if accounts.contains(&fill.maker) {
                let mut made = fill.recorded(opposite(fill.taker_side));
                made.fill.fees = fill.maker_fee;
                own.push(made);
            }
        }
        own
    }

    /// The orders `accounts` left resting.
    pub fn placed_by(&self, accounts: &[Pubkey]) -> Vec<PerpOrderPlaced> {
        self.placed
            .iter()
            .filter(|p| accounts.contains(&p.trader))
            .cloned()
            .collect()
    }
}

/// What a maker paid on `quote_lots` filled at `rate`, in quote lots, as
/// Phoenix charges it: a fee rounded up, a rebate (negative) down.
fn maker_fee_lots(rate: SignedFeeRateMicro, quote_lots: QuoteLots) -> i64 {
    rate.apply_to_quote_lots(quote_lots)
        .map_or(0, |fee| fee.as_inner())
}

/// `fee` quote lots split across a run of fills by their quote lots: each
/// fill its share rounded down, the last the remainder, so the shares add
/// up to the fee exactly. A run with no quote lots puts it all on the last.
fn split(fee: u64, run: &[(usize, u64)]) -> Vec<i64> {
    let total: u128 = run.iter().map(|&(_, lots)| u128::from(lots)).sum();
    let mut shares: Vec<i64> = run
        .iter()
        .map(|&(_, lots)| match total {
            0 => 0,
            _ => (u128::from(fee) * u128::from(lots) / total) as i64,
        })
        .collect();
    if let Some((last, before)) = shares.split_last_mut() {
        *last = fee as i64 - before.iter().sum::<i64>();
    }
    shares
}

fn side(side: phoenix_rise::math::Side) -> Side {
    match side {
        phoenix_rise::math::Side::Bid => Side::Buy,
        phoenix_rise::math::Side::Ask => Side::Sell,
    }
}

fn opposite(side: Side) -> Side {
    match side {
        Side::Buy => Side::Sell,
        Side::Sell => Side::Buy,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fee_splits_by_quote_lots_and_adds_up() {
        // 10 over 1 and 2 lots: 3.33 rounds down to 3, the last takes 7.
        assert_eq!(split(10, &[(0, 1), (1, 2)]), vec![3, 7]);
        assert_eq!(split(10, &[(0, 1), (1, 1), (2, 1)]), vec![3, 3, 4]);
        assert_eq!(split(5, &[(0, 0), (1, 0)]), vec![0, 5]);
        assert_eq!(split(0, &[(0, 4)]), vec![0]);
        assert!(split(9, &[]).is_empty());
    }

    #[test]
    fn a_makers_fee_rounds_up_and_a_rebate_down() {
        let fee = |rate, lots| maker_fee_lots(SignedFeeRateMicro::new(rate), QuoteLots::new(lots));
        assert_eq!(fee(250, 1_000_001), 251);
        assert_eq!(fee(250, 1_000_000), 250);
        assert_eq!(fee(-50, 1_000_001), -50);
        assert_eq!(fee(-50, 999_999), -49);
        assert_eq!(fee(0, 123), 0);
    }
}

#[cfg(test)]
pub(crate) mod testing {
    //! Log instructions as Phoenix writes them, for tests.

    use phoenix_rise::events::{
        LOG_EVENT_LENGTHS_INSTRUCTION_TAG, LOG_INSTRUCTION_TAG, MarketEvent,
    };

    /// The two instructions Phoenix logs `events` with: their lengths, then
    /// the events, as batch `batch`. Each is the data of an inner
    /// instruction of the program.
    pub(crate) fn log_instructions(batch: u32, events: &[MarketEvent]) -> [Vec<u8>; 2] {
        let encoded: Vec<Vec<u8>> = events
            .iter()
            .map(|e| borsh::to_vec(e).expect("events encode"))
            .collect();
        let count = (events.len() as u32).to_le_bytes();
        let mut lengths = LOG_EVENT_LENGTHS_INSTRUCTION_TAG.to_le_bytes().to_vec();
        lengths.extend_from_slice(&batch.to_le_bytes());
        lengths.extend_from_slice(&count);
        for e in &encoded {
            lengths.extend_from_slice(&(e.len() as u16).to_le_bytes());
        }
        let mut log = LOG_INSTRUCTION_TAG.to_le_bytes().to_vec();
        log.extend_from_slice(&batch.to_le_bytes());
        log.extend_from_slice(&count);
        for e in encoded {
            log.extend_from_slice(&e);
        }
        [lengths, log]
    }
}
