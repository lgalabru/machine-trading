//! Exchange-wide metadata: markets, program accounts, builders.
//!
//! `phoenix-rise` builds its metadata from Phoenix's HTTP `/exchange`
//! endpoint. Offline sandboxes cannot reach it, so the response is saved
//! once, next to the chain state it matches, and loaded from disk with
//! [`load_exchange`].

use std::{path::Path, str::FromStr};

use phoenix_rise::{
    core::{PhoenixHawkeyeClient, PhoenixMetadata, PhoenixTxBuilder, TraderKey},
    types::exchange::{ExchangeMarketConfig, ExchangeResponse},
};
use solana_pubkey::Pubkey;

use super::{PhoenixError, PhoenixMarket};

/// Reads a saved `/exchange` response.
pub fn load_exchange(path: &Path) -> Result<ExchangeResponse, PhoenixError> {
    let contents = std::fs::read_to_string(path).map_err(|source| PhoenixError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(serde_json::from_str(&contents)?)
}

/// Phoenix exchange metadata and what can be derived from it without I/O.
#[derive(Debug, Clone)]
pub struct PhoenixExchange {
    metadata: PhoenixMetadata,
}

impl PhoenixExchange {
    /// Wraps an `/exchange` response.
    pub fn new(exchange: ExchangeResponse) -> Self {
        Self {
            metadata: PhoenixMetadata::new(exchange.into()),
        }
    }

    /// Loads a saved `/exchange` response from disk.
    pub fn load(path: &Path) -> Result<Self, PhoenixError> {
        Ok(Self::new(load_exchange(path)?))
    }

    /// The SDK's metadata, for builders this crate does not wrap.
    pub fn metadata(&self) -> &PhoenixMetadata {
        &self.metadata
    }

    /// A market's raw configuration.
    pub fn market_config(&self, symbol: &str) -> Result<&ExchangeMarketConfig, PhoenixError> {
        self.metadata
            .get_market(symbol)
            .ok_or_else(|| PhoenixError::UnknownMarket(symbol.to_string()))
    }

    /// A market as a core [`Instrument`](crate::core::Instrument), with
    /// exact conversions between its integer units and decimals.
    pub fn market(&self, symbol: &str) -> Result<PhoenixMarket, PhoenixError> {
        PhoenixMarket::new(self, self.market_config(symbol)?)
    }

    /// The perp asset map account, which every trading instruction reads.
    pub fn perp_asset_map(&self) -> Result<Pubkey, PhoenixError> {
        parse_key(&self.metadata.keys().perp_asset_map)
    }

    /// The collateral mint (USDC).
    pub fn collateral_mint(&self) -> Result<Pubkey, PhoenixError> {
        parse_key(&self.metadata.keys().canonical_mint)
    }

    /// The cross-margin trader account (PDA index 0, subaccount 0) of
    /// `authority`.
    pub fn trader_account(&self, authority: &Pubkey) -> Pubkey {
        TraderKey::derive_pda(authority, 0, 0)
    }

    /// Phoenix's instruction builder.
    pub fn tx(&self) -> PhoenixTxBuilder<'_> {
        PhoenixTxBuilder::new(&self.metadata)
    }

    /// Read-only views, computed by simulating Phoenix's Hawkeye program
    /// against `rpc_url`. `fee_payer` only needs to exist; nothing is signed
    /// or charged.
    pub fn hawkeye(&self, rpc_url: &str, fee_payer: Pubkey) -> PhoenixHawkeyeClient<'_> {
        PhoenixHawkeyeClient::new(rpc_url.to_string(), &self.metadata).with_fee_payer(fee_payer)
    }
}

fn parse_key(key: &str) -> Result<Pubkey, PhoenixError> {
    Pubkey::from_str(key).map_err(|_| PhoenixError::InvalidExchangeKey(key.to_string()))
}
