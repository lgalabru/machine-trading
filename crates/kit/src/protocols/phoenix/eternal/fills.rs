//! Phoenix Eternal's fills and resting orders, from its own records: the
//! market events its `Log` instructions carry ([`phoenix_rise::events`]),
//! as the Phoenix venue profile (Perpetual markets, Measurements) defines
//! them.
//!
//! An instruction that trades a market logs a `Header` (the market's
//! sequence number, its asset, the acting trader account), then an
//! `OrderFilled` for each resting order it took from and a `SplineFilled`
//! for each spline, each naming the maker's trader account and the maker's
//! side, and an `OrderPlaced` for an order it left resting. The taker is the
//! header's trader account, on the side opposite the maker's. Positions live
//! in Phoenix's market-wide state, so a maker's account is in none of the
//! transactions that fill it: an enforcer reads the market's transactions,
//! and every trade names the market's order book.
//!
//! An instruction can also match orders between other parties: a spline
//! that re-centres crosses a stale resting order, and Phoenix logs that
//! under the same header. Each run of fills is closed by a `TradeSummary`
//! naming the trade's real taker and its fee, so a fill's taker is the
//! summary's trader, not the header's.

use phoenix_rise::{
    events::{
        MarketEvent, PhoenixLogInstruction, market_events::MarketEventHeader, parse as parse_events,
    },
    ix::PHOENIX_PROGRAM_ID,
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
    /// trade's fee, by notional. Zero where the log has no trade summary.
    pub taker_fee: Decimal,
    /// The fee the maker paid on this fill, in USDC, at its fee rate;
    /// negative is a rebate.
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
        // The fills since the last trade summary: its trader took them.
        let mut run: Vec<usize> = Vec::new();
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
                    taken.maker_fee = maker_fee(&taken, i64::from(f.maker_fee_rate.as_inner()));
                    run.push(self.fills.len());
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
                    taken.maker_fee = maker_fee(&taken, i64::from(f.maker_fee_rate.as_inner()));
                    run.push(self.fills.len());
                    self.fills.push(taken);
                }
                (MarketEvent::TradeSummary(trade), Some(_)) => {
                    let taker = Pubkey::new_from_array(trade.trader);
                    let fee = Decimal::from(trade.fee_in_quote_lots.as_inner())
                        * Decimal::new(1, u32::from(market.quote_lot_decimals));
                    let notional: Decimal = run
                        .iter()
                        .map(|&i| self.fills[i].price * self.fills[i].quantity)
                        .sum();
                    for &i in &run {
                        let fill = &mut self.fills[i];
                        fill.taker = taker;
                        if !notional.is_zero() {
                            fill.taker_fee =
                                (fee * fill.price * fill.quantity / notional).round_dp(8);
                        }
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

/// What a maker paid on `fill`, at its fee rate in millionths of the
/// notional (negative for a rebate).
fn maker_fee(fill: &PerpFill, rate_micro: i64) -> Decimal {
    (fill.price * fill.quantity * Decimal::new(rate_micro, 6)).round_dp(8)
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
