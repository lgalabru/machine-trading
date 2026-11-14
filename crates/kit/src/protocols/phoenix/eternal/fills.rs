//! Phoenix Eternal's fills, resting orders and forced position changes, from
//! its own records: the market events its `Log` instructions carry
//! ([`phoenix_rise::events`]), as the Phoenix venue profile (Perpetual
//! markets, Measurements) defines them.
//!
//! An instruction that trades a market logs a `Header` (the market's
//! sequence number, its asset, the acting trader account), then an
//! `OrderFilled` for each resting order it took from and a `SplineFilled`
//! for each spline, each naming the maker's trader account and the maker's
//! side, and an `OrderPlaced` for an order it placed to rest. Positions
//! live in Phoenix's market-wide state, so a maker's account is in none of
//! the transactions that fill it: an enforcer reads the market's
//! transactions, and every trade names the market's order book.
//!
//! Each run of fills is closed by a `TradeSummary` whose `trader` is the
//! run's taker, on the side opposite the makers', and which carries the fee
//! it paid. That need not be the header's trader: an instruction can match
//! orders between other parties (a spline that re-centres crosses a stale
//! resting order) and logs that under the same header. Only a run no
//! summary closes takes the header's trader as its taker.
//!
//! Two instructions move a position without a fill: `LiquidationTransfer`
//! (a keeper takes a liquidatee's position into its own account) and
//! `CloseMatchedPositions` (auto-deleveraging closes a long against a
//! short). Neither names the order book, so they show in the transactions
//! of the trader accounts they move. They are [`PerpPositionChange`]s: an
//! account's fills and its position changes together sum to its change in
//! position.
//!
//! Fees, in whole quote lots: a taker's is the summary's fee split across
//! its run by quote lots filled, each share rounded down and the last
//! taking the remainder (the split is the kit's: Phoenix records one fee
//! per run); a maker's is its own rate on the fill's quote lots, which
//! Phoenix rounds up for a fee and toward zero for a rebate. An
//! `AdditionalFeeAccrued` event, an extra fee on a resting order, is not
//! added to the maker's.
//!
//! An event that doesn't decode is an error, never skipped: skipping a
//! `Header` would put the fills after it on the header before it.

use std::collections::HashSet;

use phoenix_rise::{
    events::{
        MarketEvent, PhoenixLogInstruction, market_events::MarketEventHeader, parse_with_errors,
    },
    ix::PHOENIX_PROGRAM_ID,
    math::{QuoteLots, SignedFeeRateMicro},
};
use rust_decimal::Decimal;
use solana_pubkey::Pubkey;
use solana_transaction::Signature;

use super::PhoenixMarket;
use crate::{
    chain::TransactionSummary,
    core::{InstrumentId, MeterError, RecordedFill, Side},
    protocols::fills::{MarketFill, recorded_for},
};

/// An order a trader account placed to rest on a perp market (an
/// `OrderPlaced` event): it may since have filled or been cancelled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PerpOrderPlaced {
    /// The slot it landed in.
    pub slot: u64,
    /// The market's sequence number for the instruction that placed it,
    /// shifted up 32 bits, plus the event's place among the instruction's
    /// events, as a fill's [`MarketFill::sequence`].
    pub sequence: u128,
    /// The trader account it rests for.
    pub trader: Pubkey,
    /// The side it rests on: a bid is a buy.
    pub side: Side,
    /// Price, in USDC per base unit.
    pub price: Decimal,
    /// What was placed to rest, in base units.
    pub quantity: Decimal,
}

/// Why a position moved without a fill.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PositionChangeKind {
    /// `LiquidationTransfer`: a keeper took over part of a liquidatee's
    /// position, into its own trader account, at a haircut.
    LiquidationTransfer,
    /// `CloseMatchedPositions`: auto-deleveraging closed a long and a short
    /// against each other.
    AutoDeleverage,
}

/// One trader account's position moving without a fill, by Phoenix's
/// liquidation transfer or auto-deleveraging. Each such event moves two
/// accounts, and gives one change for each.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PerpPositionChange {
    /// The slot it landed in.
    pub slot: u64,
    /// The market, as the mandate names it.
    pub instrument: InstrumentId,
    /// As [`PerpOrderPlaced::sequence`]; the two accounts' changes share
    /// it.
    pub sequence: u128,
    pub kind: PositionChangeKind,
    /// The trader account whose position moved.
    pub trader: Pubkey,
    /// The other account in the event: the keeper or the liquidatee, the
    /// short or the long.
    pub counterparty: Pubkey,
    /// Which way the position moved, as a fill on this side would move it.
    pub side: Side,
    /// How far, in base units. Always positive.
    pub quantity: Decimal,
}

impl PerpPositionChange {
    /// The signed change in position, in base units: positive toward long.
    pub fn signed_quantity(&self) -> Decimal {
        self.side.sign() * self.quantity
    }
}

/// What some trader accounts did on a market, each list oldest first.
/// Their fills, signed by side, plus their position changes sum to their
/// change in position over the records' slots.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TraderRecords {
    /// Every fill they took part in, once for each side they traded (see
    /// [`MarketFill::sides_for`]), with the fee of the role they played.
    pub fills: Vec<RecordedFill>,
    /// The orders they placed to rest.
    pub placed: Vec<PerpOrderPlaced>,
    /// Their position changes that were not fills.
    pub position_changes: Vec<PerpPositionChange>,
}

/// What a perp market recorded in the transactions read, between any
/// accounts, each list in the market's own order (by slot, then sequence)
/// once [`PerpMarketRecords::sort`] has run. A transaction added twice
/// counts once.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PerpMarketRecords {
    /// Every match, its [`MarketFill::sequence`] the market's sequence
    /// number for the instruction, shifted up 32 bits, plus the event's
    /// place among its events, and its fees Phoenix's (see the module).
    pub fills: Vec<MarketFill>,
    /// Every order placed to rest.
    pub placed: Vec<PerpOrderPlaced>,
    /// Every position change that was not a fill.
    pub position_changes: Vec<PerpPositionChange>,
    /// The transactions added.
    transactions: HashSet<Signature>,
}

impl PerpMarketRecords {
    /// Adds what a landed transaction recorded on `market`: nothing for a
    /// failed transaction, or one already added. An event that doesn't
    /// decode, or that `market`'s records can't hold, is an error, and adds
    /// nothing of the transaction.
    pub fn add_transaction(
        &mut self,
        summary: &TransactionSummary,
        market: &PhoenixMarket,
    ) -> Result<(), MeterError> {
        if summary.error.is_some() || self.transactions.contains(&summary.signature) {
            return Ok(());
        }
        let mut added = PerpMarketRecords::default();
        // Each of the transaction's instructions logs its own batches.
        let mut outer: Vec<u8> = summary
            .inner_instructions
            .iter()
            .map(|ix| ix.outer)
            .collect();
        outer.sort_unstable();
        outer.dedup();
        for index in outer {
            let logs = summary
                .inner_instructions
                .iter()
                .filter(|ix| ix.outer == index && ix.program == *PHOENIX_PROGRAM_ID)
                .filter_map(|ix| PhoenixLogInstruction::from_instruction_data(&ix.data));
            let parsed = parse_with_errors(logs);
            if let Some(failure) = parsed.failures.first() {
                return Err(decode(
                    "Phoenix market events",
                    format!(
                        "transaction {}, instruction {index}: {}",
                        summary.signature, failure.error
                    ),
                ));
            }
            added.add_events(&parsed.events, market, summary)?;
        }
        self.fills.append(&mut added.fills);
        self.placed.append(&mut added.placed);
        self.position_changes.append(&mut added.position_changes);
        self.transactions.insert(summary.signature);
        Ok(())
    }

    fn add_events(
        &mut self,
        events: &[MarketEvent],
        market: &PhoenixMarket,
        summary: &TransactionSummary,
    ) -> Result<(), MeterError> {
        let mut header: Option<&MarketEventHeader> = None;
        // The fills since the last trade summary, with their quote lots: its
        // trader took them.
        let mut run: Vec<(usize, u64)> = Vec::new();
        let quote = |lots: i64| Decimal::new(lots, u32::from(market.quote_lot_decimals));
        let mut unix_timestamp = summary.block_time.map(|t| t.timestamp());
        let asset_id = u64::from(market.asset_id);
        for (i, event) in events.iter().enumerate() {
            let at = |what: &'static str| -> Result<(u128, &MarketEventHeader), MeterError> {
                let h = header.ok_or_else(|| decode(what, "logged before any header".into()))?;
                let index = u32::try_from(i).map_err(|_| decode(what, format!("event {i}")))?;
                Ok(((u128::from(h.sequence_number) << 32) | u128::from(index), h))
            };
            let fill = |what: &'static str,
                        maker: [u8; 32],
                        maker_side,
                        ticks: u64,
                        lots: u64,
                        quote_lots: QuoteLots,
                        rate: SignedFeeRateMicro,
                        maker_order: Option<u64>|
             -> Result<MarketFill, MeterError> {
                let (sequence, h) = at(what)?;
                let unix_timestamp = unix_timestamp
                    .filter(|&t| chrono::DateTime::from_timestamp(t, 0).is_some())
                    .ok_or_else(|| decode(what, "no time: no block time or slot context".into()))?;
                Ok(MarketFill {
                    slot: summary.slot,
                    instrument: market.id().clone(),
                    sequence,
                    taker: Pubkey::new_from_array(h.trader_account),
                    maker: Pubkey::new_from_array(maker),
                    taker_side: side(maker_side).opposite(),
                    price: market.ticks_to_price(ticks),
                    quantity: market.base_lots_to_quantity(lots_i64(what, lots)?),
                    unix_timestamp,
                    taker_fee: Decimal::ZERO,
                    maker_fee: quote(maker_fee_lots(rate, quote_lots)?),
                    maker_order: maker_order.map(u128::from),
                })
            };
            let on_market = header.is_some_and(|h| h.asset_id == market.asset_id);
            match event {
                MarketEvent::OrderFilled(_)
                | MarketEvent::SplineFilled(_)
                | MarketEvent::TradeSummary(_)
                | MarketEvent::OrderPlaced(_)
                    if header.is_none() =>
                {
                    return Err(decode(
                        "Phoenix market events",
                        format!("event {i}, a trade's, logged before any header"),
                    ));
                }
                MarketEvent::SlotContext(context) => {
                    unix_timestamp = Some(
                        i64::try_from(context.timestamp)
                            .map_err(|_| decode("slot context", context.timestamp.to_string()))?,
                    );
                }
                MarketEvent::Header(next) => {
                    header = Some(next);
                    run.clear();
                }
                MarketEvent::OrderFilled(f) if on_market => {
                    let taken = fill(
                        "an order's fill",
                        f.maker,
                        f.side,
                        f.price.as_inner(),
                        f.base_lots_filled.as_inner(),
                        f.quote_lots_filled,
                        f.maker_fee_rate,
                        Some(f.order_sequence_number),
                    )?;
                    run.push((self.fills.len(), f.quote_lots_filled.as_inner()));
                    self.fills.push(taken);
                }
                MarketEvent::SplineFilled(f) if on_market => {
                    let taken = fill(
                        "a spline's fill",
                        f.maker,
                        f.side,
                        f.price.as_inner(),
                        f.base_lots_filled.as_inner(),
                        f.quote_lots_filled,
                        f.maker_fee_rate,
                        None,
                    )?;
                    run.push((self.fills.len(), f.quote_lots_filled.as_inner()));
                    self.fills.push(taken);
                }
                MarketEvent::TradeSummary(trade) if on_market => {
                    let taker = Pubkey::new_from_array(trade.trader);
                    let shares = split(trade.fee_in_quote_lots.as_inner(), &run);
                    for (&(i, _), share) in run.iter().zip(shares) {
                        let fill = &mut self.fills[i];
                        fill.taker = taker;
                        fill.taker_fee = quote(lots_i64("a trade's fee", share)?);
                    }
                    run.clear();
                }
                MarketEvent::OrderPlaced(p) if on_market => {
                    let (sequence, h) = at("a placed order")?;
                    let lots = p.quantity.as_inner();
                    self.placed.push(PerpOrderPlaced {
                        slot: summary.slot,
                        sequence,
                        trader: Pubkey::new_from_array(h.trader_account),
                        side: if lots < 0 { Side::Sell } else { Side::Buy },
                        price: market.ticks_to_price(p.price.as_inner()),
                        quantity: market.base_lots_to_quantity(lots_i64(
                            "a placed order",
                            lots.unsigned_abs(),
                        )?),
                    });
                }
                MarketEvent::LiquidationTransfer(t) if t.asset_id == asset_id => {
                    let (sequence, _) = at("a liquidation transfer")?;
                    // The liquidatee's position, signed, moves to the
                    // liquidator.
                    let lots = t.base_lots_transferred.as_inner();
                    let quantity = market.base_lots_to_quantity(lots_i64(
                        "a liquidation transfer",
                        lots.unsigned_abs(),
                    )?);
                    let toward_long = if lots < 0 { Side::Sell } else { Side::Buy };
                    let (liquidatee, liquidator) = (
                        Pubkey::new_from_array(t.liquidatee),
                        Pubkey::new_from_array(t.liquidator),
                    );
                    for (trader, counterparty, side) in [
                        (liquidatee, liquidator, toward_long.opposite()),
                        (liquidator, liquidatee, toward_long),
                    ] {
                        self.position_changes.push(PerpPositionChange {
                            slot: summary.slot,
                            instrument: market.id().clone(),
                            sequence,
                            kind: PositionChangeKind::LiquidationTransfer,
                            trader,
                            counterparty,
                            side,
                            quantity,
                        });
                    }
                }
                MarketEvent::CloseMatchedPositions(c) if c.asset_id == asset_id => {
                    let (sequence, _) = at("an auto-deleveraging close")?;
                    let quantity = market.base_lots_to_quantity(lots_i64(
                        "an auto-deleveraging close",
                        c.base_lots_closed.as_inner().unsigned_abs(),
                    )?);
                    let (long, short) = (
                        Pubkey::new_from_array(c.closed_long),
                        Pubkey::new_from_array(c.closed_short),
                    );
                    for (trader, counterparty, side) in
                        [(long, short, Side::Sell), (short, long, Side::Buy)]
                    {
                        self.position_changes.push(PerpPositionChange {
                            slot: summary.slot,
                            instrument: market.id().clone(),
                            sequence,
                            kind: PositionChangeKind::AutoDeleverage,
                            trader,
                            counterparty,
                            side,
                            quantity,
                        });
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Whether the transaction `signature` was added.
    pub fn contains(&self, signature: &Signature) -> bool {
        self.transactions.contains(signature)
    }

    /// Puts the records in the market's own order: by slot, then sequence.
    pub fn sort(&mut self) {
        self.fills.sort_by_key(|f| (f.slot, f.sequence));
        self.placed.sort_by_key(|p| (p.slot, p.sequence));
        self.position_changes
            .sort_by_key(|c| (c.slot, c.sequence, c.trader));
    }

    /// What `accounts` did: each fill once for each side they traded, with
    /// that role's fee; the orders they placed to rest; and their position
    /// changes.
    pub fn for_traders(&self, accounts: &[Pubkey]) -> TraderRecords {
        TraderRecords {
            fills: recorded_for(&self.fills, accounts),
            placed: self
                .placed
                .iter()
                .filter(|p| accounts.contains(&p.trader))
                .cloned()
                .collect(),
            position_changes: self
                .position_changes
                .iter()
                .filter(|c| accounts.contains(&c.trader))
                .cloned()
                .collect(),
        }
    }
}

/// What a maker paid on `quote_lots` filled at `rate`, in quote lots, as
/// Phoenix charges it: a fee rounded up, a rebate (negative) toward zero.
fn maker_fee_lots(rate: SignedFeeRateMicro, quote_lots: QuoteLots) -> Result<i64, MeterError> {
    // The SDK reads the lots as an i64, so it is only exact below that.
    i64::try_from(quote_lots.as_inner())
        .ok()
        .and_then(|_| rate.apply_to_quote_lots(quote_lots))
        .map(|fee| fee.as_inner())
        .ok_or_else(|| {
            decode(
                "a maker's fee",
                format!(
                    "{} quote lots at {} millionths overflow",
                    quote_lots.as_inner(),
                    rate.as_inner()
                ),
            )
        })
}

/// `fee` quote lots split across a run of fills by their quote lots: each
/// fill its share rounded down, the last the remainder, so the shares add
/// up to the fee exactly. A run with no quote lots puts it all on the last.
fn split(fee: u64, run: &[(usize, u64)]) -> Vec<u64> {
    let total: u128 = run.iter().map(|&(_, lots)| u128::from(lots)).sum();
    let mut shares: Vec<u64> = run
        .iter()
        .map(|&(_, lots)| match total {
            0 => 0,
            // At most `fee`, so it fits.
            _ => (u128::from(fee) * u128::from(lots) / total) as u64,
        })
        .collect();
    if let Some((last, before)) = shares.split_last_mut() {
        *last = fee - before.iter().sum::<u64>();
    }
    shares
}

fn lots_i64(what: &'static str, lots: u64) -> Result<i64, MeterError> {
    i64::try_from(lots).map_err(|_| decode(what, format!("{lots} lots overflow")))
}

fn decode(what: &'static str, detail: String) -> MeterError {
    MeterError::Decode { what, detail }
}

fn side(side: phoenix_rise::math::Side) -> Side {
    match side {
        phoenix_rise::math::Side::Bid => Side::Buy,
        phoenix_rise::math::Side::Ask => Side::Sell,
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
        assert_eq!(
            split(u64::MAX, &[(0, u64::MAX), (1, 1)])
                .iter()
                .sum::<u64>(),
            u64::MAX
        );
        assert!(split(9, &[]).is_empty());
    }

    #[test]
    fn a_maker_fee_that_overflows_is_an_error_not_zero() {
        let fee = |rate, lots| maker_fee_lots(SignedFeeRateMicro::new(rate), QuoteLots::new(lots));
        for (rate, lots) in [(1_000_000, i64::MAX as u64), (-1, u64::MAX)] {
            let fee = fee(rate, lots);
            assert!(matches!(fee, Err(MeterError::Decode { .. })), "{fee:?}");
        }
        assert_eq!(fee(250, 1_000_001), Ok(251));
    }
}

#[cfg(test)]
pub(crate) mod testing {
    //! Log instructions as Phoenix writes them, for tests.

    use phoenix_rise::events::{LOG_EVENT_LENGTHS_INSTRUCTION_TAG, LOG_INSTRUCTION_TAG};

    /// The two instructions Phoenix logs the encoded events with: their
    /// lengths, then the events, as batch `batch`. Each is the data of an
    /// inner instruction of the program. Tests may corrupt an event.
    pub(crate) fn log_encoded(batch: u32, encoded: &[Vec<u8>]) -> [Vec<u8>; 2] {
        let count = (encoded.len() as u32).to_le_bytes();
        let mut lengths = LOG_EVENT_LENGTHS_INSTRUCTION_TAG.to_le_bytes().to_vec();
        lengths.extend_from_slice(&batch.to_le_bytes());
        lengths.extend_from_slice(&count);
        for e in encoded {
            lengths.extend_from_slice(&(e.len() as u16).to_le_bytes());
        }
        let mut log = LOG_INSTRUCTION_TAG.to_le_bytes().to_vec();
        log.extend_from_slice(&batch.to_le_bytes());
        log.extend_from_slice(&count);
        for e in encoded {
            log.extend_from_slice(e);
        }
        [lengths, log]
    }
}
