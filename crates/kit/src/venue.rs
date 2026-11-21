//! What a strategy reads of Phoenix without the chain (feature `venue`): the
//! market metadata the session hands it ([`Session::venue`](crate::Session)),
//! the conversions between Phoenix's integer units and USD, the accounts a
//! strategy reads through [`rpc_account`](crate::rpc_account), and
//! Phoenix's own decoding of them ([`phoenix_rise::accounts`]). It builds
//! for `wasm32-wasip2`, so a component can use it.

mod top;

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

/// `market`'s index price at `current_slot`, in USD, as Hawkeye's
/// `view_bbo` reports it: the median of the spot oracles' reports no older
/// than the perp asset map's spot staleness threshold, the two middle ones
/// averaged (rounded down) when their number is even. The same as the view
/// on 153 of 153 mainnet SOL samples (`crates/kit/tests/perp_book.rs`).
pub fn perp_index(
    market: &ExchangeMarketConfig,
    asset_map: &[u8],
    current_slot: u64,
) -> Result<f64, String> {
    use phoenix_rise::accounts::perp_asset_map::PerpAssetMap;
    let map = PerpAssetMap::try_from_account_bytes(asset_map).map_err(|e| e.to_string())?;
    let entry = map
        .find_by_symbol(&market.symbol)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("no market {} in the perp asset map", market.symbol))?;
    let spot = &entry
        .metadata
        .oracle_price()
        .mark_price
        .spot_price_component;
    let mut fresh: Vec<u64> = spot
        .last_exchange_spot_price
        .iter()
        .filter(|report| report.ticks.as_inner() > 0 && report.ticks.as_inner() != u64::MAX)
        .filter(|report| report.slot.saturating_add(spot.stale_threshold) >= current_slot)
        .map(|report| report.ticks.as_inner())
        .collect();
    fresh.sort_unstable();
    let middle = fresh.len() / 2;
    let ticks = match fresh.len() {
        0 => return Err(format!("no fresh oracle report for {}", market.symbol)),
        n if n % 2 == 1 => fresh[middle],
        _ => {
            fresh[middle - 1] / 2
                + fresh[middle] / 2
                + (fresh[middle - 1] % 2 + fresh[middle] % 2) / 2
        }
    };
    Ok(ticks_to_usd(market, ticks))
}

/// The position in `market` a trader account holds, from the account's
/// bytes: flat if it holds none. The entry is Phoenix's own: the position's
/// quote lots per base lot, rounded down to a whole number.
///
/// The account is the position as of its last settlement: a maker's fills
/// since are in the active trader buffer ([`perp_position_live`]).
pub fn perp_position(
    market: &ExchangeMarketConfig,
    trader_account: &[u8],
) -> Result<crate::Position, String> {
    use phoenix_rise::accounts::trader::Trader;
    let trader = Trader::try_from_account_bytes(trader_account).map_err(|e| e.to_string())?;
    let position = trader
        .positions()
        .find(|(asset, _)| *asset == u64::from(market.asset_id))
        .map(|(_, position)| position.to_position());
    Ok(position.map_or(FLAT, |p| {
        position_of(
            market,
            p.base_lot_position.as_inner(),
            p.virtual_quote_lot_position.as_inner(),
        )
    }))
}

/// The live position in `market` of the trader account `trader`, as
/// Phoenix's matching engine holds it: its entry in the active trader
/// buffer, found through the global trader index (each a header account
/// and its overflow accounts, in order). `None` if the trader isn't active
/// in `market`: its account then holds the position ([`perp_position`]).
pub fn perp_position_live(
    market: &ExchangeMarketConfig,
    trader: &Pubkey,
    global_trader_index: &[&[u8]],
    active_trader_buffer: &[&[u8]],
) -> Result<Option<crate::Position>, String> {
    let traders = Traders::decode(global_trader_index, active_trader_buffer)?;
    Ok(traders.position(trader, market.asset_id).map(|position| {
        position_of(
            market,
            position.base_lot_position.as_inner(),
            position.virtual_quote_lot_position.as_inner(),
        )
    }))
}

const FLAT: crate::Position = crate::Position {
    size: 0.0,
    entry_usd: 0.0,
};

fn position_of(market: &ExchangeMarketConfig, base_lots: i64, quote_lots: i64) -> crate::Position {
    if base_lots == 0 {
        return FLAT;
    }
    let quote_lots_per_base_lot = quote_lots.unsigned_abs() / base_lots.unsigned_abs();
    crate::Position {
        size: lots_to_base_units(market, base_lots),
        entry_usd: entry_price_to_usd(market, quote_lots_per_base_lot),
    }
}

/// A perp's best bid and ask, in ticks, as Phoenix's Hawkeye `view_bbo`
/// reports them: `None` for a side with no liquidity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TopOfBook {
    pub best_bid_ticks: Option<u64>,
    pub best_ask_ticks: Option<u64>,
}

/// The accounts a perp's book is made of, as the chain holds them: the five
/// that Hawkeye's `view_bbo` reads besides the global config. The trader
/// index and the active trader buffer are multi-arena trees: their header
/// account first, then every overflow account in order.
#[derive(Debug, Clone, Copy)]
pub struct PerpBookAccounts<'a> {
    pub perp_asset_map: &'a [u8],
    pub orderbook: &'a [u8],
    pub spline_collection: &'a [u8],
    pub global_trader_index: &'a [&'a [u8]],
    pub active_trader_buffer: &'a [&'a [u8]],
}

/// Positions read for one maker's margin: one a market, and Phoenix lists
/// a few dozen.
const POSITIONS_PER_TRADER_MAX: usize = 256;

/// Spline ticks walked per side of one spline. A region can span any number
/// of ticks, and only the ticks a cross can reach matter to the top of the
/// book; the widest ladder in 153 mainnet SOL samples ends 200 ticks out.
const SPLINE_TICKS_PER_SIDE_MAX: u64 = 4_096;

/// `symbol`'s top of book at `current_slot`, from its accounts alone: the
/// same as Hawkeye's `view_bbo` on 153 of 153 mainnet SOL samples and at
/// every step of the bench's flash-crash and market-making sessions
/// (`crates/kit/tests/perp_book.rs`, `crates/grader/tests/geyser_parity.rs`,
/// `docs/rfd/0004`).
///
/// The book is the orderbook's resting orders and each spline's live
/// regions, tick by tick: a bid region `[start, end)` quotes at
/// `mid - (tick + bid_base_width)`, an ask at `mid + (tick + ask_base_width)`,
/// with the region's fills taken off its nearest ticks first. A side quotes no more
/// than the maker's position limit leaves, nor than its margin funds
/// ([`MakerMargin`]). Where makers' quotes lock or cross, they match: the
/// best bid and ask each lose the smaller of their two sizes, until the
/// book no longer crosses.
pub fn perp_top_of_book(
    venue: &PhoenixVenue,
    symbol: &str,
    accounts: &PerpBookAccounts<'_>,
    current_slot: u64,
) -> Result<TopOfBook, String> {
    let traders = Traders::decode(accounts.global_trader_index, accounts.active_trader_buffer)?;
    top_of_book(venue, symbol, accounts, current_slot, &traders)
}

/// [`perp_top_of_book`] and [`perp_position_live`] for `trader`, from one
/// decoding of the trader accounts: what a strategy's market read needs.
pub fn perp_top_and_position(
    venue: &PhoenixVenue,
    symbol: &str,
    accounts: &PerpBookAccounts<'_>,
    trader: &Pubkey,
    current_slot: u64,
) -> Result<(TopOfBook, Option<crate::Position>), String> {
    let market = venue.market(symbol)?;
    let traders = Traders::decode(accounts.global_trader_index, accounts.active_trader_buffer)?;
    let top = top_of_book(venue, symbol, accounts, current_slot, &traders)?;
    let position = traders.position(trader, market.asset_id).map(|position| {
        position_of(
            market,
            position.base_lot_position.as_inner(),
            position.virtual_quote_lot_position.as_inner(),
        )
    });
    Ok((top, position))
}

fn top_of_book(
    venue: &PhoenixVenue,
    symbol: &str,
    accounts: &PerpBookAccounts<'_>,
    current_slot: u64,
    traders: &Traders<'_>,
) -> Result<TopOfBook, String> {
    use phoenix_rise::accounts::{orderbook::Orderbook, spline_collection::SplineCollection};
    type Stream = Box<dyn Iterator<Item = (u64, u64)>>;
    let market = venue.market(symbol)?;
    let marks = Marks::decode(venue, accounts.perp_asset_map)?;
    let (mut bid_orders, mut ask_orders) = (Vec::new(), Vec::new());
    {
        let aligned = aligned(accounts.orderbook);
        let orderbook = Orderbook::try_from_account_bytes(aligned.as_ref())
            .map_err(|e| format!("the orderbook: {e}"))?;
        for order in orderbook.bid_orders() {
            bid_orders.push((
                order.price_in_ticks().as_inner(),
                order.num_base_lots_remaining().as_inner(),
            ));
        }
        for order in orderbook.ask_orders() {
            ask_orders.push((
                order.price_in_ticks().as_inner(),
                order.num_base_lots_remaining().as_inner(),
            ));
        }
    }
    let mut bids: Vec<Stream> = Vec::new();
    let mut asks: Vec<Stream> = Vec::new();
    let splines = SplineCollection::try_from_account_bytes(accounts.spline_collection)
        .map_err(|e| format!("the spline collection: {e}"))?;
    for spline in splines.splines() {
        let spline = spline.map_err(|e| format!("a spline: {e}"))?;
        if !spline.is_enabled() {
            continue;
        }
        let (bid_lots_max, ask_lots_max) = spline_capacity(&spline, market, &marks, traders);
        let mid = spline.mid_price().as_inner();
        let updated = spline.user_update_slot();
        let regions = |regions: &[phoenix_rise::accounts::spline_collection::TickRegion]| {
            regions
                .iter()
                .filter(|r| r.is_active(current_slot, updated) && !r.is_empty())
                .map(|r| top::Region {
                    start: r.start_offset().as_inner(),
                    end: r.end_offset().as_inner(),
                    lots_per_tick: r.density().as_inner(),
                    filled: r.filled_size().as_inner(),
                })
                .collect::<Vec<_>>()
        };
        for (is_bid, walk) in [
            (
                true,
                top::Walk::new(
                    regions(spline.active_bid_regions()),
                    mid,
                    spline.bid_base_width().as_inner(),
                    true,
                    bid_lots_max,
                    SPLINE_TICKS_PER_SIDE_MAX,
                ),
            ),
            (
                false,
                top::Walk::new(
                    regions(spline.active_ask_regions()),
                    mid,
                    spline.ask_base_width().as_inner(),
                    false,
                    ask_lots_max,
                    SPLINE_TICKS_PER_SIDE_MAX,
                ),
            ),
        ] {
            let (orders, streams) = match is_bid {
                true => (&mut bid_orders, &mut bids),
                false => (&mut ask_orders, &mut asks),
            };
            // A spline whose regions aren't in order of their offsets is
            // read whole, with the resting orders, which are sorted below.
            match walk.regions_in_order() {
                true => streams.push(Box::new(walk)),
                false => orders.extend(walk),
            }
        }
    }
    bids.push(top::sorted(bid_orders, true));
    asks.push(top::sorted(ask_orders, false));
    let (best_bid_ticks, best_ask_ticks) = top::top(bids, asks);
    Ok(TopOfBook {
        best_bid_ticks,
        best_ask_ticks,
    })
}

/// Where [`perp_top_of_book`]'s time goes, stage by stage, in
/// microseconds: for the runtime's read benchmark.
#[doc(hidden)]
#[cfg(not(target_arch = "wasm32"))]
pub fn perp_top_of_book_profile(
    venue: &PhoenixVenue,
    symbol: &str,
    accounts: &PerpBookAccounts<'_>,
    current_slot: u64,
) -> Result<Vec<(&'static str, f64)>, String> {
    use phoenix_rise::accounts::spline_collection::SplineCollection;
    let us = |t: std::time::Instant| t.elapsed().as_secs_f64() * 1e6;
    let mut stages = Vec::new();
    let market = venue.market(symbol)?;
    let t = std::time::Instant::now();
    let marks = Marks::decode(venue, accounts.perp_asset_map)?;
    stages.push(("marks", us(t)));
    let t = std::time::Instant::now();
    let traders = Traders::decode(accounts.global_trader_index, accounts.active_trader_buffer)?;
    stages.push(("traders", us(t)));
    let t = std::time::Instant::now();
    let splines = SplineCollection::try_from_account_bytes(accounts.spline_collection)
        .map_err(|e| format!("the spline collection: {e}"))?;
    let all: Vec<_> = splines
        .splines()
        .collect::<Result<_, _>>()
        .map_err(|e| format!("a spline: {e}"))?;
    stages.push(("splines", us(t)));
    let enabled: Vec<_> = all.iter().filter(|s| s.is_enabled()).collect();
    stages.push(("splines enabled", enabled.len() as f64));
    let t = std::time::Instant::now();
    let mut position_us = 0.0;
    let mut margin_us = 0.0;
    for spline in &enabled {
        let trader = Pubkey::new_from_array(spline.trader());
        let p = std::time::Instant::now();
        let position_lots = traders
            .position(&trader, market.asset_id)
            .map_or(0, |position| position.base_lot_position.as_inner());
        position_us += us(p);
        let m = std::time::Instant::now();
        let _ = traders
            .margin(&trader, &marks)
            .map(|margin| margin.capacity(market, &marks, position_lots));
        margin_us += us(m);
    }
    stages.push(("capacity", us(t)));
    stages.push(("  position lookups", position_us));
    stages.push(("  margins", margin_us));
    let t = std::time::Instant::now();
    let mut levels = 0u64;
    for spline in &enabled {
        let mid = spline.mid_price().as_inner();
        let updated = spline.user_update_slot();
        let regions: Vec<top::Region> = spline
            .active_bid_regions()
            .iter()
            .filter(|r| r.is_active(current_slot, updated) && !r.is_empty())
            .map(|r| top::Region {
                start: r.start_offset().as_inner(),
                end: r.end_offset().as_inner(),
                lots_per_tick: r.density().as_inner(),
                filled: r.filled_size().as_inner(),
            })
            .collect();
        levels += top::Walk::new(
            regions,
            mid,
            spline.bid_base_width().as_inner(),
            true,
            u64::MAX,
            SPLINE_TICKS_PER_SIDE_MAX,
        )
        .count() as u64;
    }
    stages.push(("walk every bid level", us(t)));
    stages.push(("bid levels", levels as f64));
    let t = std::time::Instant::now();
    perp_top_of_book(venue, symbol, accounts, current_slot)?;
    stages.push(("whole", us(t)));
    Ok(stages)
}

/// The book built whole, level by level, then uncrossed: what
/// [`perp_top_of_book`] did before it merged the ladders lazily, kept to
/// check it against (the runtime's read benchmark).
#[doc(hidden)]
pub fn perp_top_of_book_reference(
    venue: &PhoenixVenue,
    symbol: &str,
    accounts: &PerpBookAccounts<'_>,
    current_slot: u64,
) -> Result<TopOfBook, String> {
    let market = venue.market(symbol)?;
    let marks = Marks::decode(venue, accounts.perp_asset_map)?;
    let mut book = Ladders::default();
    add_resting_orders(&mut book, accounts.orderbook)?;
    add_splines(&mut book, market, &marks, accounts, current_slot)?;
    book.uncross();
    let top = TopOfBook {
        best_bid_ticks: book.bids.last_key_value().map(|(&ticks, _)| ticks),
        best_ask_ticks: book.asks.first_key_value().map(|(&ticks, _)| ticks),
    };
    if let (Some(bid), Some(ask)) = (top.best_bid_ticks, top.best_ask_ticks) {
        assert!(bid < ask, "an uncrossed book: bid {bid} under ask {ask}");
    }
    Ok(top)
}

/// Base lots at each price in ticks, a side each.
#[derive(Debug, Default)]
struct Ladders {
    bids: std::collections::BTreeMap<u64, u64>,
    asks: std::collections::BTreeMap<u64, u64>,
}

impl Ladders {
    fn add(&mut self, is_bid: bool, ticks: u64, lots: u64) {
        assert!(lots > 0, "a level holds lots");
        let side = if is_bid {
            &mut self.bids
        } else {
            &mut self.asks
        };
        let level = side.entry(ticks).or_insert(0);
        *level = level.saturating_add(lots);
    }

    /// Matches the best bid against the best ask while they lock or cross.
    /// Each round empties at least one level, so it ends within as many
    /// rounds as the book has levels.
    fn uncross(&mut self) {
        let rounds_max = self.bids.len() + self.asks.len();
        for _ in 0..rounds_max {
            let (Some((&bid, &bid_lots)), Some((&ask, &ask_lots))) =
                (self.bids.last_key_value(), self.asks.first_key_value())
            else {
                return;
            };
            if bid < ask {
                return;
            }
            let matched = bid_lots.min(ask_lots);
            take(&mut self.bids, bid, matched);
            take(&mut self.asks, ask, matched);
        }
        assert!(
            match (self.bids.last_key_value(), self.asks.first_key_value()) {
                (Some((bid, _)), Some((ask, _))) => bid < ask,
                _ => true,
            },
            "the book still crosses after every level was matched"
        );
    }
}

fn take(side: &mut std::collections::BTreeMap<u64, u64>, ticks: u64, lots: u64) {
    let level = side.get_mut(&ticks).expect("the level being matched");
    assert!(*level >= lots, "a match takes no more than the level holds");
    *level -= lots;
    if *level == 0 {
        side.remove(&ticks);
    }
}

/// The orderbook's bytes, aligned as its decoder needs (the alignment of
/// its widest field): borrowed when they already are, copied when not.
fn aligned(orderbook: &[u8]) -> std::borrow::Cow<'_, [u8]> {
    if (orderbook.as_ptr() as usize).is_multiple_of(std::mem::align_of::<u128>()) {
        return std::borrow::Cow::Borrowed(orderbook);
    }
    let words: Vec<u128> = orderbook
        .chunks(16)
        .map(|chunk| {
            let mut word = [0u8; 16];
            word[..chunk.len()].copy_from_slice(chunk);
            u128::from_ne_bytes(word)
        })
        .collect();
    std::borrow::Cow::Owned(bytemuck::cast_slice::<u128, u8>(&words)[..orderbook.len()].to_vec())
}

fn add_resting_orders(book: &mut Ladders, orderbook: &[u8]) -> Result<(), String> {
    use phoenix_rise::accounts::orderbook::Orderbook;
    let bytes = aligned(orderbook);
    let orderbook = Orderbook::try_from_account_bytes(bytes.as_ref())
        .map_err(|e| format!("the orderbook: {e}"))?;
    for (is_bid, orders) in [
        (true, orderbook.bid_orders()),
        (false, orderbook.ask_orders()),
    ] {
        for order in orders {
            let lots = order.num_base_lots_remaining().as_inner();
            if lots > 0 {
                book.add(is_bid, order.price_in_ticks().as_inner(), lots);
            }
        }
    }
    Ok(())
}

fn add_splines(
    book: &mut Ladders,
    market: &ExchangeMarketConfig,
    marks: &Marks<'_>,
    accounts: &PerpBookAccounts<'_>,
    current_slot: u64,
) -> Result<(), String> {
    use phoenix_rise::accounts::spline_collection::SplineCollection;
    let traders = Traders::decode(accounts.global_trader_index, accounts.active_trader_buffer)?;
    let splines = SplineCollection::try_from_account_bytes(accounts.spline_collection)
        .map_err(|e| format!("the spline collection: {e}"))?;
    for spline in splines.splines() {
        let spline = spline.map_err(|e| format!("a spline: {e}"))?;
        if !spline.is_enabled() {
            continue;
        }
        let capacity = spline_capacity(&spline, market, marks, &traders);
        add_spline(book, &spline, capacity, current_slot);
    }
    Ok(())
}

/// What a spline may quote each way: what its position limits leave, and
/// what its maker's margin carries. A maker the index doesn't hold, or with
/// no position in this market, is flat.
fn spline_capacity(
    spline: &phoenix_rise::accounts::spline_collection::Spline,
    market: &ExchangeMarketConfig,
    marks: &Marks<'_>,
    traders: &Traders<'_>,
) -> (u64, u64) {
    let trader = Pubkey::new_from_array(spline.trader());
    let position_lots = traders
        .position(&trader, market.asset_id)
        .map_or(0, |position| position.base_lot_position.as_inner());
    let (position_bids, position_asks) = position_capacity(spline, position_lots);
    let (margin_bids, margin_asks) = traders
        .margin(&trader, marks)
        .map_or((u64::MAX, u64::MAX), |margin| {
            margin.capacity(market, marks, position_lots)
        });
    (
        position_bids.min(margin_bids),
        position_asks.min(margin_asks),
    )
}

/// One spline's ladder, both sides: Phoenix's own walk of its regions
/// (`phoenix_rise::accounts::owned::Spline::liquidity_at_slot_with_capacity`).
fn add_spline(
    book: &mut Ladders,
    spline: &phoenix_rise::accounts::spline_collection::Spline,
    (bid_lots_max, ask_lots_max): (u64, u64),
    current_slot: u64,
) {
    let mid = spline.mid_price().as_inner();
    let updated = spline.user_update_slot();
    let sides = [
        (
            true,
            spline.active_bid_regions(),
            spline.bid_base_width().as_inner(),
            bid_lots_max,
        ),
        (
            false,
            spline.active_ask_regions(),
            spline.ask_base_width().as_inner(),
            ask_lots_max,
        ),
    ];
    for (is_bid, regions, base_width, lots_max) in sides {
        let mut lots_left = lots_max;
        let mut ticks_walked = 0u64;
        for region in regions {
            if !region.is_active(current_slot, updated) || region.is_empty() {
                continue;
            }
            let lots_per_tick = region.density().as_inner();
            let mut fills_left = region.filled_size().as_inner();
            let end = region.end_offset().as_inner();
            let mut tick = region.start_offset().as_inner();
            while tick < end && lots_left > 0 && ticks_walked < SPLINE_TICKS_PER_SIDE_MAX {
                let filled = fills_left.min(lots_per_tick);
                fills_left -= filled;
                let lots = (lots_per_tick - filled).min(lots_left);
                let offset = tick.saturating_add(base_width);
                if lots > 0 {
                    let ticks = if is_bid {
                        mid.checked_sub(offset)
                    } else {
                        mid.checked_add(offset)
                    };
                    if let Some(ticks) = ticks.filter(|&t| t > 0) {
                        book.add(is_bid, ticks, lots);
                        lots_left -= lots;
                    }
                }
                tick += 1;
                ticks_walked += 1;
            }
        }
    }
}

/// The active traders: who is where in the global trader index, and their
/// live positions in the active trader buffer.
struct Traders<'a> {
    index: phoenix_rise::accounts::global_trader_index::GlobalTraderIndex<'a>,
    buffer: phoenix_rise::accounts::active_trader_buffer::ActiveTraderBuffer<'a>,
    /// Every trader's positions, by node, read in one pass the first time a
    /// margin is asked for: the buffer's own lookup scans all of it for each
    /// trader, and a book asks for every spline maker's.
    holdings: std::cell::OnceCell<std::collections::HashMap<u32, Vec<Held>>>,
}

/// One position, as a maker's margin reads it.
#[derive(Debug, Clone, Copy)]
struct Held {
    asset_id: u32,
    base_lots: i64,
    quote_lots: i64,
}

impl<'a> Traders<'a> {
    fn decode(
        global_trader_index: &[&'a [u8]],
        active_trader_buffer: &[&'a [u8]],
    ) -> Result<Self, String> {
        use phoenix_rise::accounts::{
            active_trader_buffer::ActiveTraderBuffer, global_trader_index::GlobalTraderIndex,
        };
        Ok(Self {
            index: GlobalTraderIndex::try_from_account_buffers(global_trader_index.iter().copied())
                .map_err(|e| format!("the global trader index: {e}"))?,
            buffer: ActiveTraderBuffer::try_from_account_buffers(
                active_trader_buffer.iter().copied(),
            )
            .map_err(|e| format!("the active trader buffer: {e}"))?,
            holdings: std::cell::OnceCell::new(),
        })
    }

    fn holdings(&self) -> &std::collections::HashMap<u32, Vec<Held>> {
        self.holdings.get_or_init(|| {
            let mut by_node: std::collections::HashMap<u32, Vec<Held>> =
                std::collections::HashMap::new();
            for (id, entry) in self.buffer.iter_all_positions() {
                by_node
                    .entry(id.trader_id().to_u32_or_sentinel())
                    .or_default()
                    .push(Held {
                        asset_id: id.asset_id(),
                        base_lots: entry.position.base_lot_position.as_inner(),
                        quote_lots: entry.position.virtual_quote_lot_position.as_inner(),
                    });
            }
            by_node
        })
    }

    /// `trader`'s cross margin, if it is active and every market it holds
    /// a position in has a mark: a maker the margin can't be worked out
    /// for is sized by its position limit alone.
    fn margin(&self, trader: &Pubkey, marks: &Marks<'_>) -> Option<MakerMargin> {
        let node = self.index.index(trader)?;
        let state = self.index.get(trader)?;
        let mut free = i128::from(state.quote_lot_collateral.as_inner());
        let positions = self.holdings().get(&node).map_or(&[][..], Vec::as_slice);
        for position in positions.iter().take(POSITIONS_PER_TRADER_MAX) {
            let (market, mark) = marks.get(position.asset_id)?;
            let unit_price = i128::from(mark) * i128::from(market.tick_size);
            let base = i128::from(position.base_lots);
            let quote = i128::from(position.quote_lots);
            let held = position.base_lots.unsigned_abs();
            free += base * unit_price + quote;
            free -= initial_margin(market, unit_price, held) * i128::from(held);
        }
        Some(MakerMargin {
            free_quote_lots: free,
        })
    }

    /// `trader`'s position in the asset, if it is active there.
    fn position(
        &self,
        trader: &Pubkey,
        asset_id: u32,
    ) -> Option<phoenix_rise::accounts::trader::TraderPosition> {
        use phoenix_rise::accounts::orderbook::{NodePointer, TraderPositionId};
        let node = self.index.index(trader)?;
        let entry = self
            .buffer
            .get_position(&TraderPositionId::new(NodePointer::new(node), asset_id))?;
        Some(entry.position)
    }
}

/// The lots of bids and of asks a spline's position limit leaves its maker
/// (`phoenix_rise::accounts::owned::Spline::position_limit_capacity`).
fn position_capacity(
    spline: &phoenix_rise::accounts::spline_collection::Spline,
    position_lots: i64,
) -> (u64, u64) {
    if !spline.has_max_position_size() {
        return (u64::MAX, u64::MAX);
    }
    let long = i128::from(spline.max_position_size_long().as_inner());
    let short = i128::from(spline.max_position_size_short().as_inner());
    let position = i128::from(position_lots);
    (lots_from(long - position), lots_from(short + position))
}

/// Every market's mark, from the perp asset map, keyed by asset id: what a
/// maker's positions are valued and margined at.
struct Marks<'a> {
    /// Each market's configuration and mark in ticks.
    markets: Vec<(&'a ExchangeMarketConfig, u64)>,
}

impl<'a> Marks<'a> {
    fn decode(venue: &'a PhoenixVenue, perp_asset_map: &[u8]) -> Result<Self, String> {
        use phoenix_rise::accounts::perp_asset_map::PerpAssetMap;
        let map = PerpAssetMap::try_from_account_bytes(perp_asset_map)
            .map_err(|e| format!("the perp asset map: {e}"))?;
        // One pass over the map, each entry decoded once: looking each
        // market up by symbol scans and decodes the map again, which on
        // mainnet's map was most of a book read.
        let mut markets = Vec::with_capacity(venue.exchange.markets.len());
        for entry in map.iter() {
            let entry = entry.map_err(|e| e.to_string())?;
            let Some(market) = venue
                .exchange
                .markets
                .iter()
                .find(|market| entry.symbol.matches(&market.symbol))
            else {
                continue;
            };
            let mark = entry
                .metadata
                .oracle_price()
                .mark_price
                .price
                .ticks
                .as_inner();
            markets.push((market, mark));
        }
        Ok(Self { markets })
    }

    fn get(&self, asset_id: u32) -> Option<(&'a ExchangeMarketConfig, u64)> {
        self.markets
            .iter()
            .find(|(m, _)| m.asset_id == asset_id)
            .copied()
    }
}

/// A maker's cross margin: what it holds against what its positions need.
///
/// Phoenix doesn't publish how the view sizes a spline by margin. This is
/// the rule the bench showed (`docs/rfd/0004`): a maker with $0.08 of
/// collateral quoted 8 lots while flat, and nothing that added risk once
/// it held a few. Free collateral is the collateral plus every position's
/// unrealized PnL at the mark, less every position's initial margin (its
/// value over its leverage tier's maximum); a side that adds risk quotes
/// what the free collateral margins at this market's mark.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MakerMargin {
    /// Free collateral, in quote lots: negative when the maker is short of
    /// initial margin already.
    pub free_quote_lots: i128,
}

impl MakerMargin {
    /// The lots of bids and of asks the free collateral funds for a maker
    /// holding `position_lots` of `market`: the side that reduces the
    /// position quotes the position, and what it frees, besides.
    fn capacity(
        &self,
        market: &ExchangeMarketConfig,
        marks: &Marks<'_>,
        position_lots: i64,
    ) -> (u64, u64) {
        let Some((_, mark)) = marks.get(market.asset_id) else {
            return (u64::MAX, u64::MAX);
        };
        let unit_price = i128::from(mark) * i128::from(market.tick_size);
        let held = position_lots.unsigned_abs();
        let margin_per_lot = initial_margin(market, unit_price, held.saturating_add(1)).max(1);
        let adding = lots_from(self.free_quote_lots / margin_per_lot);
        let freed =
            self.free_quote_lots + initial_margin(market, unit_price, held) * i128::from(held);
        let after_reducing = held.saturating_add(lots_from(freed / margin_per_lot));
        match position_lots {
            p if p > 0 => (adding, after_reducing),
            p if p < 0 => (after_reducing, adding),
            _ => (adding, adding),
        }
    }
}

/// The initial margin, in quote lots per base lot, of a position of `lots`
/// at `unit_price` quote lots per base lot: its value over the maximum
/// leverage of the tier its size falls in.
fn initial_margin(market: &ExchangeMarketConfig, unit_price: i128, lots: u64) -> i128 {
    let leverage = market
        .leverage_tiers
        .iter()
        .find(|tier| lots <= tier.max_size_base_lots)
        .or(market.leverage_tiers.last())
        .map_or(1.0, |tier| tier.max_leverage);
    assert!(leverage >= 1.0, "a leverage tier of at least 1x");
    // Ceil, as Phoenix rounds margin against the trader.
    (unit_price as f64 / leverage).ceil() as i128
}

/// Base lots from a signed count, none below zero.
fn lots_from(lots: i128) -> u64 {
    u64::try_from(lots.max(0)).unwrap_or(u64::MAX)
}
