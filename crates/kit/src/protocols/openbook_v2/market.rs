//! An OpenBook v2 market as a core [`Instrument`].
//!
//! OpenBook v2 counts in lots:
//!
//! - **base lots**: `base_lot_size` base atoms;
//! - **quote lots**: `quote_lot_size` quote atoms;
//! - **price lots**: quote lots per base lot, so one price lot is
//!   `quote_lot_size × 10^base_decimals / (base_lot_size × 10^quote_decimals)`
//!   quote units per base unit.
//!
//! Both lot sizes are powers of ten, so every conversion here is exact.
//! Fees are in millionths of the notional.

use rust_decimal::Decimal;
use solana_pubkey::Pubkey;

use super::{
    MARKETS, VENUE,
    state::{LeafOrder, MarketAccount},
};
use crate::{
    core::{
        Asset, FeeSchedule, Instrument, InstrumentError, InstrumentId, InstrumentKind, OrderBook,
        PriceLevel, VenueId,
    },
    protocols::NamedMarket,
};

/// What an OpenBook instruction needs to know about its market.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MarketKeys {
    /// The market account.
    pub market: Pubkey,
    /// The PDA that signs for the vaults.
    pub authority: Pubkey,
    /// The bid side.
    pub bids: Pubkey,
    /// The ask side.
    pub asks: Pubkey,
    /// The event heap.
    pub event_heap: Pubkey,
    /// The market's base vault.
    pub base_vault: Pubkey,
    /// The market's quote vault.
    pub quote_vault: Pubkey,
    /// The base mint.
    pub base_mint: Pubkey,
    /// The quote mint.
    pub quote_mint: Pubkey,
}

/// A side of the book, for [`OpenBookV2Market::order_book`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sort {
    BestBidFirst,
    BestAskFirst,
}

/// One OpenBook v2 market and its unit conversions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenBookV2Market {
    /// The market as a core spot instrument.
    pub instrument: Instrument,
    /// The accounts its instructions use.
    pub keys: MarketKeys,
    /// The market's parameters.
    pub account: MarketAccount,
    /// Maker and taker fees as fractions of the notional.
    pub fees: FeeSchedule,
}

fn pow10(exp: i32) -> Result<Decimal, InstrumentError> {
    let out_of_range = || InstrumentError::OutOfRange {
        what: "decimals",
        value: Decimal::from(exp),
    };
    if exp >= 0 {
        10_i128
            .checked_pow(exp.unsigned_abs())
            .and_then(|p| Decimal::try_from_i128_with_scale(p, 0).ok())
            .ok_or_else(out_of_range)
    } else {
        Decimal::try_from_i128_with_scale(1, exp.unsigned_abs()).map_err(|_| out_of_range())
    }
}

impl OpenBookV2Market {
    /// The market at `address`, from its decoded account. `base_symbol` and
    /// `quote_symbol` name the assets.
    pub fn new(
        address: Pubkey,
        account: MarketAccount,
        base_symbol: &str,
        quote_symbol: &str,
    ) -> Result<Self, InstrumentError> {
        let positive = |what, v: i64| {
            if v > 0 {
                Ok(Decimal::from(v))
            } else {
                Err(InstrumentError::NonPositiveIncrement {
                    what,
                    value: Decimal::from(v),
                })
            }
        };
        let base_lot = positive("base lot size", account.base_lot_size)?;
        let quote_lot = positive("quote lot size", account.quote_lot_size)?;
        let bd = i32::from(account.base_decimals);
        let qd = i32::from(account.quote_decimals);
        let lot_size = base_lot * pow10(-bd)?;
        let tick_size = (quote_lot * pow10(bd - qd)?).checked_div(base_lot).ok_or(
            InstrumentError::OutOfRange {
                what: "tick size",
                value: quote_lot,
            },
        )?;
        let base = Asset::new(
            account.base_mint.to_string(),
            base_symbol,
            account.base_decimals,
        )
        .map_err(|_| InstrumentError::OutOfRange {
            what: "decimals",
            value: Decimal::from(bd),
        })?;
        let quote = Asset::new(
            account.quote_mint.to_string(),
            quote_symbol,
            account.quote_decimals,
        )
        .map_err(|_| InstrumentError::OutOfRange {
            what: "decimals",
            value: Decimal::from(qd),
        })?;
        let symbol = NamedMarket::symbol_of(MARKETS, &address);
        let instrument = Instrument::new(
            InstrumentId::new(format!("{VENUE}:spot:{symbol}")),
            VenueId::new(VENUE),
            symbol,
            InstrumentKind::Spot,
            base,
            quote,
            tick_size,
            lot_size,
        )?;
        let fees = FeeSchedule {
            maker_rate: Decimal::new(account.maker_fee, 6),
            taker_rate: Decimal::new(account.taker_fee, 6),
        };
        Ok(Self {
            keys: MarketKeys {
                market: address,
                authority: account.market_authority,
                bids: account.bids,
                asks: account.asks,
                event_heap: account.event_heap,
                base_vault: account.base_vault,
                quote_vault: account.quote_vault,
                base_mint: account.base_mint,
                quote_mint: account.quote_mint,
            },
            instrument,
            account,
            fees,
        })
    }

    /// The market from its account data.
    pub fn decode(
        address: Pubkey,
        data: &[u8],
        base_symbol: &str,
        quote_symbol: &str,
    ) -> Result<Self, super::OpenBookV2Error> {
        Ok(Self::new(
            address,
            MarketAccount::decode(data)?,
            base_symbol,
            quote_symbol,
        )?)
    }

    /// The instrument's reference, as a mandate names it:
    /// `openbook:spot:<SYMBOL>`, such as `openbook:spot:SOL-USDC`.
    pub fn id(&self) -> &InstrumentId {
        &self.instrument.id
    }

    /// Price for a number of price lots.
    pub fn price_lots_to_price(&self, lots: i64) -> Decimal {
        Decimal::from(lots) * self.instrument.tick_size
    }

    /// Price lots for a price, exactly. Fails if the price is off the grid.
    pub fn price_to_price_lots(&self, price: Decimal) -> Result<i64, InstrumentError> {
        let ticks = self.instrument.price_to_ticks(price)?;
        i64::try_from(ticks).map_err(|_| InstrumentError::OutOfRange {
            what: "price",
            value: price,
        })
    }

    /// Base units for a number of base lots.
    pub fn base_lots_to_quantity(&self, lots: i64) -> Decimal {
        Decimal::from(lots) * self.instrument.lot_size
    }

    /// Base lots for a quantity, exactly. Fails if the quantity is off the
    /// lot grid.
    pub fn quantity_to_base_lots(&self, quantity: Decimal) -> Result<i64, InstrumentError> {
        let lots = self.instrument.quantity_to_lots(quantity)?;
        i64::try_from(lots).map_err(|_| InstrumentError::OutOfRange {
            what: "quantity",
            value: quantity,
        })
    }

    /// The book by price level from the two sides' orders, best first.
    /// Orders expired at `unix_timestamp` are left out.
    pub fn order_book(
        &self,
        bids: &[LeafOrder],
        asks: &[LeafOrder],
        unix_timestamp: u64,
        slot: Option<u64>,
    ) -> OrderBook {
        let levels = |orders: &[LeafOrder], sort: Sort| {
            let mut by_price: std::collections::BTreeMap<i64, i64> =
                std::collections::BTreeMap::new();
            for o in orders.iter().filter(|o| o.is_live(unix_timestamp)) {
                *by_price.entry(o.price_lots).or_default() += o.base_lots;
            }
            let mut out: Vec<PriceLevel> = by_price
                .into_iter()
                .map(|(price, lots)| {
                    PriceLevel::new(
                        self.price_lots_to_price(price),
                        self.base_lots_to_quantity(lots),
                    )
                })
                .collect();
            if sort == Sort::BestBidFirst {
                out.reverse();
            }
            out
        };
        OrderBook {
            instrument: self.instrument.id.clone(),
            timestamp: chrono::Utc::now(),
            slot,
            bids: levels(bids, Sort::BestBidFirst),
            asks: levels(asks, Sort::BestAskFirst),
        }
    }
}

#[cfg(test)]
mod tests {
    use rust_decimal_macros::dec;

    use super::*;
    use crate::protocols::openbook_v2::state::testing::{book_side_bytes, market_bytes};

    fn market() -> OpenBookV2Market {
        let keys: [Pubkey; 8] = std::array::from_fn(|i| Pubkey::new_from_array([i as u8 + 1; 32]));
        OpenBookV2Market::decode(Pubkey::new_unique(), &market_bytes(&keys), "SOL", "USDC").unwrap()
    }

    #[test]
    fn units_are_exact() {
        let m = market();
        assert_eq!(m.instrument.kind, InstrumentKind::Spot);
        assert_eq!(m.instrument.lot_size, dec!(0.001));
        assert_eq!(m.instrument.tick_size, dec!(0.001));
        assert_eq!(m.id().0, format!("openbook:spot:{}", m.keys.market));
        let keys: [Pubkey; 8] = std::array::from_fn(|i| Pubkey::new_from_array([i as u8 + 1; 32]));
        let named =
            OpenBookV2Market::decode(MARKETS[0].address, &market_bytes(&keys), "SOL", "USDC")
                .unwrap();
        assert_eq!(named.id().0, "openbook:spot:SOL-USDC");
        assert_eq!(named.instrument.symbol, "SOL-USDC");
        assert_eq!(m.fees.taker_rate, dec!(0.001));
        assert_eq!(m.price_lots_to_price(150_025), dec!(150.025));
        assert_eq!(m.price_to_price_lots(dec!(150.025)).unwrap(), 150_025);
        assert!(m.price_to_price_lots(dec!(150.0251)).is_err());
        assert_eq!(m.quantity_to_base_lots(dec!(2.5)).unwrap(), 2_500);
        assert_eq!(m.base_lots_to_quantity(2_500), dec!(2.5));
    }

    #[test]
    fn levels_aggregate_best_first() {
        let m = market();
        let owner = Pubkey::new_unique();
        let side = |leaves: &[_]| {
            crate::protocols::openbook_v2::state::book_side_orders(&book_side_bytes(leaves))
                .unwrap()
        };
        let bids = side(&[
            (149_990, 1, 1_000, owner, 0),
            (150_000, 2, 500, owner, 0),
            (150_000, 3, 500, owner, 0),
        ]);
        let asks = side(&[(150_020, 4, 2_000, owner, 0)]);
        let book = m.order_book(&bids, &asks, 0, Some(7));
        assert_eq!(book.bids[0], PriceLevel::new(dec!(150), dec!(1)));
        assert_eq!(book.bids[1].price, dec!(149.99));
        assert_eq!(book.best_ask().unwrap().quantity, Some(dec!(2)));
        assert_eq!(book.slot, Some(7));
    }
}
