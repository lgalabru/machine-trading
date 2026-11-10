//! A Phoenix v1 market as a core [`Instrument`].
//!
//! Phoenix v1 counts in integers:
//!
//! - **base lots**: `base_lot_size` base atoms (`10^-base_decimals` base
//!   units each), so `base_lots_per_base_unit` lots make one unit;
//! - **quote lots**: `quote_lot_size` quote atoms;
//! - **ticks**: one tick is `tick_size_in_quote_atoms_per_base_unit` quote
//!   atoms per base unit.
//!
//! Every conversion here is exact: lot and tick sizes are whole numbers of
//! atoms, and atoms are powers of ten of a unit.

use rust_decimal::Decimal;
use solana_pubkey::Pubkey;

use super::{
    MARKETS, VENUE,
    ix::MarketKeys,
    state::{BookSide, MarketHeader, MarketState},
};
use crate::{
    core::{
        Asset, FeeSchedule, Instrument, InstrumentError, InstrumentId, InstrumentKind, OrderBook,
        PriceLevel, VenueId,
    },
    protocols::NamedMarket,
};

/// One order on the book with its owner: what a reader needs to tell who
/// shows size, and whether an order rests or comes back new.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedOrder {
    /// The book side.
    pub side: BookSide,
    /// Price, quote units per base unit.
    pub price: Decimal,
    /// What is left, in base units.
    pub quantity: Decimal,
    /// The trader whose seat placed it, when the seat tree names one.
    pub owner: Option<Pubkey>,
    /// Its sequence number: placing an order gives it a new one.
    pub sequence_number: u64,
    /// The last slot it is valid in (0: no expiry).
    pub last_valid_slot: u64,
}

/// One Phoenix v1 market and its unit conversions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhoenixV1Market {
    /// The market as a core spot instrument.
    pub instrument: Instrument,
    /// The accounts its instructions use.
    pub keys: MarketKeys,
    /// The market's parameters.
    pub header: MarketHeader,
    /// Taker and maker fees (Phoenix v1 charges takers only).
    pub fees: FeeSchedule,
}

/// `atoms × 10^-decimals`, exactly.
fn atoms(atoms: u64, decimals: u32) -> Result<Decimal, InstrumentError> {
    Decimal::try_from_i128_with_scale(i128::from(atoms), decimals).map_err(|_| {
        InstrumentError::OutOfRange {
            what: "atoms",
            value: Decimal::from(atoms),
        }
    })
}

impl PhoenixV1Market {
    /// The market at `address` with header `header`. `base_symbol` and
    /// `quote_symbol` name the assets (mints and decimals come from the
    /// header). The instrument's symbol is the profile's for a market in
    /// [`MARKETS`], and the address for any other.
    pub fn new(
        address: Pubkey,
        header: MarketHeader,
        base_symbol: &str,
        quote_symbol: &str,
    ) -> Result<Self, InstrumentError> {
        let asset = |mint: &Pubkey, symbol: &str, decimals: u32| {
            Asset::new(
                mint.to_string(),
                symbol,
                u8::try_from(decimals).unwrap_or(u8::MAX),
            )
            .map_err(|_| InstrumentError::OutOfRange {
                what: "decimals",
                value: Decimal::from(decimals),
            })
        };
        let base = asset(&header.base_mint, base_symbol, header.base_decimals)?;
        let quote = asset(&header.quote_mint, quote_symbol, header.quote_decimals)?;
        let symbol = NamedMarket::symbol_of(MARKETS, &address);
        let instrument = Instrument::new(
            InstrumentId::new(format!("{VENUE}:spot:{symbol}")),
            VenueId::new(VENUE),
            symbol,
            InstrumentKind::Spot,
            base,
            quote,
            atoms(
                header.tick_size_in_quote_atoms_per_base_unit,
                header.quote_decimals,
            )?,
            atoms(header.base_lot_size, header.base_decimals)?,
        )?;
        let fees = FeeSchedule {
            maker_rate: Decimal::ZERO,
            taker_rate: Decimal::new(i64::try_from(header.taker_fee_bps).unwrap_or(i64::MAX), 4),
        };
        Ok(Self {
            keys: MarketKeys {
                market: address,
                base_mint: header.base_mint,
                quote_mint: header.quote_mint,
                base_vault: header.base_vault,
                quote_vault: header.quote_vault,
            },
            instrument,
            header,
            fees,
        })
    }

    /// The market from its account data.
    pub fn decode(
        address: Pubkey,
        data: &[u8],
        base_symbol: &str,
        quote_symbol: &str,
    ) -> Result<Self, super::PhoenixV1Error> {
        Ok(Self::new(
            address,
            MarketHeader::decode(data)?,
            base_symbol,
            quote_symbol,
        )?)
    }

    /// The instrument's reference, as a mandate names it:
    /// `phoenix:spot:<SYMBOL>`, such as `phoenix:spot:SOL-USDC`.
    pub fn id(&self) -> &InstrumentId {
        &self.instrument.id
    }

    /// Price for a number of ticks.
    pub fn ticks_to_price(&self, ticks: u64) -> Decimal {
        self.instrument.ticks_to_price(ticks)
    }

    /// Ticks for a price, exactly. Fails if the price is off the tick grid.
    pub fn price_to_ticks(&self, price: Decimal) -> Result<u64, InstrumentError> {
        self.instrument.price_to_ticks(price)
    }

    /// Base units for a number of base lots.
    pub fn base_lots_to_quantity(&self, lots: u64) -> Decimal {
        self.instrument.lots_to_quantity(lots)
    }

    /// Base lots for a quantity, exactly. Fails if the quantity is off the
    /// lot grid.
    pub fn quantity_to_base_lots(&self, quantity: Decimal) -> Result<u64, InstrumentError> {
        self.instrument.quantity_to_lots(quantity)
    }

    /// Quote units for a number of quote lots.
    pub fn quote_lots_to_amount(&self, lots: u64) -> Decimal {
        Decimal::from(lots)
            * atoms(self.header.quote_lot_size, self.header.quote_decimals)
                .expect("validated with the instrument")
    }

    /// Every resting order, best first on each side, with its owner. Orders
    /// expired at `slot` and `unix_timestamp` are left out.
    pub fn owned_orders(
        &self,
        state: &MarketState,
        slot: u64,
        unix_timestamp: u64,
    ) -> Vec<OwnedOrder> {
        state
            .bids
            .iter()
            .chain(&state.asks)
            .filter(|o| o.is_live(slot, unix_timestamp))
            .map(|o| OwnedOrder {
                side: o.side,
                price: self.ticks_to_price(o.price_in_ticks),
                quantity: self.base_lots_to_quantity(o.base_lots),
                owner: state.trader_at(o.trader_index),
                sequence_number: o.sequence_number,
                last_valid_slot: o.last_valid_slot,
            })
            .collect()
    }

    /// The book by price level, with sizes, best first. Orders expired at
    /// `slot` and `unix_timestamp` are left out.
    pub fn order_book(&self, state: &MarketState, slot: u64, unix_timestamp: u64) -> OrderBook {
        let levels = |side: BookSide| {
            let mut out: Vec<(u64, u64)> = Vec::new();
            for o in state
                .orders(side)
                .iter()
                .filter(|o| o.is_live(slot, unix_timestamp))
            {
                match out.last_mut() {
                    Some(level) if level.0 == o.price_in_ticks => level.1 += o.base_lots,
                    _ => out.push((o.price_in_ticks, o.base_lots)),
                }
            }
            out.into_iter()
                .map(|(ticks, lots)| {
                    PriceLevel::new(self.ticks_to_price(ticks), self.base_lots_to_quantity(lots))
                })
                .collect()
        };
        OrderBook {
            instrument: self.instrument.id.clone(),
            timestamp: chrono::Utc::now(),
            slot: Some(slot),
            bids: levels(BookSide::Bid),
            asks: levels(BookSide::Ask),
        }
    }
}

#[cfg(test)]
mod tests {
    use rust_decimal_macros::dec;

    use super::*;
    use crate::protocols::phoenix::v1::state::testing::{MarketSpec, market_bytes};

    #[test]
    fn units_are_exact() {
        let data = market_bytes(&MarketSpec::default());
        let m = PhoenixV1Market::decode(MARKETS[0].address, &data, "SOL", "USDC").unwrap();
        assert_eq!(m.instrument.kind, InstrumentKind::Spot);
        assert_eq!(m.instrument.tick_size, dec!(0.001));
        assert_eq!(m.instrument.lot_size, dec!(0.001));
        assert_eq!(m.id().0, "phoenix:spot:SOL-USDC");
        assert_eq!(m.instrument.symbol, "SOL-USDC");
        assert_eq!(m.instrument.base.symbol, "SOL");
        assert_eq!(m.fees.taker_rate, dec!(0.0002));
        assert_eq!(m.ticks_to_price(150_025), dec!(150.025));
        assert_eq!(m.price_to_ticks(dec!(150.025)).unwrap(), 150_025);
        assert!(m.price_to_ticks(dec!(150.0255)).is_err());
        assert_eq!(m.base_lots_to_quantity(2_500), dec!(2.5));
        assert_eq!(m.quantity_to_base_lots(dec!(2.5)).unwrap(), 2_500);
        assert_eq!(m.quote_lots_to_amount(1_500_000), dec!(1.5));
    }

    #[test]
    fn markets_the_profile_doesnt_name_go_by_address() {
        let data = market_bytes(&MarketSpec::default());
        let address = Pubkey::new_unique();
        let m = PhoenixV1Market::decode(address, &data, "SOL", "USDC").unwrap();
        assert_eq!(m.id().0, format!("phoenix:spot:{address}"));
        assert_eq!(
            NamedMarket::find(MARKETS, "SOL-USDC").map(|m| m.address),
            Some(MARKETS[0].address)
        );
        assert!(NamedMarket::find(MARKETS, &MARKETS[0].address.to_string()).is_some());
        assert!(NamedMarket::find(MARKETS, "SOL/USDC").is_none());
    }

    #[test]
    fn books_carry_sizes_and_owners() {
        let maker = Pubkey::new_from_array([8; 32]);
        let data = market_bytes(&MarketSpec {
            bids: vec![
                (150_000, 7, 1, 2_000),
                (150_000, 9, 1, 500),
                (149_990, 3, 1, 1_000),
            ],
            asks: vec![(150_020, 5, 1, 1_000)],
            seats: vec![(maker, 0, 1_000, 0, 375_000)],
            ..MarketSpec::default()
        });
        let state = MarketState::decode(&data).unwrap();
        let m = PhoenixV1Market::decode(Pubkey::new_unique(), &data, "SOL", "USDC").unwrap();
        let book = m.order_book(&state, 1, 1);
        assert_eq!(book.bids.len(), 2);
        assert_eq!(book.bids[0], PriceLevel::new(dec!(150), dec!(2.5)));
        assert_eq!(book.best_ask().unwrap().price, dec!(150.02));
        assert_eq!(book.bid_depth(), Some(dec!(3.5)));
        let orders = m.owned_orders(&state, 1, 1);
        assert_eq!(orders.len(), 4);
        assert_eq!(orders[0].owner, Some(maker));
        assert_eq!(orders[1].sequence_number, 9);
        assert_eq!(orders[3].side, BookSide::Ask);
    }
}
