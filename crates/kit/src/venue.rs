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
