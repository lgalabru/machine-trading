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
        PerpBookAccounts, PhoenixVenue, key, perp_index, perp_position, perp_prices,
        perp_top_and_position, perp_top_levels, ticks_to_usd,
    },
};
use serde::Serialize;
use solana_pubkey::Pubkey;

#[cfg(test)]
use super::feed::AccountCache;
use super::feed::Accounts;

/// The trader index's and the active trader buffer's accounts a reader
/// takes at most: Phoenix's `/exchange` lists one of each today.
const ARENA_ACCOUNTS_MAX: usize = 16;

/// The most accounts one reader reads: the asset map, the trader, the
/// orderbook and the splines, and the two arenas.
pub const ADDRESSES_MAX: usize = 4 + 2 * ARENA_ACCOUNTS_MAX;

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
/// A reader's identity, for sharing its readings ([`PerpReader::key`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ReaderKey {
    orderbook: Address,
    trader: Address,
    flat_without_account: bool,
}

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

    /// What makes two readers' readings the same: the market, and the
    /// account whose position they read.
    pub fn key(&self) -> ReaderKey {
        ReaderKey {
            orderbook: self.orderbook,
            trader: self.trader,
            flat_without_account: self.flat_without_account,
        }
    }

    /// The accounts a read needs: what the feed has to watch.
    pub fn addresses(&self) -> Vec<Address> {
        let addresses: Vec<Address> = [
            self.asset_map,
            self.trader,
            self.orderbook,
            self.spline_collection,
        ]
        .into_iter()
        .chain(self.global_trader_index.iter().copied())
        .chain(self.active_trader_buffer.iter().copied())
        .collect();
        assert!(
            addresses.len() <= ADDRESSES_MAX,
            "the arenas are bounded at construction"
        );
        addresses
    }

    /// The market's accounts as the cache holds them, handed to `read`
    /// with the perp asset map. An account the feed hasn't seen is an
    /// error: a reading must never be a guess.
    fn with_book<T>(
        &self,
        cache: &impl Accounts,
        read: impl FnOnce(&PerpBookAccounts<'_>) -> Result<T, String>,
    ) -> Result<T, String> {
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
        let global_trader_index = seen_all(&self.global_trader_index, "global trader index")?;
        let active_trader_buffer = seen_all(&self.active_trader_buffer, "active trader buffer")?;
        let book = PerpBookAccounts {
            perp_asset_map: seen(&self.asset_map, "perp asset map")?,
            orderbook: seen(&self.orderbook, "orderbook")?,
            spline_collection: seen(&self.spline_collection, "spline collection")?,
            global_trader_index: &global_trader_index,
            active_trader_buffer: &active_trader_buffer,
        };
        read(&book)
    }

    /// The market and the account as the cache holds them, at `slot` (a
    /// spline region's life is counted in slots). The trader account is
    /// read only while the trader isn't active in the market, and one that
    /// doesn't exist yet reads as missing, not as flat.
    pub fn read(&self, cache: &impl Accounts, slot: u64) -> Result<PerpReading, String> {
        let (prices, index, top, live) = self.with_book(cache, |book| {
            let prices = perp_prices(&self.market, book.perp_asset_map)
                .map_err(|e| format!("the perp asset map: {e}"))?;
            let index = perp_index(&self.market, book.perp_asset_map, slot)
                .map_err(|e| format!("the index: {e}"))?;
            let (top, live) = perp_top_and_position(
                &self.venue,
                &self.market.symbol,
                book,
                &key_of(&self.trader),
                slot,
            )?;
            Ok((prices, index, top, live))
        })?;
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

    /// The market's index and touch at `slot`: Hawkeye's index, the mark,
    /// and the best bid and ask with the base units resting at each. No
    /// trader account: what a watch of how the book follows the index reads.
    pub fn read_touch(&self, cache: &impl Accounts, slot: u64) -> Result<Touch, String> {
        let (prices, index, levels) = self.with_book(cache, |book| {
            let prices = perp_prices(&self.market, book.perp_asset_map)
                .map_err(|e| format!("the perp asset map: {e}"))?;
            let index = perp_index(&self.market, book.perp_asset_map, slot)
                .map_err(|e| format!("the index: {e}"))?;
            let levels = perp_top_levels(&self.venue, &self.market.symbol, book, slot)?;
            Ok((prices, index, levels))
        })?;
        let level = |(ticks, lots): (u64, u64)| {
            let lots = i64::try_from(lots).unwrap_or(i64::MAX);
            (
                ticks_to_usd(&self.market, ticks),
                mtx_kit::venue::lots_to_base_units(&self.market, lots),
            )
        };
        Ok(Touch {
            index,
            mark: prices.mark,
            bid: levels.bid.map(level),
            ask: levels.ask.map(level),
        })
    }
}

/// A market's index and touch ([`PerpReader::read_touch`]), in USD and base
/// units: `(price, size)` a side.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Touch {
    pub index: f64,
    pub mark: f64,
    pub bid: Option<(f64, f64)>,
    pub ask: Option<(f64, f64)>,
}

fn key_of(address: &Address) -> Pubkey {
    Pubkey::new_from_array(*address)
}

/// A pubkey from text, as an [`Address`].
pub fn address(text: &str) -> Result<Address, String> {
    key(text).map(|key| key.to_bytes())
}

#[cfg(test)]
mod bench {
    use mtx_kit::venue::{perp_position_live, perp_top_of_book};

    use super::*;

    /// A slot the market's index is fresh at: the newest the cache saw.
    fn book_slot(cache: &AccountCache, reader: &PerpReader) -> u64 {
        reader
            .addresses()
            .iter()
            .filter_map(|a| cache.slot(a))
            .max()
            .unwrap_or(0)
    }

    /// What a market read costs, stage by stage, on a node's real accounts:
    /// `MTX_BENCH_RPC=http://127.0.0.1:8899 MTX_BENCH_EXCHANGE=exchange.json
    /// cargo test --release -p mtx-runtime --features live --lib read_speed
    /// -- --ignored --nocapture`.
    #[test]
    #[ignore = "needs a node's RPC and Phoenix's exchange file"]
    fn read_speed() {
        let rpc = std::env::var("MTX_BENCH_RPC").unwrap();
        let exchange: ExchangeResponse = serde_json::from_str(
            &std::fs::read_to_string(std::env::var("MTX_BENCH_EXCHANGE").unwrap()).unwrap(),
        )
        .unwrap();
        let reader = PerpReader::new(exchange, "SOL", &Pubkey::new_unique())
            .unwrap()
            .flat_without_account();
        let mut cache = AccountCache::default();
        super::super::backfill_blocking(&rpc, &reader.addresses(), &mut cache).unwrap();
        let sizes: usize = reader
            .addresses()
            .iter()
            .filter_map(|a| cache.data(a))
            .map(<[u8]>::len)
            .sum();
        eprintln!(
            "accounts: {} bytes over {}",
            sizes,
            reader.addresses().len()
        );
        let rounds = 200;
        let time = |name: &str, f: &mut dyn FnMut()| {
            f();
            let started = std::time::Instant::now();
            for _ in 0..rounds {
                f();
            }
            eprintln!(
                "{name:<24} {:>9.1} µs",
                started.elapsed().as_secs_f64() * 1e6 / rounds as f64
            );
        };
        let slot = u64::MAX / 2;
        let asset_map = cache.data(&reader.asset_map).unwrap();
        let gti: Vec<&[u8]> = reader
            .global_trader_index
            .iter()
            .map(|a| cache.data(a).unwrap())
            .collect();
        let atb: Vec<&[u8]> = reader
            .active_trader_buffer
            .iter()
            .map(|a| cache.data(a).unwrap())
            .collect();
        let book = PerpBookAccounts {
            perp_asset_map: asset_map,
            orderbook: cache.data(&reader.orderbook).unwrap(),
            spline_collection: cache.data(&reader.spline_collection).unwrap(),
            global_trader_index: &gti,
            active_trader_buffer: &atb,
        };
        time("read (all of it)", &mut || {
            let _ = reader.read(&cache, slot);
        });
        time("prices", &mut || {
            let _ = perp_prices(&reader.market, asset_map);
        });
        time("index", &mut || {
            let _ = perp_index(&reader.market, asset_map, slot);
        });
        let real_slot = book_slot(&cache, &reader);
        time("top of book", &mut || {
            let _ = perp_top_of_book(&reader.venue, &reader.market.symbol, &book, real_slot);
        });
        time("top of book, built whole", &mut || {
            let _ = mtx_kit::venue::perp_top_of_book_reference(
                &reader.venue,
                &reader.market.symbol,
                &book,
                real_slot,
            );
        });
        // The merge and the whole book agree, at the slot the accounts were
        // read and at slots around it (a spline's regions expire by slot).
        for slot in real_slot.saturating_sub(300)..real_slot + 300 {
            let merged =
                perp_top_of_book(&reader.venue, &reader.market.symbol, &book, slot).unwrap();
            let whole = mtx_kit::venue::perp_top_of_book_reference(
                &reader.venue,
                &reader.market.symbol,
                &book,
                slot,
            )
            .unwrap();
            assert_eq!(
                (merged.best_bid_ticks, merged.best_ask_ticks),
                (whole.best_bid_ticks, whole.best_ask_ticks),
                "slot {slot}"
            );
        }
        eprintln!("the merge and the whole book agree at 600 slots around {real_slot}");
        for _ in 0..3 {
            let _ = mtx_kit::venue::perp_top_of_book_profile(
                &reader.venue,
                &reader.market.symbol,
                &book,
                real_slot,
            );
        }
        for (stage, value) in mtx_kit::venue::perp_top_of_book_profile(
            &reader.venue,
            &reader.market.symbol,
            &book,
            real_slot,
        )
        .unwrap()
        {
            eprintln!("  stage {stage:<24} {value:>9.1}");
        }
        time("trader index decode", &mut || {
            let _ = mtx_kit::phoenix_rise::accounts::global_trader_index::GlobalTraderIndex::try_from_account_buffers(gti.iter().copied());
        });
        time("trader buffer decode", &mut || {
            let _ = mtx_kit::phoenix_rise::accounts::active_trader_buffer::ActiveTraderBuffer::try_from_account_buffers(atb.iter().copied());
        });
        time("orderbook decode", &mut || {
            let _ = mtx_kit::phoenix_rise::accounts::orderbook::Orderbook::try_from_account_bytes(
                book.orderbook,
            );
        });
        time("orderbook walk", &mut || {
            let book =
                mtx_kit::phoenix_rise::accounts::orderbook::Orderbook::try_from_account_bytes(
                    book.orderbook,
                )
                .unwrap();
            let mut n = 0u64;
            for order in book.bid_orders().chain(book.ask_orders()) {
                n += order.num_base_lots_remaining().as_inner();
            }
            std::hint::black_box(n);
        });
        {
            let ob = mtx_kit::phoenix_rise::accounts::orderbook::Orderbook::try_from_account_bytes(
                book.orderbook,
            )
            .unwrap();
            let live = ob
                .bid_orders()
                .chain(ob.ask_orders())
                .filter(|o| o.num_base_lots_remaining().as_inner() > 0)
                .count();
            let all = ob.bid_orders().chain(ob.ask_orders()).count();
            eprintln!("orders: {live} live of {all} iterated");
        }
        time("splines decode", &mut || {
            let _ = mtx_kit::phoenix_rise::accounts::spline_collection::SplineCollection::try_from_account_bytes(book.spline_collection);
        });
        eprintln!(
            "orderbook {} B (aligned {}), splines {} B, index {:?} B, buffer {:?} B",
            book.orderbook.len(),
            (book.orderbook.as_ptr() as usize).is_multiple_of(16),
            book.spline_collection.len(),
            gti.iter().map(|a| a.len()).collect::<Vec<_>>(),
            atb.iter().map(|a| a.len()).collect::<Vec<_>>()
        );
        time("read at a live slot", &mut || {
            let _ = reader.read(&cache, real_slot);
        });
        time("position (live)", &mut || {
            let _ = perp_position_live(&reader.market, &key_of(&reader.trader), &gti, &atb);
        });
    }
}
