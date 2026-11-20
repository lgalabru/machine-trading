//! A Phoenix perpetual, read from the feed's accounts: the mark, the
//! oracle and the account's position, decoded from the perp asset map and
//! the trader account with no RPC call and no simulation.
//!
//! These are the accounts' numbers, which are Phoenix's Hawkeye views'
//! (what a grade reads, through `mtx_kit::Trader`) only while the accounts
//! are current: the oracle always; the mark until the book moves after a
//! crank; the position until the account makes a fill it hasn't settled
//! (`crates/grader/tests/geyser_parity.rs`). The best bid and ask are not
//! here at all: Phoenix's liquidity is quoted through spline accounts,
//! whose live prices the accounts' decoders don't give. `docs/rfd/0004`
//! has what was measured, and the ways on.

use mtx_core::feed::Address;
use mtx_kit::{
    phoenix_rise::types::exchange::{ExchangeMarketConfig, ExchangeResponse},
    venue::{PhoenixVenue, key, perp_position, perp_prices},
};
use serde::Serialize;
use solana_pubkey::Pubkey;

use super::feed::AccountCache;

/// One account's view of one perp market, and where it is read from.
#[derive(Debug, Clone)]
pub struct PerpReader {
    market: ExchangeMarketConfig,
    asset_map: Address,
    trader: Address,
}

/// What [`PerpReader::read`] found, in USD and base units.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct PerpReading {
    pub mark: f64,
    pub oracle: f64,
    /// The position, positive long, and its average entry.
    pub position: f64,
    pub entry_usd: f64,
}

impl PerpReader {
    /// The reader of `authority`'s cross-margin account on `symbol`, from
    /// Phoenix's `/exchange` metadata.
    pub fn new(
        exchange: ExchangeResponse,
        symbol: &str,
        authority: &Pubkey,
    ) -> Result<Self, String> {
        let venue = PhoenixVenue::from_exchange(exchange);
        Ok(Self {
            market: venue.market(symbol)?.clone(),
            asset_map: venue.perp_asset_map()?.to_bytes(),
            trader: venue.trader_account(authority).to_bytes(),
        })
    }

    pub fn symbol(&self) -> &str {
        &self.market.symbol
    }

    /// The accounts a read needs: what the feed has to watch.
    pub fn addresses(&self) -> [Address; 2] {
        [self.asset_map, self.trader]
    }

    /// The market and the account as the cache holds them. An account the
    /// feed hasn't seen is an error: a reading must never be a guess. A
    /// trader account that doesn't exist yet reads as one that is missing,
    /// not as a flat position.
    pub fn read(&self, cache: &AccountCache) -> Result<PerpReading, String> {
        let asset_map = cache
            .data(&self.asset_map)
            .ok_or_else(|| format!("the perp asset map {} wasn't seen", key_of(&self.asset_map)))?;
        let trader = cache
            .data(&self.trader)
            .ok_or_else(|| format!("the trader account {} wasn't seen", key_of(&self.trader)))?;
        let prices =
            perp_prices(&self.market, asset_map).map_err(|e| format!("the perp asset map: {e}"))?;
        let position =
            perp_position(&self.market, trader).map_err(|e| format!("the trader account: {e}"))?;
        Ok(PerpReading {
            mark: prices.mark,
            oracle: prices.oracle,
            position: position.size,
            entry_usd: position.entry_usd,
        })
    }
}

fn key_of(address: &Address) -> Pubkey {
    Pubkey::new_from_array(*address)
}

/// A pubkey from text, as an [`Address`].
pub fn address(text: &str) -> Result<Address, String> {
    key(text).map(|key| key.to_bytes())
}
