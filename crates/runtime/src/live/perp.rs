//! A Phoenix perpetual, read from the feed's accounts with no RPC call and
//! no simulation: the oracle and the mark from the perp asset map, the top
//! of the book from the orderbook and the splines, and the account's
//! position from the active trader buffer (or its trader account, while it
//! isn't active in the market).
//!
//! These are Phoenix's Hawkeye views' numbers (what a grade reads, through
//! `mtx_kit::Trader`): the top of the book is computed as `view_bbo` does
//! (`mtx_kit::venue::perp_top_of_book`), and the position is the matching
//! engine's own. The mark is the one the last crank stored, which the
//! view's equals while the market is cranked every slot, as mainnet's is;
//! between cranks the view blends the book as it stands
//! (`crates/grader/tests/geyser_parity.rs`, `docs/rfd/0004`).

use mtx_core::feed::Address;
use mtx_kit::{
    phoenix_rise::types::exchange::{ExchangeMarketConfig, ExchangeResponse},
    venue::{
        PerpBookAccounts, PhoenixVenue, key, perp_index, perp_position, perp_position_live,
        perp_prices, perp_top_of_book, ticks_to_usd,
    },
};
use serde::Serialize;
use solana_pubkey::Pubkey;

use super::feed::AccountCache;

/// The trader index's and the active trader buffer's accounts a reader
/// takes at most: Phoenix's `/exchange` lists one of each today.
const ARENA_ACCOUNTS_MAX: usize = 16;

/// One account's view of one perp market, and where it is read from.
#[derive(Debug, Clone)]
pub struct PerpReader {
    venue: PhoenixVenue,
    market: ExchangeMarketConfig,
    asset_map: Address,
    trader: Address,
    orderbook: Address,
    spline_collection: Address,
    global_trader_index: Vec<Address>,
    active_trader_buffer: Vec<Address>,
    /// Read a trader account the cache doesn't hold as flat
    /// ([`PerpReader::flat_without_account`]).
    flat_without_account: bool,
}

/// What [`PerpReader::read`] found, in USD and base units.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct PerpReading {
    pub mark: f64,
    pub oracle: f64,
    /// Hawkeye's index: the fresh spot oracles' median.
    pub index: f64,
    /// The book's best bid and ask, if it has any.
    pub best_bid: Option<f64>,
    pub best_ask: Option<f64>,
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
        let keys = |list: &[String], what: &str| -> Result<Vec<Address>, String> {
            if list.is_empty() || list.len() > ARENA_ACCOUNTS_MAX {
                return Err(format!(
                    "the {what} has {} accounts: 1 to {ARENA_ACCOUNTS_MAX}",
                    list.len()
                ));
            }
            list.iter().map(|text| address(text)).collect()
        };
        let global_trader_index = keys(&exchange.keys.global_trader_index, "global trader index")?;
        let active_trader_buffer =
            keys(&exchange.keys.active_trader_buffer, "active trader buffer")?;
        let venue = PhoenixVenue::from_exchange(exchange);
        let market = venue.market(symbol)?.clone();
        Ok(Self {
            asset_map: venue.perp_asset_map()?.to_bytes(),
            trader: venue.trader_account(authority).to_bytes(),
            orderbook: address(&market.market_pubkey)?,
            spline_collection: address(&market.spline_pubkey)?,
            global_trader_index,
            active_trader_buffer,
            market,
            venue,
            flat_without_account: false,
        })
    }

    /// Read a trader account the cache doesn't hold as a flat position,
    /// rather than as an error: for a shadow run, whose key may never have
    /// opened an account on the venue, and sends nothing that could hold a
    /// position. Every other reading still refuses to guess.
    pub fn flat_without_account(self) -> Self {
        Self {
            flat_without_account: true,
            ..self
        }
    }

    pub fn symbol(&self) -> &str {
        &self.market.symbol
    }

    /// The trader account whose position is read.
    pub fn trader(&self) -> Address {
        self.trader
    }

    /// The accounts a read needs: what the feed has to watch.
    pub fn addresses(&self) -> Vec<Address> {
        [
            self.asset_map,
            self.trader,
            self.orderbook,
            self.spline_collection,
        ]
        .into_iter()
        .chain(self.global_trader_index.iter().copied())
        .chain(self.active_trader_buffer.iter().copied())
        .collect()
    }

    /// The market and the account as the cache holds them, at `slot` (a
    /// spline region's life is counted in slots). An account the feed
    /// hasn't seen is an error: a reading must never be a guess. The trader
    /// account is read only while the trader isn't active in the market,
    /// and one that doesn't exist yet reads as missing, not as flat.
    pub fn read(&self, cache: &AccountCache, slot: u64) -> Result<PerpReading, String> {
        let seen = |address: &Address, what: &str| {
            cache
                .data(address)
                .ok_or_else(|| format!("the {what} {} wasn't seen", key_of(address)))
        };
        let seen_all = |addresses: &[Address], what: &str| -> Result<Vec<&[u8]>, String> {
            addresses
                .iter()
                .map(|address| seen(address, what))
                .collect()
        };
        let asset_map = seen(&self.asset_map, "perp asset map")?;
        let global_trader_index = seen_all(&self.global_trader_index, "global trader index")?;
        let active_trader_buffer = seen_all(&self.active_trader_buffer, "active trader buffer")?;
        let book = PerpBookAccounts {
            perp_asset_map: asset_map,
            orderbook: seen(&self.orderbook, "orderbook")?,
            spline_collection: seen(&self.spline_collection, "spline collection")?,
            global_trader_index: &global_trader_index,
            active_trader_buffer: &active_trader_buffer,
        };
        let prices =
            perp_prices(&self.market, asset_map).map_err(|e| format!("the perp asset map: {e}"))?;
        let index =
            perp_index(&self.market, asset_map, slot).map_err(|e| format!("the index: {e}"))?;
        let top = perp_top_of_book(&self.venue, &self.market.symbol, &book, slot)?;
        let live = perp_position_live(
            &self.market,
            &key_of(&self.trader),
            &global_trader_index,
            &active_trader_buffer,
        )?;
        let position = match (live, cache.data(&self.trader)) {
            (Some(position), _) => position,
            (None, Some(account)) => perp_position(&self.market, account)
                .map_err(|e| format!("the trader account: {e}"))?,
            (None, None) if self.flat_without_account => mtx_kit::Position::default(),
            (None, None) => {
                return Err(format!(
                    "the trader account {} wasn't seen",
                    key_of(&self.trader)
                ));
            }
        };
        let usd = |ticks: u64| ticks_to_usd(&self.market, ticks);
        Ok(PerpReading {
            mark: prices.mark,
            oracle: prices.oracle,
            index,
            best_bid: top.best_bid_ticks.map(usd),
            best_ask: top.best_ask_ticks.map(usd),
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
