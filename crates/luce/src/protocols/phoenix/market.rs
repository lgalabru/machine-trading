//! A Phoenix perpetual market as a core [`Instrument`].
//!
//! Phoenix counts in integers:
//!
//! - **base lots**: `10^-base_lots_decimals` base units (0.01 SOL when the
//!   market's `base_lots_decimals` is 2);
//! - **quote lots**: `10^-quote_lot_decimals` USDC (1e-6, one micro-USDC);
//! - **ticks**: `tick_size` quote lots per base lot.
//!
//! So one tick is `tick_size × 10^(base_lots_decimals − quote_lot_decimals)`
//! USDC per base unit, and an entry price of `q` quote lots per base lot is
//! `q × 10^(base_lots_decimals − quote_lot_decimals)` USDC per base unit.
//! Every conversion here is exact: powers of ten are scale changes in
//! [`Decimal`], never floating point.

use rust_decimal::Decimal;

use super::{PhoenixError, PhoenixExchange, VENUE};
use crate::core::{
    Asset, FeeSchedule, Instrument, InstrumentId, InstrumentKind, OrderBook, Position, PriceLevel,
    VenueId,
};

/// Decimals of Phoenix's quote lots when the metadata does not say.
pub const DEFAULT_QUOTE_LOT_DECIMALS: u8 = 6;

/// One Phoenix market and its unit conversions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhoenixMarket {
    /// The market as a core instrument: base and quote assets, tick and lot
    /// sizes in decimal units.
    pub instrument: Instrument,
    /// Phoenix's asset index for the market, used by margin views.
    pub asset_id: u32,
    /// Raw tick size, in quote lots per base lot.
    pub tick_size_quote_lots: u64,
    /// Base lot decimals (may be negative for lots larger than one unit).
    pub base_lots_decimals: i8,
    /// Quote lot decimals.
    pub quote_lot_decimals: u8,
    /// Taker and maker fees.
    pub fees: FeeSchedule,
}

/// `10^exp` as an exact [`Decimal`], for `exp` in `-28..=28`.
fn pow10(exp: i32) -> Option<Decimal> {
    if exp >= 0 {
        10_i128
            .checked_pow(exp.unsigned_abs())
            .and_then(|p| Decimal::try_from_i128_with_scale(p, 0).ok())
    } else {
        Decimal::try_from_i128_with_scale(1, exp.unsigned_abs()).ok()
    }
}

/// An `f64` fee from the metadata as the decimal it was written as.
///
/// The `/exchange` JSON carries fees as short decimal literals (`0.00035`);
/// serde parses them into the nearest `f64`, whose shortest representation
/// is the original literal. Going back through that representation recovers
/// it exactly.
fn fee_decimal(what: &'static str, fee: f64) -> Result<Decimal, PhoenixError> {
    let text = format!("{fee}");
    text.parse::<Decimal>()
        .or_else(|_| Decimal::from_scientific(&text))
        .map_err(|_| PhoenixError::InvalidExchangeValue { what, value: text })
}

impl PhoenixMarket {
    /// The market `config` describes, in `exchange`.
    pub fn new(
        exchange: &PhoenixExchange,
        config: &phoenix_rise::types::exchange::ExchangeMarketConfig,
    ) -> Result<Self, PhoenixError> {
        let quote_lot_decimals = exchange
            .metadata()
            .get_market_calculator(&config.symbol)
            .map_or(DEFAULT_QUOTE_LOT_DECIMALS, |c| c.quote_lot_decimals);
        let quote = Asset::new(
            exchange.metadata().keys().canonical_mint.clone(),
            "USDC",
            quote_lot_decimals,
        )
        .map_err(|_| PhoenixError::InvalidExchangeValue {
            what: "quote lot decimals",
            value: quote_lot_decimals.to_string(),
        })?;
        let fees = FeeSchedule {
            maker_rate: fee_decimal("maker fee", config.maker_fee)?,
            taker_rate: fee_decimal("taker fee", config.taker_fee)?,
        };
        Self::from_parts(
            &config.symbol,
            config.asset_id,
            config.tick_size,
            config.base_lots_decimals,
            quote,
            fees,
        )
    }

    /// A market from its raw parameters. `quote` is the collateral asset;
    /// its decimals are the quote lot decimals.
    pub fn from_parts(
        symbol: &str,
        asset_id: u32,
        tick_size_quote_lots: u64,
        base_lots_decimals: i8,
        quote: Asset,
        fees: FeeSchedule,
    ) -> Result<Self, PhoenixError> {
        let quote_lot_decimals = quote.decimals;
        let invalid = |what, value: String| PhoenixError::InvalidExchangeValue { what, value };
        let tick_unit = pow10(i32::from(base_lots_decimals) - i32::from(quote_lot_decimals))
            .ok_or_else(|| invalid("base lots decimals", base_lots_decimals.to_string()))?;
        let lot_size = pow10(-i32::from(base_lots_decimals))
            .ok_or_else(|| invalid("base lots decimals", base_lots_decimals.to_string()))?;
        let base = Asset::new(
            format!("{VENUE}:{symbol}"),
            symbol,
            base_lots_decimals.max(0).unsigned_abs(),
        )
        .map_err(|_| invalid("base lots decimals", base_lots_decimals.to_string()))?;
        let instrument = Instrument::new(
            InstrumentId::new(format!("{VENUE}:{symbol}")),
            VenueId::new(VENUE),
            symbol,
            InstrumentKind::Perpetual,
            base,
            quote,
            Decimal::from(tick_size_quote_lots) * tick_unit,
            lot_size,
        )?;
        Ok(Self {
            instrument,
            asset_id,
            tick_size_quote_lots,
            base_lots_decimals,
            quote_lot_decimals,
            fees,
        })
    }

    /// The market's symbol, e.g. `"SOL"`.
    pub fn symbol(&self) -> &str {
        &self.instrument.symbol
    }

    /// The instrument's identifier, `"phoenix:{symbol}"`.
    pub fn id(&self) -> &InstrumentId {
        &self.instrument.id
    }

    /// USDC per base unit for a price in ticks.
    pub fn ticks_to_price(&self, ticks: u64) -> Decimal {
        self.instrument.ticks_to_price(ticks)
    }

    /// Ticks for a price, exactly. Fails if the price is off the tick grid.
    pub fn price_to_ticks(&self, price: Decimal) -> Result<u64, PhoenixError> {
        Ok(self.instrument.price_to_ticks(price)?)
    }

    /// Base units for a signed number of base lots (Phoenix positions).
    pub fn base_lots_to_quantity(&self, lots: i64) -> Decimal {
        Decimal::from(lots) * self.instrument.lot_size
    }

    /// Base lots for a quantity, exactly. Fails if the quantity is off the
    /// lot grid or negative.
    pub fn quantity_to_base_lots(&self, quantity: Decimal) -> Result<u64, PhoenixError> {
        Ok(self.instrument.quantity_to_lots(quantity)?)
    }

    /// USDC per base unit for Phoenix's entry-price unit (quote lots per
    /// base lot). Entry prices are averages and need not sit on the tick grid.
    pub fn entry_price(&self, quote_lots_per_base_lot: u64) -> Decimal {
        let unit = pow10(i32::from(self.base_lots_decimals) - i32::from(self.quote_lot_decimals))
            .expect("validated in from_parts");
        Decimal::from(quote_lots_per_base_lot) * unit
    }

    /// A position as Phoenix reports it, in core terms.
    pub fn position(&self, base_lots: i64, entry_quote_lots_per_base_lot: u64) -> Position {
        Position::from_entry(
            self.instrument.id.clone(),
            self.instrument.venue.clone(),
            self.base_lots_to_quantity(base_lots),
            self.entry_price(entry_quote_lots_per_base_lot),
        )
    }

    /// A top-of-book snapshot from Hawkeye's best bid and offer.
    ///
    /// Hawkeye reports prices only, so level sizes are unknown (`None`).
    pub fn book_from_bbo(
        &self,
        bbo: &phoenix_rise::ix::hawkeye::ViewBboReturn,
        slot: Option<u64>,
    ) -> OrderBook {
        let level = |ticks: Option<u64>| {
            ticks
                .map(|t| PriceLevel::price_only(self.ticks_to_price(t)))
                .into_iter()
                .collect()
        };
        OrderBook {
            instrument: self.instrument.id.clone(),
            timestamp: chrono::Utc::now(),
            slot,
            bids: level(bbo.best_bid_ticks()),
            asks: level(bbo.best_ask_ticks()),
        }
    }
}

#[cfg(test)]
mod tests {
    use rust_decimal_macros::dec;

    use super::*;

    fn usdc() -> Asset {
        Asset::new("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v", "USDC", 6).unwrap()
    }

    fn sol() -> PhoenixMarket {
        PhoenixMarket::from_parts("SOL", 0, 100, 2, usdc(), FeeSchedule::ZERO).unwrap()
    }

    #[test]
    fn sol_market_units() {
        let m = sol();
        assert_eq!(m.instrument.tick_size, dec!(0.01));
        assert_eq!(m.instrument.lot_size, dec!(0.01));
        assert_eq!(m.instrument.kind, InstrumentKind::Perpetual);
        assert_eq!(m.id().0, "phoenix:SOL");
        assert_eq!(m.instrument.base.decimals, 2);
        assert_eq!(m.instrument.quote.symbol, "USDC");
        assert_eq!(m.ticks_to_price(15_025), dec!(150.25));
        assert_eq!(m.price_to_ticks(dec!(150.25)).unwrap(), 15_025);
        assert!(m.price_to_ticks(dec!(150.255)).is_err());
        assert_eq!(m.base_lots_to_quantity(-250), dec!(-2.5));
        assert_eq!(m.quantity_to_base_lots(dec!(2.5)).unwrap(), 250);
        assert!(m.quantity_to_base_lots(dec!(0.001)).is_err());
        // 1_502_500 micro-USDC per base lot of 0.01 SOL = 150.25 USDC per SOL.
        assert_eq!(m.entry_price(1_502_500), dec!(150.25));
        // Entry prices are averages: off-tick values are fine.
        assert_eq!(m.entry_price(1_502_501), dec!(150.2501));
        assert_eq!(m.entry_price(1), dec!(0.0001));
    }

    #[test]
    fn matches_the_float_formulas_it_replaces() {
        // botkit's f64 conversions: ticks × tick_size × 10^(bld − 6), and
        // lots / 10^bld.
        let m = sol();
        for ticks in [1_u64, 99, 15_025, 1_234_567] {
            let float = ticks as f64 * 100.0 * 10f64.powi(2 - 6);
            let exact =
                rust_decimal::prelude::ToPrimitive::to_f64(&m.ticks_to_price(ticks)).unwrap();
            assert!((float - exact).abs() < 1e-9, "{ticks}: {float} vs {exact}");
        }
    }

    #[test]
    fn negative_lot_decimals_mean_large_lots() {
        // A memecoin market whose lot is 1000 units and tick 1 quote lot.
        let m = PhoenixMarket::from_parts("BONK", 7, 1, -3, usdc(), FeeSchedule::ZERO).unwrap();
        assert_eq!(m.instrument.lot_size, dec!(1000));
        assert_eq!(m.instrument.tick_size, dec!(0.000000001));
        assert_eq!(m.instrument.base.decimals, 0);
        assert_eq!(m.quantity_to_base_lots(dec!(5000)).unwrap(), 5);
        assert_eq!(m.base_lots_to_quantity(3), dec!(3000));
    }

    #[test]
    fn positions_come_out_in_core_terms() {
        let p = sol().position(-150, 1_400_000);
        assert_eq!(p.quantity, dec!(-1.5));
        assert_eq!(p.average_entry_price(), Some(dec!(140)));
        assert_eq!(p.unrealized_pnl(dec!(130)), dec!(15));
    }

    #[test]
    fn fees_parse_exactly() {
        assert_eq!(fee_decimal("taker fee", 0.00035).unwrap(), dec!(0.00035));
        assert_eq!(fee_decimal("maker fee", 5e-05).unwrap(), dec!(0.00005));
        assert_eq!(fee_decimal("maker fee", -0.0001).unwrap(), dec!(-0.0001));
        assert!(fee_decimal("maker fee", f64::NAN).is_err());
    }

    #[test]
    fn snapshot_metadata_loads() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../snapshots/phoenix-sol-2026-10-27/exchange.json");
        if !path.exists() {
            eprintln!("skipping: {} not found", path.display());
            return;
        }
        let exchange = PhoenixExchange::load(&path).unwrap();
        let market = exchange.market("SOL").unwrap();
        assert_eq!(market.instrument.tick_size, dec!(0.01));
        assert_eq!(market.instrument.lot_size, dec!(0.01));
        assert_eq!(market.fees.taker_rate, dec!(0.00035));
        assert_eq!(market.fees.maker_rate, dec!(0.00005));
        assert_eq!(
            market.instrument.quote.id.0,
            exchange.collateral_mint().unwrap().to_string()
        );
        assert!(exchange.market("NOPE").is_err());
    }
}
