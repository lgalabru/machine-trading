//! What a strategy reads of Phoenix without the chain (feature `venue`): the
//! market metadata the session hands it ([`Session::venue`](crate::Session)),
//! the conversions between Phoenix's integer units and USD, the accounts a
//! strategy reads through [`rpc_account`](crate::rpc_account), and
//! Phoenix's own decoding of them ([`phoenix_rise::accounts`]). It builds
//! for `wasm32-wasip2`, so a component can use it.

pub use phoenix_rise;
use phoenix_rise::types::exchange::{ExchangeMarketConfig, ExchangeResponse};
use solana_pubkey::Pubkey;

/// USD lots per USD: Phoenix's quote lots are micro-USDC.
pub const QUOTE_LOTS_PER_USD: f64 = 1e6;

/// Phoenix's market metadata, as its `/exchange` response holds it.
#[derive(Debug, Clone)]
pub struct PhoenixVenue {
    exchange: ExchangeResponse,
}

impl PhoenixVenue {
    /// The venue from the JSON a session hands a strategy.
    pub fn from_json(json: &str) -> Result<Self, String> {
        serde_json::from_str(json)
            .map(|exchange| Self { exchange })
            .map_err(|e| format!("the venue's metadata: {e}"))
    }

    /// The venue from an `/exchange` response already parsed.
    pub fn from_exchange(exchange: ExchangeResponse) -> Self {
        Self { exchange }
    }

    /// The whole `/exchange` response.
    pub fn exchange(&self) -> &ExchangeResponse {
        &self.exchange
    }

    /// A market's configuration, by symbol (`SPCX`).
    pub fn market(&self, symbol: &str) -> Result<&ExchangeMarketConfig, String> {
        self.exchange
            .markets
            .iter()
            .find(|m| m.symbol == symbol)
            .ok_or_else(|| format!("no market {symbol}"))
    }

    /// The perp asset map, which holds every market's oracle and mark.
    pub fn perp_asset_map(&self) -> Result<Pubkey, String> {
        key(&self.exchange.keys.perp_asset_map)
    }

    /// The cross-margin trader account (PDA index 0, subaccount 0) of
    /// `authority`.
    pub fn trader_account(&self, authority: &Pubkey) -> Pubkey {
        Pubkey::find_program_address(
            &[b"trader", authority.as_ref(), &[0, 0]],
            &phoenix_rise::ix::PHOENIX_PROGRAM_ID,
        )
        .0
    }

    /// USD per base unit for a price in ticks: [`ticks_to_usd`].
    pub fn ticks_to_usd(&self, market: &ExchangeMarketConfig, ticks: u64) -> f64 {
        ticks_to_usd(market, ticks)
    }

    /// [`lots_to_base_units`].
    pub fn lots_to_base_units(&self, market: &ExchangeMarketConfig, lots: i64) -> f64 {
        lots_to_base_units(market, lots)
    }

    /// [`base_units_to_lots`].
    pub fn base_units_to_lots(&self, market: &ExchangeMarketConfig, units: f64) -> u64 {
        base_units_to_lots(market, units)
    }
}

/// A pubkey from Phoenix's metadata.
pub fn key(text: &str) -> Result<Pubkey, String> {
    text.parse().map_err(|_| format!("{text} is not a pubkey"))
}

/// USD per base unit for a price in ticks.
pub fn ticks_to_usd(market: &ExchangeMarketConfig, ticks: u64) -> f64 {
    ticks as f64 * market.tick_size as f64 * 10f64.powi(market.base_lots_decimals as i32 - 6)
}

/// Base units for a signed number of base lots.
pub fn lots_to_base_units(market: &ExchangeMarketConfig, lots: i64) -> f64 {
    lots as f64 / 10f64.powi(market.base_lots_decimals as i32)
}

/// Base lots for base units, rounded.
pub fn base_units_to_lots(market: &ExchangeMarketConfig, units: f64) -> u64 {
    (units * 10f64.powi(market.base_lots_decimals as i32)).round() as u64
}

/// USD per base unit for Phoenix's entry-price unit (quote lots per base lot).
pub fn entry_price_to_usd(market: &ExchangeMarketConfig, quote_lots_per_base_lot: u64) -> f64 {
    quote_lots_per_base_lot as f64 * 10f64.powi(market.base_lots_decimals as i32)
        / QUOTE_LOTS_PER_USD
}

/// A perp's prices, in USD, as the perp asset map account holds them:
/// what Phoenix's own view of the market reports, read without a
/// simulation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PerpPrices {
    /// The mark Phoenix last computed: its blend of the oracles and the
    /// book.
    pub mark: f64,
    /// The median of the latest prices the market's spot oracles reported,
    /// or its perp oracles' where no spot oracle has: quotes can't push it.
    pub oracle: f64,
}

/// `market`'s prices, from the perp asset map account's bytes.
pub fn perp_prices(market: &ExchangeMarketConfig, asset_map: &[u8]) -> Result<PerpPrices, String> {
    use phoenix_rise::accounts::perp_asset_map::PerpAssetMap;
    let map = PerpAssetMap::try_from_account_bytes(asset_map).map_err(|e| e.to_string())?;
    let entry = map
        .find_by_symbol(&market.symbol)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("no market {} in the perp asset map", market.symbol))?;
    let price = &entry.metadata.oracle_price().mark_price;
    // An oracle that never reported holds 0, and one that was reset the
    // largest value: neither is a price.
    let reported = |ticks: u64| ticks > 0 && ticks != u64::MAX;
    let mut ticks: Vec<u64> = price
        .spot_price_component
        .last_exchange_spot_price
        .iter()
        .map(|p| p.ticks.as_inner())
        .filter(|&t| reported(t))
        .collect();
    if ticks.is_empty() {
        ticks = price
            .perp_price_component
            .last_exchange_perp_price
            .iter()
            .map(|p| p.ticks.as_inner())
            .filter(|&t| reported(t))
            .collect();
    }
    if ticks.is_empty() {
        return Err(format!("no oracle reports for {}", market.symbol));
    }
    ticks.sort_unstable();
    let median = ticks[ticks.len() / 2];
    Ok(PerpPrices {
        mark: ticks_to_usd(market, price.price.ticks.as_inner()),
        oracle: ticks_to_usd(market, median),
    })
}

/// The position in `market` a trader account holds, from the account's
/// bytes: flat if it holds none. The entry is Phoenix's own: the position's
/// quote lots per base lot, rounded down to a whole number.
pub fn perp_position(
    market: &ExchangeMarketConfig,
    trader_account: &[u8],
) -> Result<crate::Position, String> {
    use phoenix_rise::accounts::trader::Trader;
    let trader = Trader::try_from_account_bytes(trader_account).map_err(|e| e.to_string())?;
    let Some((_, position)) = trader
        .positions()
        .find(|(asset, _)| *asset == u64::from(market.asset_id))
    else {
        return Ok(crate::Position {
            size: 0.0,
            entry_usd: 0.0,
        });
    };
    let position = position.to_position();
    let base_lots = position.base_lot_position.as_inner();
    if base_lots == 0 {
        return Ok(crate::Position {
            size: 0.0,
            entry_usd: 0.0,
        });
    }
    let quote_lots = position.virtual_quote_lot_position.as_inner();
    let quote_lots_per_base_lot = quote_lots.unsigned_abs() / base_lots.unsigned_abs();
    Ok(crate::Position {
        size: lots_to_base_units(market, base_lots),
        entry_usd: entry_price_to_usd(market, quote_lots_per_base_lot),
    })
}
