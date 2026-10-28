//! Phoenix Eternal access that works against an offline surfnet.
//!
//! `phoenix-rise` builds its market metadata from the Phoenix HTTP API, which an
//! offline sandbox can't reach. The metadata is fetched once, next to the
//! chain snapshot, and loaded from disk from then on.

use std::{path::Path, str::FromStr};

use phoenix_rise::{
    core::{PhoenixHawkeyeClient, PhoenixMetadata, PhoenixTxBuilder, TraderKey},
    types::exchange::{ExchangeMarketConfig, ExchangeResponse},
};
use solana_pubkey::Pubkey;

use crate::{Error, Result, error::io_error};

/// Phoenix accounts collateral in quote lots of 1e-6 USDC.
pub const QUOTE_LOTS_PER_USD: f64 = 1e6;

pub fn load_exchange(path: &Path) -> Result<ExchangeResponse> {
    let contents = std::fs::read_to_string(path).map_err(io_error(path))?;
    Ok(serde_json::from_str(&contents)?)
}

pub struct Phoenix {
    metadata: PhoenixMetadata,
    rpc_url: String,
}

impl Phoenix {
    pub fn new(exchange: ExchangeResponse, rpc_url: impl Into<String>) -> Self {
        Self {
            metadata: PhoenixMetadata::new(exchange.into()),
            rpc_url: rpc_url.into(),
        }
    }

    pub fn metadata(&self) -> &PhoenixMetadata {
        &self.metadata
    }

    pub fn market(&self, symbol: &str) -> Result<&ExchangeMarketConfig> {
        self.metadata
            .get_market(symbol)
            .ok_or_else(|| Error::UnknownMarket(symbol.to_string()))
    }

    pub fn perp_asset_map(&self) -> Result<Pubkey> {
        let key = &self.metadata.keys().perp_asset_map;
        Pubkey::from_str(key).map_err(|_| Error::InvalidExchangeKey(key.clone()))
    }

    /// The cross-margin trader account bots trade from.
    pub fn trader_account(&self, authority: &Pubkey) -> Pubkey {
        TraderKey::derive_pda(authority, 0, 0)
    }

    pub fn tx(&self) -> PhoenixTxBuilder<'_> {
        PhoenixTxBuilder::new(&self.metadata)
    }

    /// Read-only views, computed by simulating Phoenix's Hawkeye program.
    /// `fee_payer` only needs to exist; nothing is signed or charged.
    pub fn hawkeye(&self, fee_payer: Pubkey) -> PhoenixHawkeyeClient<'_> {
        PhoenixHawkeyeClient::new(self.rpc_url.clone(), &self.metadata).with_fee_payer(fee_payer)
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
