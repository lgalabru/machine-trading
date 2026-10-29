//! Phoenix Eternal access that works against an offline surfnet.
//!
//! `phoenix-rise` builds its market metadata from the Phoenix HTTP API, which
//! an offline sandbox can't reach. The metadata is fetched once, next to the
//! chain snapshot, and loaded from disk from then on.
//!
//! [`Phoenix`] wraps luce's [`PhoenixExchange`]. Its `f64` helpers are kept
//! for existing callers and keep their historical arithmetic exactly; new
//! code should prefer luce's exact conversions on [`PhoenixMarket`]
//! ([`Phoenix::luce_market`]).

use std::{path::Path, sync::Arc};

use luce::protocols::phoenix::{
    PhoenixExchange, PhoenixMarket,
    phoenix_rise::{
        core::{PhoenixHawkeyeClient, PhoenixMetadata, PhoenixTxBuilder},
        types::exchange::{ExchangeMarketConfig, ExchangeResponse},
    },
};
use solana_pubkey::Pubkey;

use crate::Result;

/// Phoenix accounts collateral in quote lots of 1e-6 USDC.
pub const QUOTE_LOTS_PER_USD: f64 = 1e6;

pub fn load_exchange(path: &Path) -> Result<ExchangeResponse> {
    Ok(luce::protocols::phoenix::load_exchange(path)?)
}

pub struct Phoenix {
    exchange: Arc<PhoenixExchange>,
    rpc_url: String,
}

impl Phoenix {
    pub fn new(exchange: ExchangeResponse, rpc_url: impl Into<String>) -> Self {
        Self {
            exchange: Arc::new(PhoenixExchange::new(exchange)),
            rpc_url: rpc_url.into(),
        }
    }

    /// The luce exchange this wraps.
    pub fn exchange(&self) -> &Arc<PhoenixExchange> {
        &self.exchange
    }

    pub fn metadata(&self) -> &PhoenixMetadata {
        self.exchange.metadata()
    }

    pub fn market(&self, symbol: &str) -> Result<&ExchangeMarketConfig> {
        Ok(self.exchange.market_config(symbol)?)
    }

    /// The market as a luce instrument, with exact `Decimal` conversions.
    pub fn luce_market(&self, symbol: &str) -> Result<PhoenixMarket> {
        Ok(self.exchange.market(symbol)?)
    }

    pub fn perp_asset_map(&self) -> Result<Pubkey> {
        Ok(self.exchange.perp_asset_map()?)
    }

    /// The cross-margin trader account bots trade from.
    pub fn trader_account(&self, authority: &Pubkey) -> Pubkey {
        self.exchange.trader_account(authority)
    }

    pub fn tx(&self) -> PhoenixTxBuilder<'_> {
        self.exchange.tx()
    }

    /// Read-only views, computed by simulating Phoenix's Hawkeye program.
    /// `fee_payer` only needs to exist; nothing is signed or charged.
    pub fn hawkeye(&self, fee_payer: Pubkey) -> PhoenixHawkeyeClient<'_> {
        self.exchange.hawkeye(&self.rpc_url, fee_payer)
    }

    /// USD per base unit for a price in ticks.
    pub fn ticks_to_usd(&self, market: &ExchangeMarketConfig, ticks: u64) -> f64 {
        ticks as f64 * market.tick_size as f64 * 10f64.powi(market.base_lots_decimals as i32 - 6)
    }

    pub fn base_units_to_lots(&self, market: &ExchangeMarketConfig, units: f64) -> u64 {
        (units * 10f64.powi(market.base_lots_decimals as i32)).round() as u64
    }

    /// USD per base unit for Phoenix's entry-price unit (quote lots per base lot).
    pub fn entry_price_to_usd(
        &self,
        market: &ExchangeMarketConfig,
        quote_lots_per_base_lot: u64,
    ) -> f64 {
        quote_lots_per_base_lot as f64 * 10f64.powi(market.base_lots_decimals as i32)
            / QUOTE_LOTS_PER_USD
    }

    pub fn lots_to_base_units(&self, market: &ExchangeMarketConfig, lots: i64) -> f64 {
        lots as f64 / 10f64.powi(market.base_lots_decimals as i32)
    }
}
