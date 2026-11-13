//! Spot sessions: a bot trading SOL/USDC on Phoenix v1 and OpenBook v2, or
//! SPCX/USDC in Raydium's, Meteora's and Orca's pools, as the venue profiles in
//! `specs/venues/` define them.
//!
//! A scenario with a [`Spot`] runs here instead of on Phoenix's perpetuals.
//! The session is the same as a perp session's (see [`crate::session`]):
//! a fresh offline world from a spot snapshot, the RPC guard, the bot's turns
//! in steps of [`SLOTS_PER_STEP`] slots. A scenario's
//! execution faults (see [`crate::faults`]) apply here as they do there,
//! opt-in. What differs is the account:
//!
//! 1. The bot's wallet starts with the scenario's base asset (wrapped SOL,
//!    or the pools' base) and USDC, and SOL for fees. It claims its own
//!    Phoenix seat or OpenBook open orders account if it wants to rest
//!    orders; a take or a swap needs neither.
//! 2. Every step, the session reads the wallet and what each venue holds
//!    for it, as the profiles count them: the position is the base held,
//!    wherever it sits, and the value is both assets at the base's Pyth
//!    price and USDC at $1, each counted once, and any pool positions (the
//!    amm instruments a scenario lists in `liquidity`) at the same prices.
//! 3. At the end, it reads every fill the venues recorded for the bot, from
//!    the programs' own records (see the kit's meters), and audits them
//!    against the mandate's sides and limit prices.
//!
//! Nothing moves the market here but the bot and the scenario's actors (a
//! tape's takers, a pool's arbitrageur): the scenario has no path. A
//! session values the account at the snapshot's Pyth price throughout.

#[cfg(test)]
use std::time::Duration;
use std::{collections::HashSet, path::PathBuf, sync::Arc};

use async_trait::async_trait;
use mtx_core::{
    Decimal, Holding, Holdings, Side,
    mandate::{InstrumentRef, Kind, Mandate, Observed, ObservedFill, PositionRange, Term},
    turns::{self, BookLevels, Role, Step, Trade},
};
use mtx_kit::{
    BotEnv,
    chain::{ChainClient, RpcChainClient, token},
    protocols::{
        USDC_MINT, openbook::v2 as openbook_v2, phoenix::v1 as phoenix_v1, pyth, spot::MarketFill,
    },
};
use serde::{Deserialize, Serialize};
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_rpc_client::rpc_client::RpcClient;
use solana_signer::Signer;
use surfpool_sdk::{CheatcodeFilter, SimnetCommand, Surfnet};
use tracing::warn;

use crate::{
    Error, Result,
    actors::{Arbitrageur, Cast, Moment, Pricing, Takers},
    error::io_error,
    guard::Guard,
    making::TAKER_ACCOUNTS,
    orders::Traded,
    scenario::Scenario,
    session::{
        self, Account, BOT_USERS, Done, IxRecord, Prepared, SLOTS_PER_STEP, SessionDir,
        SessionOptions, SessionReport, StepRecord, Timeline, World, grader_f64,
    },
    snapshot,
    venue::{Market, books, price_usd, spot_venue, value_of_markets},
    wasm::{Desk, Reader},
};

/// The pair the book venues' profiles name.
pub const SYMBOL: &str = "SOL-USDC";
/// Venues a spot session can trade on: books, then pools.
pub const VENUES: [&str; 5] = ["phoenix", "openbook", "raydium", "meteora", "orca"];
/// The pool venues, whose markets offer liquidity too.
pub const POOLS: [&str; 3] = ["raydium", "meteora", "orca"];
/// SOL the bot's wallet holds for fees and account rent, on top of the
/// scenario's wrapped SOL. The profiles don't count it.
const FEE_SOL: f64 = 1.0;

/// A spot scenario's markets and starting wallet.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Spot {
    /// The venues whose spot market of the symbol the bot may trade: books
    /// (`phoenix`, `openbook`) for SOL-USDC, pools (`raydium`, `meteora`, `orca`)
    /// for SPCX-USDC. The mandate's universe is exactly these, and the
    /// amm instruments of `liquidity`.
    pub venues: Vec<String>,
    /// The pool venues where the bot may provide liquidity: each adds its
    /// amm instrument to the universe.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub liquidity: Vec<String>,
    /// An arbitrageur that keeps each pool's price near the reference.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arbitrage: Option<crate::actors::ArbitrageSpec>,
    /// The base asset the wallet starts with, in base units.
    pub base: f64,
    /// USDC the wallet starts with.
    pub quote: f64,
    /// What the reward divides the session's PnL by. Without it the reward
    /// is 0 and only `skill` against the reference is reported: a family
    /// that scores spot sessions sets it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bound_usd: Option<f64>,
}

/// What a spot scenario's step messages carry besides the clock: what a bot
/// can't cheaply read from the chain itself. Each is off unless asked for,
/// and a scenario without this section sends the step message it always did.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TurnData {
    /// Every trade on the venues since the previous step, the bot's own
    /// marked.
    #[serde(default)]
    pub tape: bool,
    /// The top this many price levels of each venue's book, per side. Zero
    /// sends none.
    #[serde(default)]
    pub depth_levels: usize,
    /// The world's clock, in Unix seconds.
    #[serde(default)]
    pub timestamps: bool,
    /// With the tape, the taker and maker of each trade.
    #[serde(default)]
    pub parties: bool,
}

/// The most price levels a step can carry per side.
pub const MAX_DEPTH_LEVELS: usize = 20;

impl Spot {
    /// The `symbol` spot markets, then the amm instruments, as the mandate
    /// names them.
    pub fn instruments(&self, symbol: &str) -> Vec<InstrumentRef> {
        let at = |venue: &String, kind| InstrumentRef {
            venue: venue.clone(),
            kind,
            symbol: symbol.into(),
        };
        self.venues
            .iter()
            .map(|venue| at(venue, Kind::Spot))
            .chain(self.liquidity.iter().map(|venue| at(venue, Kind::Amm)))
            .collect()
    }

    /// A spot scenario trades a pair its venues' profiles name, under a
    /// mandate whose universe is exactly those markets, and has no path or
    /// market making.
    pub fn validate(&self, scenario: &Scenario) -> std::result::Result<(), String> {
        if self.venues.is_empty() {
            return Err("spot: no venues".into());
        }
        for (i, venue) in self.venues.iter().enumerate() {
            if !VENUES.contains(&venue.as_str()) {
                return Err(format!(
                    "spot: unknown venue {venue:?}; spot sessions trade on {VENUES:?}"
                ));
            }
            if self.venues[..i].contains(venue) {
                return Err(format!("spot: {venue} is listed twice"));
            }
            let named = crate::venue::books::named_markets(venue).map_err(|e| e.to_string())?;
            if !named.iter().any(|m| m.symbol == scenario.symbol) {
                return Err(format!(
                    "spot: {venue} has no {} market; its profile names {:?}",
                    scenario.symbol,
                    named.iter().map(|m| m.symbol).collect::<Vec<_>>()
                ));
            }
        }
        for (i, venue) in self.liquidity.iter().enumerate() {
            if !POOLS.contains(&venue.as_str()) || !self.venues.contains(venue) {
                return Err(format!(
                    "spot: liquidity on {venue:?}: only a session's pool venues ({POOLS:?}) take liquidity"
                ));
            }
            if self.liquidity[..i].contains(venue) {
                return Err(format!("spot: liquidity on {venue} is listed twice"));
            }
        }
        if let Some(arbitrage) = &self.arbitrage {
            arbitrage.validate()?;
            if !self.venues.iter().any(|v| POOLS.contains(&v.as_str())) {
                return Err("spot: an arbitrageur trades pools, and the session has none".into());
            }
        }
        for (name, amount) in [("base", self.base), ("quote", self.quote)] {
            if !(amount.is_finite() && amount >= 0.0) {
                return Err(format!("spot: {name} must be a number of at least 0"));
            }
        }
        if self.bound_usd.is_some_and(|bound| !bound.is_finite()) {
            return Err("spot: bound_usd must be a number".into());
        }
        let mut wanted = self.instruments(&scenario.symbol);
        let mut universe: Vec<InstrumentRef> = scenario
            .mandate
            .universe
            .iter()
            .map(|a| a.instrument.clone())
            .collect();
        wanted.sort();
        universe.sort();
        if wanted != universe {
            return Err(format!(
                "mandate: the universe must be exactly the spot markets, {}",
                wanted
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if scenario.mandate.term.is_some() {
            return Err("mandate: a scenario's mandate has no term; the session sets it".into());
        }
        if !scenario.path.is_empty()
            || scenario.making.is_some()
            || scenario.offhours.is_some()
            || scenario.funding.is_some()
            || scenario.fair_path.is_some()
        {
            return Err(
                "spot: a spot scenario has no path, fair path, market making, off-hours or funding"
                    .into(),
            );
        }
        // A tape trades the session's first venue.
        scenario.validate_tape()?;
        if scenario.duration_slots == 0 {
            return Err("spot: the session has no slots".into());
        }
        if let Some(data) = &scenario.turn_data {
            if !(data.tape || data.depth_levels > 0 || data.timestamps)
                || (data.parties && !data.tape)
            {
                return Err("turn_data: asks for nothing, or for parties without a tape".into());
            }
            if data.depth_levels > MAX_DEPTH_LEVELS {
                return Err(format!(
                    "turn_data: depth_levels {} is over {MAX_DEPTH_LEVELS}",
                    data.depth_levels
                ));
            }
        }
        if let Some(order) = &scenario.order {
            order.validate()?;
            if order.side == Side::Sell && self.base < order.quantity {
                return Err(format!(
                    "order: selling {} SOL from a wallet of {}",
                    order.quantity, self.base
                ));
            }
        }
        if let Some(participation) = &scenario.participation {
            participation.validate()?;
        }
        if let Some(net) = &scenario.net {
            net.validate()?;
            if scenario.order.is_some() {
                return Err("net: a net-scored session has no order".into());
            }
        }
        Ok(())
    }
}

/// A spot session's account of the bot's wallet and trading.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct SpotReport {
    /// SOL and USDC held at the start and the end: wallet and venues.
    pub start: Wallet,
    pub end: Wallet,
    /// Every fill the venues recorded for the bot during the session.
    pub fills: Vec<SpotFill>,
    /// Each side the bot was seen resting an order on, per market.
    pub open_orders: Vec<(InstrumentRef, Side)>,
    /// Each side the bot's liquidity deposits and withdrawals moved base
    /// on, per amm market, which the mandate's sides rule counts as trades.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub liquidity_moves: Vec<(InstrumentRef, Side)>,
    /// For a scenario with a tape or a participation charge: each venue's
    /// traded base volume at each step, the bot's own fills included.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub volume: Vec<VenueVolume>,
    /// Every pool position the bot held, as last seen.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub pools: Vec<PoolPositionReport>,
    /// What the session's arbitrageur did, if it has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arbitrage: Option<crate::actors::ArbitrageReport>,
}

/// One of the bot's pool positions, as the session last read it.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PoolPositionReport {
    pub instrument: InstrumentRef,
    /// The venue's name for it.
    pub id: String,
    /// The first and last step it was seen at.
    pub first_step: Option<u64>,
    pub last_step: Option<u64>,
    /// What it held, its uncollected fees, and their value at the session's
    /// price (USDC at $1).
    pub base: f64,
    pub quote: f64,
    pub fees_base: f64,
    pub fees_quote: f64,
    pub value_usd: f64,
    /// Whether the bot still held it at the end.
    pub at_end: bool,
}

/// One venue's traded base volume, step by step.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct VenueVolume {
    pub venue: String,
    pub per_step: Vec<f64>,
}

impl SpotReport {
    /// The fills, as the mandate's audit reads them.
    pub fn observed_fills(&self) -> Vec<ObservedFill> {
        self.fills
            .iter()
            .map(|fill| ObservedFill {
                instrument: fill.instrument.clone(),
                side: fill.side,
                price: fill.price,
            })
            .collect()
    }
}

/// What the account holds, and what it's worth.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct Wallet {
    /// SOL and USDC, in the wallet and on every venue, free or locked.
    pub base: f64,
    pub quote: f64,
    /// The Pyth SOL/USD price it was valued at, and its value.
    pub price_usd: f64,
    pub value_usd: f64,
}

/// One of the bot's fills.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SpotFill {
    /// Slots into the session it landed at (or, waiting in OpenBook's event
    /// heap, was read at).
    pub at: u64,
    pub instrument: InstrumentRef,
    /// The bot's side, and whether it took or made.
    pub side: Side,
    pub role: &'static str,
    /// Price in USDC per SOL, before fees, and size in SOL.
    pub price: f64,
    pub base: f64,
}

/// The session's markets, read through the grader's own client for the
/// bot's account, in the profiles' order (Phoenix first), and the assets
/// they trade.
struct Venues {
    chain: Arc<dyn ChainClient>,
    owner: Pubkey,
    markets: Vec<Box<dyn Market>>,
}

/// One reading of the account.
struct Reading {
    wallet: Wallet,
    open_orders: Vec<(InstrumentRef, Side)>,
    /// Each amm instrument's position: the base its pool positions hold.
    pooled: Vec<(InstrumentRef, f64)>,
    /// The pool positions, as the amm profile holds them.
    positions: Vec<(InstrumentRef, mtx_core::PoolPosition)>,
}

impl Reading {
    /// The position on `instrument`: on an amm instrument the base its
    /// positions hold; on a spot one the base held, in the wallet and on
    /// the venues, and in every pool position too (one base-asset
    /// position, wherever it sits).
    fn position(&self, instrument: &InstrumentRef) -> f64 {
        match instrument.kind {
            Kind::Amm => self
                .pooled
                .iter()
                .find(|(i, _)| i == instrument)
                .map_or(0.0, |(_, held)| *held),
            _ => self.wallet.base + self.pooled.iter().map(|(_, held)| held).sum::<f64>(),
        }
    }
}

impl Venues {
    /// The `symbol` spot market of each of `venues`, in the profiles' order
    /// (books first), then the amm instrument of each of `liquidity`, as
    /// their venues offer them.
    async fn connect(
        chain: Arc<dyn ChainClient>,
        rpc_url: &str,
        spot: &Spot,
        symbol: &str,
        owner: Pubkey,
    ) -> Result<Self> {
        let mut markets = Vec::new();
        for (listed, kind) in [(&spot.venues, Kind::Spot), (&spot.liquidity, Kind::Amm)] {
            for venue in VENUES.iter().filter(|v| listed.iter().any(|w| w == *v)) {
                let instrument = InstrumentRef {
                    venue: (*venue).into(),
                    kind,
                    symbol: symbol.into(),
                };
                markets.push(spot_venue(venue, rpc_url)?.market(&instrument).await?);
            }
        }
        Ok(Self {
            chain,
            owner,
            markets,
        })
    }

    /// The spot market of `venue`.
    fn market(&self, venue: &str) -> &dyn Market {
        self.markets
            .iter()
            .find(|m| m.instrument().venue == venue && m.kind() == Kind::Spot)
            .expect("a market on each of the session's venues")
            .as_ref()
    }

    /// The market of `instrument`.
    fn of(&self, instrument: &InstrumentRef) -> &dyn Market {
        self.markets
            .iter()
            .find(|m| m.instrument() == instrument)
            .expect("a market for each of the session's instruments")
            .as_ref()
    }

    /// The account across the markets: each asset counted once in the
    /// wallet and on every venue (the spot profile's position), and valued
    /// at the reference prices, the base asset's also the session's price.
    async fn read(&self) -> Result<Reading> {
        let chain = self.chain.as_ref();
        let mut all: Vec<Holding> = Vec::new();
        let mut open_orders = Vec::new();
        let mut pooled = Vec::new();
        let mut positions = Vec::new();
        let mut per_market: Vec<(&dyn Market, Vec<Holding>)> = Vec::new();
        for market in &self.markets {
            let held = market.holdings(&self.owner).await?;
            if market.kind() == Kind::Amm {
                let instrument = market.instrument();
                pooled.push((
                    instrument.clone(),
                    grader_f64(held.iter().map(Holding::position).sum()),
                ));
                positions.extend(held.iter().filter_map(|h| match h {
                    Holding::Amm(p) => Some((instrument.clone(), p.clone())),
                    _ => None,
                }));
            }
            // An ask locks the base, a bid the quote.
            for (holding, side) in held.iter().zip([Side::Sell, Side::Buy]) {
                if let Holding::Spot(asset) = holding
                    && asset.locked > Decimal::ZERO
                {
                    open_orders.push((market.instrument().clone(), side));
                }
            }
            all.extend(held.iter().cloned());
            per_market.push((market.as_ref(), held));
        }
        let value = value_of_markets(chain, &per_market).await?;
        let asset = |i: usize| -> Option<&Holdings> {
            match all.get(i)? {
                Holding::Spot(asset) => Some(asset),
                _ => None,
            }
        };
        let (Some(base), Some(quote)) = (asset(0), asset(1)) else {
            return Err(Error::UnknownMarket(
                "a spot session's markets hold no base and quote".into(),
            ));
        };
        let price = price_usd(
            chain,
            &base
                .base
                .id
                .0
                .parse()
                .map_err(|_| Error::UnknownMarket(format!("{} is not a mint", base.base.id)))?,
        )
        .await?;
        // The wallet is the same on every venue: count it once.
        let total = |of: &Holdings| -> Decimal {
            of.wallet
                + all
                    .iter()
                    .filter_map(|h| match h {
                        Holding::Spot(asset) if asset.base.id == of.base.id => {
                            Some(asset.free + asset.locked)
                        }
                        _ => None,
                    })
                    .sum::<Decimal>()
        };
        Ok(Reading {
            wallet: Wallet {
                base: grader_f64(total(base)),
                quote: grader_f64(total(quote)),
                price_usd: grader_f64(price),
                value_usd: grader_f64(value),
            },
            open_orders,
            pooled,
            positions,
        })
    }

    /// Each side the bot's liquidity moved base on, per amm market, in
    /// slots after `after_slot`.
    async fn liquidity_moves(&self, after_slot: u64) -> Result<Vec<(InstrumentRef, Side)>> {
        let mut moves = Vec::new();
        for market in self.markets.iter().filter(|m| m.kind() == Kind::Amm) {
            for side in market.liquidity_moves(after_slot, &self.owner).await? {
                moves.push((market.instrument().clone(), side));
            }
        }
        Ok(moves)
    }

    /// Add to the ledger every match the venues recorded in slots after
    /// `after_slot` that it hasn't got, oldest first, and return the index
    /// of the first new one.
    async fn scan(&self, ledger: &mut Ledger, after_slot: u64) -> Result<usize> {
        let first = ledger.fills.len();
        let mut found: Vec<(MarketFill, Vec<Pubkey>)> = Vec::new();
        for market in &self.markets {
            found.extend(market.fills(after_slot, &self.owner).await?);
        }
        for (fill, ids) in found {
            if ledger
                .seen
                .insert((fill.instrument.0.clone(), fill.sequence))
            {
                let own = fill.side_for(&ids).map(|side| {
                    let role = if ids.contains(&fill.taker) {
                        Role::Taker
                    } else {
                        Role::Maker
                    };
                    (side, role)
                });
                ledger.fills.push(Seen { fill, own });
            }
        }
        ledger.fills[first..]
            .sort_by_key(|s| (s.fill.slot, s.fill.instrument.0.clone(), s.fill.sequence));
        Ok(first)
    }

    /// The top `levels` price levels of each venue's book, per side, at
    /// `slot`.
    async fn depth(
        &self,
        levels: usize,
        slot: u64,
        unix_timestamp: i64,
    ) -> Result<Vec<BookLevels>> {
        let mut out = Vec::new();
        for market in &self.markets {
            out.extend(market.depth(levels, slot, unix_timestamp).await?);
        }
        Ok(out)
    }
}

/// A match a venue recorded, and which side of it the bot was.
struct Seen {
    fill: MarketFill,
    own: Option<(Side, Role)>,
}

/// Every match the venues recorded in the session, once each, as first
/// seen. A session scans once at the end, or at every step when it reports
/// a tape or prices participation, so that a fill waiting in OpenBook's
/// event heap is dated to the step it first showed in.
#[derive(Default)]
struct Ledger {
    seen: HashSet<(String, u128)>,
    fills: Vec<Seen>,
    /// How many entries were there before the session began: matches the
    /// event heap still held from the snapshot. They are not the session's.
    before: usize,
}

impl Ledger {
    /// The bot's own fills, oldest first.
    fn own(&self, base_slot: u64) -> Result<Vec<SpotFill>> {
        let mut own: Vec<&Seen> = self.fills[self.before..]
            .iter()
            .filter(|s| s.own.is_some())
            .collect();
        own.sort_by_key(|s| (s.fill.slot, s.fill.instrument.0.clone(), s.fill.sequence));
        own.into_iter()
            .map(|Seen { fill, own }| {
                let (side, role) = own.expect("filtered to the bot's own");
                Ok(SpotFill {
                    at: fill.slot.saturating_sub(base_slot),
                    instrument: fill.instrument.0.parse().map_err(|reason: String| {
                        Error::Transaction {
                            label: "a fill's market".into(),
                            reason,
                        }
                    })?,
                    side,
                    role: match role {
                        Role::Taker => "taker",
                        Role::Maker => "maker",
                    },
                    price: grader_f64(fill.price),
                    base: grader_f64(fill.quantity),
                })
            })
            .collect()
    }

    /// Every match as a trade for the participation charge.
    fn traded(&self, base_slot: u64) -> Vec<Traded> {
        self.fills[self.before..]
            .iter()
            .map(|s| Traded {
                venue: venue_of(&s.fill),
                price: grader_f64(s.fill.price),
                slot: s.fill.slot.saturating_sub(base_slot),
                base: grader_f64(s.fill.quantity),
                own: s.own.is_some(),
            })
            .collect()
    }

    /// Each venue's traded base volume at each of `steps` steps.
    fn volume(&self, venues: &[String], base_slot: u64, steps: u64) -> Vec<VenueVolume> {
        venues
            .iter()
            .map(|venue| {
                let mut per_step = vec![0.0; steps as usize];
                for seen in self.fills[self.before..]
                    .iter()
                    .filter(|s| &venue_of(&s.fill) == venue)
                {
                    let at = seen.fill.slot.saturating_sub(base_slot);
                    let step = (at.saturating_sub(1) / SLOTS_PER_STEP).min(steps.saturating_sub(1));
                    per_step[step as usize] += grader_f64(seen.fill.quantity);
                }
                VenueVolume {
                    venue: venue.clone(),
                    per_step,
                }
            })
            .collect()
    }
}

fn venue_of(fill: &MarketFill) -> String {
    fill.instrument
        .0
        .split(':')
        .next()
        .unwrap_or_default()
        .to_string()
}

/// A ledger entry as a step's tape shows it.
fn trade(seen: &Seen, parties: bool) -> Trade {
    Trade {
        venue: venue_of(&seen.fill),
        market: Some(seen.fill.instrument.0.clone()),
        side: seen.fill.taker_side,
        price: grader_f64(seen.fill.price),
        base: grader_f64(seen.fill.quantity),
        slot: seen.fill.slot,
        taker: parties.then(|| seen.fill.taker.to_string()),
        maker: parties.then(|| seen.fill.maker.to_string()),
        own: seen.own.map(|(_, role)| role),
    }
}

fn venue_error<E: std::fmt::Display>(what: &'static str) -> impl Fn(E) -> Error {
    move |error| Error::Transaction {
        label: what.into(),
        reason: error.to_string(),
    }
}

/// Give `owner` `base` units of `symbol`'s base asset and `quote` USDC, and
/// SOL for fees: wrapped SOL by [`fund`], any other base by cheatcode
/// ([`fund_mint`]), into its associated account under its token program.
/// With `accounts` the wallet has a base account even when it starts with
/// none (a pool's seat creates any it lacks as it trades).
pub fn fund_pair(
    surfnet: &Surfnet,
    rpc: &RpcClient,
    owner: &Keypair,
    base_mint: &Pubkey,
    base: f64,
    quote: f64,
    accounts: bool,
) -> Result<()> {
    if *base_mint == token::native_mint() {
        return fund(surfnet, rpc, owner, base, quote, accounts);
    }
    let cheats = surfnet.cheatcodes();
    cheats.fund_sol(&owner.pubkey(), (FEE_SOL * 1e9) as u64)?;
    cheats.fund_token(
        &owner.pubkey(),
        &USDC_MINT,
        (quote * 1e6).round() as u64,
        None,
    )?;
    let account = crate::chain::rpc_client(surfnet).get_account(base_mint)?;
    let decimals = mtx_kit::token::Mint::decode(&account.owner, &account.data)
        .map_err(venue_error("mint"))?
        .decimals;
    let atoms = (base * 10f64.powi(i32::from(decimals))).round() as u64;
    if atoms > 0 || accounts {
        fund_mint(surfnet, &owner.pubkey(), base_mint, atoms)?;
    }
    Ok(())
}

/// Give `owner` `base` wrapped SOL and `quote` USDC, and SOL for fees. Setup
/// only: USDC comes from a cheatcode; the SOL is wrapped by an ordinary
/// transaction, so its token account is a real native one. With `accounts`
/// the wallet has both token accounts even when it starts with none of an
/// asset, as a bot trading toward an order needs (it buys into the wSOL
/// account, or sells into the USDC one).
pub fn fund(
    surfnet: &Surfnet,
    rpc: &RpcClient,
    owner: &Keypair,
    base: f64,
    quote: f64,
    accounts: bool,
) -> Result<()> {
    let cheats = surfnet.cheatcodes();
    let wrapped = (base * 1e9).round() as u64;
    let fees = (FEE_SOL * 1e9) as u64;
    cheats.fund_sol(&owner.pubkey(), fees + wrapped)?;
    cheats.fund_token(
        &owner.pubkey(),
        &USDC_MINT,
        (quote * 1e6).round() as u64,
        None,
    )?;
    if wrapped > 0 || accounts {
        let (_, mut instructions) = token::wrap_sol(&owner.pubkey(), wrapped, true);
        if accounts {
            instructions.insert(
                0,
                token::create_ata_idempotent(&owner.pubkey(), &owner.pubkey(), &USDC_MINT),
            );
        }
        mtx_kit::send(rpc, owner, instructions, "wrap SOL")?;
    }
    Ok(())
}

/// Give `owner` `atoms` of `mint` in its associated token account, under
/// whichever token program owns the mint (SPL Token or Token-2022). Setup
/// only, by cheatcode. Refuses a mint the kit can't move
/// ([`Mint::unsupported`](mtx_kit::token::Mint::unsupported)): a token a bot
/// is funded with must be one it can trade. Returns the mint, as read.
pub fn fund_mint(
    surfnet: &Surfnet,
    owner: &Pubkey,
    mint: &Pubkey,
    atoms: u64,
) -> Result<mtx_kit::token::Mint> {
    let account = crate::chain::rpc_client(surfnet).get_account(mint)?;
    let decoded =
        mtx_kit::token::Mint::decode(&account.owner, &account.data).map_err(venue_error("mint"))?;
    if let Some(why) = decoded.unsupported() {
        return Err(Error::Transaction {
            label: format!("funding {mint}"),
            reason: format!("the kit can't trade this token: {why}"),
        });
    }
    surfnet
        .cheatcodes()
        .fund_token(owner, mint, atoms, Some(&decoded.program))?;
    Ok(decoded)
}

/// Trade a little on each venue as `trader` would: a Phoenix seat, a resting
/// bid and its cancel, a small sell; an OpenBook open orders account, the
/// same. Returns the Pyth SOL/USD price. For a snapshot recipe's `spot-exercise` action ([`crate::recipe`]):
/// every account these touch is then in the surfnet, and so in the export.
pub async fn exercise(rpc_url: &str, trader: &Keypair) -> Result<f64> {
    use mtx_kit::protocols::{
        openbook::v2::{OpenBookV2Order, OpenBookV2Trader},
        phoenix::v1::{PhoenixV1Order, PhoenixV1Trader},
    };

    let chain: Arc<dyn ChainClient> = Arc::new(RpcChainClient::new(rpc_url));
    let owner = trader.pubkey();
    let price = pyth::read_price(chain.as_ref(), &pyth::SOL_USD_FEED_ID)
        .await
        .map_err(venue_error("pyth SOL/USD"))?;
    let tenth = Decimal::new(1, 1);
    let hundredth = Decimal::new(1, 2);
    let on_grid = |price: Decimal, tick: Decimal| (price / tick).floor() * tick;

    let named = phoenix_v1::MARKETS[0];
    let phoenix = PhoenixV1Trader::connect(chain.clone(), named.address, "SOL", "USDC", owner)
        .await
        .map_err(venue_error("phoenix v1 market"))?;
    let tick = phoenix.market().instrument.tick_size;
    let steps = [
        ("phoenix v1 claim seat", phoenix.plan_claim_seat()),
        (
            "phoenix v1 resting bid",
            phoenix.plan(&PhoenixV1Order::PostOnly {
                side: Side::Buy,
                price: on_grid(price / Decimal::TWO, tick),
                quantity: tenth,
                amend_if_crossing: false,
                last_valid_slot: None,
            }),
        ),
        ("phoenix v1 cancel", phoenix.plan_cancel_all()),
    ];
    for (label, plan) in steps {
        let plan = plan.map_err(venue_error(label))?;
        phoenix
            .send(&plan, trader)
            .await
            .map_err(venue_error(label))?;
    }
    let sell = PhoenixV1Order::ImmediateOrCancel {
        side: Side::Sell,
        quantity: hundredth,
        worst_price: None,
        all_or_none: false,
    };
    if let Err(error) = phoenix.submit(&sell, trader).await {
        warn!(%error, "phoenix v1 warmup sell didn't go through");
    }

    let named = openbook_v2::MARKETS[0];
    let openbook = OpenBookV2Trader::connect(chain.clone(), named.address, "SOL", "USDC", owner)
        .await
        .map_err(venue_error("openbook v2 market"))?;
    let tick = openbook.market().instrument.tick_size;
    let steps = [
        ("openbook v2 open account", openbook.plan_open_account()),
        (
            "openbook v2 resting bid",
            openbook.plan(&OpenBookV2Order::PostOnly {
                side: Side::Buy,
                price: on_grid(price / Decimal::TWO, tick),
                quantity: tenth,
                expiry_timestamp: 0,
            }),
        ),
        ("openbook v2 cancel", openbook.plan_cancel_all()),
    ];
    for (label, plan) in steps {
        let plan = plan.map_err(venue_error(label))?;
        openbook
            .send(&plan, trader)
            .await
            .map_err(venue_error(label))?;
    }
    let take = OpenBookV2Order::Take {
        side: Side::Sell,
        quantity: hundredth,
        worst_price: tick,
    };
    if let Err(error) = openbook.submit(&take, trader).await {
        warn!(%error, "openbook v2 warmup sell didn't go through");
    }
    Ok(grader_f64(price))
}

/// Trade each pool the pool venues' profiles name as `trader` would, and
/// read every tick and bin array that holds liquidity: a sale, a purchase
/// and a taker's swap to a price on each, then a position opened, half
/// taken out, and the rest, each transaction confirmed. Returns the base
/// asset's Pyth price. For a snapshot recipe's `pools-exercise` action
/// ([`crate::recipe`]): every account these touch, and every array a
/// session's swaps may cross, is then in the surfnet, and so in the export.
/// `trader` must hold the pools' assets.
pub async fn exercise_pools(rpc_url: &str, trader: &Keypair) -> Result<f64> {
    use mtx_kit::{AmountUnit, PriceRange, Side as KitSide};

    use crate::venue::pools::{self, Deposit, SwapOrder};

    let chain: Arc<dyn ChainClient> = Arc::new(RpcChainClient::new(rpc_url));
    let rpc = mtx_kit::rpc_client(rpc_url);
    let owner = trader.pubkey();
    let mut price = None;
    for venue in POOLS {
        for named in crate::venue::books::named_markets(venue)? {
            load_arrays(chain.as_ref(), venue, &named.address).await?;
            let pool = pools::connect(venue, chain.as_ref(), named).await?;
            let label = |what: &str| format!("{venue}:{} {what}", named.symbol);
            let refused = |what: &str| {
                let label = label(what);
                move |reason: String| Error::Transaction { label, reason }
            };
            let mid = pool.price(chain.as_ref()).await?.price;
            let assets = pool.assets();
            let mut accounts = Vec::new();
            for (mint, program) in [
                (assets.base_mint, assets.base_program),
                (assets.quote_mint, assets.quote_program),
            ] {
                accounts.push(token::create_ata_idempotent_for(
                    &owner, &owner, &mint, &program,
                ));
            }
            send_signed(&rpc, trader, None, accounts, &label("token accounts"))?;
            for (what, order) in [
                (
                    "sale",
                    SwapOrder {
                        side: KitSide::Ask,
                        amount: 0.05,
                        unit: AmountUnit::Base,
                        limit: mid * 0.9,
                        partial: false,
                    },
                ),
                (
                    "purchase",
                    SwapOrder {
                        side: KitSide::Bid,
                        amount: 5.0,
                        unit: AmountUnit::Quote,
                        limit: mid * 1.1,
                        partial: false,
                    },
                ),
                (
                    "taker's sale",
                    SwapOrder {
                        side: KitSide::Ask,
                        amount: 0.05,
                        unit: AmountUnit::Base,
                        limit: mid * 0.95,
                        partial: true,
                    },
                ),
            ] {
                let swap = pool
                    .swap(chain.as_ref(), &owner, &order)
                    .await
                    .map_err(refused(what))?;
                send_signed(&rpc, trader, None, swap, &label(what))?;
            }
            let new = Keypair::new();
            let deposit = Deposit {
                base: 0.05,
                quote: 5.0,
                range: Some(PriceRange {
                    lower: mid * 0.97,
                    upper: mid * 1.03,
                }),
                shape: None,
            };
            let (opened, _) = pool
                .deposit(chain.as_ref(), &owner, None, &new.pubkey(), &deposit)
                .await
                .map_err(refused("deposit"))?;
            send_signed(&rpc, trader, Some(&new), opened, &label("deposit"))?;
            for share in [0.5, 1.0] {
                let held = pool
                    .position(chain.as_ref(), &owner, &new.pubkey())
                    .await?
                    .filter(|lp| lp.live)
                    .ok_or_else(|| refused("withdrawal")("the position isn't there".into()))?;
                let out = pool
                    .withdraw(chain.as_ref(), &owner, &held, share, None, None)
                    .await
                    .map_err(refused("withdrawal"))?;
                send_signed(&rpc, trader, None, out, &label("withdrawal"))?;
            }
            if pool
                .position(chain.as_ref(), &owner, &new.pubkey())
                .await?
                .is_some_and(|lp| lp.live)
            {
                return Err(refused("withdrawal")(
                    "the position still holds liquidity".into(),
                ));
            }
            if price.is_none() {
                price = Some(grader_f64(
                    price_usd(chain.as_ref(), &named.base_mint).await?,
                ));
            }
        }
    }
    price.ok_or_else(|| Error::UnknownMarket("no pools to exercise".into()))
}

/// Simulate `instructions` as one transaction (its logs say why it would
/// fail), then sign it, by `payer` and `extra`, send it and wait for it.
fn send_signed(
    rpc: &RpcClient,
    payer: &Keypair,
    extra: Option<&Keypair>,
    instructions: Vec<mtx_kit::Instruction>,
    label: &str,
) -> Result<()> {
    use mtx_kit::chain::{InstructionGroup, TransactionPlan};

    let plan = TransactionPlan::from_group(&InstructionGroup::new(label, instructions), 1_000_000);
    let blockhash = rpc.get_latest_blockhash()?;
    let mut tx = plan.transaction(&payer.pubkey(), blockhash);
    let mut signers = vec![payer];
    signers.extend(extra);
    tx.try_sign(&signers, blockhash)
        .map_err(venue_error("signing"))?;
    let simulation = rpc.simulate_transaction(&tx)?.value;
    if let Some(error) = simulation.err {
        let logs = simulation.logs.unwrap_or_default();
        return Err(Error::Transaction {
            label: label.into(),
            reason: format!(
                "{error}\n{}",
                logs[logs.len().saturating_sub(12)..].join("\n")
            ),
        });
    }
    rpc.send_and_confirm_transaction(&tx)?;
    Ok(())
}

/// Read every tick array (Raydium, Orca) or bin array (Meteora) its pool's bitmap
/// marks, and the pool's bitmap extension: a fork loads what is read.
async fn load_arrays(chain: &dyn ChainClient, venue: &str, pool: &Pubkey) -> Result<()> {
    use mtx_kit::protocols::{meteora::dlmm, orca::whirlpool as orca, raydium::clmm};

    let account = chain
        .get_account(pool)
        .await
        .map_err(venue_error("a pool"))?
        .ok_or_else(|| Error::UnknownMarket(format!("pool {pool} doesn't exist")))?;
    let mut addresses = Vec::new();
    match venue {
        "raydium" => {
            let state =
                clmm::PoolState::decode(*pool, &account.data).map_err(venue_error("a pool"))?;
            let span = clmm::TICK_ARRAY_SIZE * i32::from(state.tick_spacing);
            addresses.push(clmm::bitmap_extension_address(pool));
            addresses.push(state.observation);
            addresses.push(state.amm_config);
            for bit in 0..1024i32 {
                let start = (bit - 512) * span;
                if state.tick_array_initialized(start) == Some(true) {
                    addresses.push(clmm::tick_array_address(pool, start));
                }
            }
        }
        "orca" => {
            let state =
                orca::PoolState::decode(*pool, &account.data).map_err(venue_error("a pool"))?;
            let span = orca::TICK_ARRAY_SIZE * i32::from(state.tick_spacing);
            addresses.push(orca::oracle_address(pool));
            // The pool keeps no record of which tick arrays exist: ask for
            // each place one could be.
            for index in
                orca::math::MIN_TICK.div_euclid(span)..=orca::math::MAX_TICK.div_euclid(span)
            {
                addresses.push(orca::tick_array_address(pool, index * span));
            }
        }
        _ => {
            let pair = dlmm::LbPair::decode(*pool, &account.data).map_err(venue_error("a pool"))?;
            addresses.push(dlmm::bitmap_extension_address(pool));
            addresses.push(pair.oracle);
            for index in -512..512i64 {
                if pair.bin_array_has_liquidity(index) == Some(true) {
                    addresses.push(dlmm::bin_array_address(pool, index));
                }
            }
        }
    }
    for address in addresses {
        chain
            .get_account(&address)
            .await
            .map_err(venue_error("a pool's array"))?;
    }
    Ok(())
}

/// The base mint of `symbol` on the session's first venue, as its profile
/// names it.
fn base_mint(spot: &Spot, symbol: &str) -> Result<Pubkey> {
    let venue = spot
        .venues
        .first()
        .ok_or_else(|| Error::UnknownMarket("a spot session with no venue".into()))?;
    crate::venue::books::named_markets(venue)?
        .iter()
        .find(|m| m.symbol == symbol)
        .map(|m| m.base_mint)
        .ok_or_else(|| Error::UnknownMarket(format!("{venue}:spot:{symbol}")))
}

/// Each spot taker's wallet: far more than its orders need, so none fails
/// for funds.
const TAKER_BASE: f64 = 10_000.0;
const TAKER_QUOTE: f64 = 2_000_000.0;

/// Set a spot session up, up to the bot's start; see the module docs and
/// [`session::run`].
pub(crate) async fn prepare(opts: &SessionOptions<'_>) -> Result<Prepared> {
    let scenario = opts.scenario;
    let spot = scenario
        .spot
        .as_ref()
        .expect("a spot session's scenario has a spot section");
    let snapshot_dir = opts
        .snapshot_dir
        .canonicalize()
        .map_err(io_error(opts.snapshot_dir))?;
    let (surfnet, _) = snapshot::start_snapshot_stepped(&snapshot_dir).await?;
    let internal = Guard::internal(surfnet.rpc_url()).await?;
    let rpc = mtx_kit::rpc_client_with_timeout(internal.url(), session::INTERNAL_RPC_TIMEOUT);
    let setup_from = rpc.get_slot()?;
    let chain: Arc<dyn ChainClient> = Arc::new(RpcChainClient::with_timeout(
        internal.url(),
        session::INTERNAL_RPC_TIMEOUT,
    ));

    let bot = opts.keys.keypair("bot", 0);
    let base_mint = base_mint(spot, &scenario.symbol)?;
    fund_pair(
        &surfnet,
        &rpc,
        &bot,
        &base_mint,
        spot.base,
        spot.quote,
        scenario.order.is_some(),
    )?;
    let venues = Venues::connect(
        chain.clone(),
        internal.url(),
        spot,
        &scenario.symbol,
        bot.pubkey(),
    )
    .await?;
    let start = venues.read().await?;
    let instruments = spot.instruments(&scenario.symbol);
    // A tape's takers, each a funded wallet trading the session's first
    // venue.
    let mut cast = Cast::default();
    if !scenario.tape.is_empty() {
        let first = venues.market(&spot.venues[0]);
        let mut seats = Vec::new();
        for i in 0..TAKER_ACCOUNTS {
            let keypair = opts.keys.keypair("taker", u32::from(i));
            fund_pair(
                &surfnet,
                &rpc,
                &keypair,
                &base_mint,
                TAKER_BASE,
                TAKER_QUOTE,
                true,
            )?;
            seats.push(first.seat(keypair, internal.url()).await?);
        }
        cast.add(Box::new(Takers::new(
            seats,
            scenario.tape.clone(),
            Pricing::Book,
            0,
        )));
    }
    // An arbitrageur on each pool, each pool its own account's.
    if let Some(spec) = &spot.arbitrage {
        let mut seats = Vec::new();
        for (i, venue) in spot
            .venues
            .iter()
            .filter(|v| POOLS.contains(&v.as_str()))
            .enumerate()
        {
            let keypair = opts.keys.keypair("arbitrageur", i as u32);
            fund_pair(
                &surfnet, &rpc, &keypair, &base_mint, spec.base, spec.quote, true,
            )?;
            seats.push(venues.market(venue).seat(keypair, internal.url()).await?);
        }
        cast.add(Box::new(Arbitrageur::new(seats, *spec)));
    }

    let guard = Guard::start(surfnet.rpc_url()).await?;
    let workdir = SessionDir::create(&bot, BOT_USERS[opts.slot])?;
    let mut slot = rpc.get_slot()?;
    let first_slot = setup_from + scenario.setup_slots();
    if slot > first_slot {
        return Err(Error::SetupSlots {
            scenario: scenario.name.clone(),
            used: slot - setup_from,
            budget: scenario.setup_slots(),
        });
    }
    while slot < first_slot {
        surfnet.send_command(SimnetCommand::SlotForward(None))?;
        slot = session::wait_for_slot(&rpc, slot + 1).await?;
    }
    surfnet.cheatcodes().lock(CheatcodeFilter::all())?;
    let base_slot = rpc.get_slot()?;
    let end_slot = base_slot + scenario.duration_slots;
    let mandate = Mandate {
        term: Some(Term {
            until_slot: end_slot,
        }),
        ..scenario.mandate.clone()
    };
    let env = BotEnv {
        rpc_url: guard.url().to_string(),
        keypair_path: workdir.keypair_path(),
        // No Phoenix Eternal exchange in a spot session: the markets are the
        // profiles' named ones, which the mandate lists.
        exchange_path: PathBuf::new(),
        symbol: scenario.symbol.clone(),
        end_slot,
        max_position: scenario.max_position(),
        // Without a drawdown limit, the most the account could lose.
        max_drawdown_usd: mandate
            .limits
            .max_drawdown_usd
            .unwrap_or(start.wallet.value_usd),
        mandate_path: Some(workdir.write_mandate(&mandate)?),
    };
    // What the steps carry beyond the clock, for either kind of bot.
    let reports = scenario.turn_data.as_ref().map(|data| turns::Reports {
        tape: data.tape,
        depth: data.depth_levels > 0,
        timestamps: data.timestamps,
    });
    // Every spot market bounds the same base, held from the start; each
    // amm instrument, what its positions hold.
    let mut observed = Observed::default();
    for instrument in &instruments {
        observed.begin(instrument, start.position(instrument));
    }
    let account = SpotAccount {
        scenario: scenario.clone(),
        spot: spot.clone(),
        venues,
        instruments,
        base_slot,
        steps: scenario.duration_slots.div_ceil(SLOTS_PER_STEP),
        observed,
        open_orders: Vec::new(),
        pools: Vec::new(),
        start_value_usd: start.wallet.value_usd,
        start_price_usd: start.wallet.price_usd,
        peak_value_usd: start.wallet.value_usd,
        max_drawdown_usd: 0.0,
        // Scan the venues at every step only when the scenario reports a
        // tape or prices participation; otherwise once, at the end.
        tracking: scenario.turn_data.as_ref().is_some_and(|data| data.tape)
            || scenario.participation.is_some(),
        ledger: Ledger::default(),
        last_scan: base_slot,
        first_new: 0,
        start,
        last: None,
    };
    Ok(Prepared {
        world: World::Snapshot(Box::new(surfnet)),
        internal,
        rpc,
        guard,
        workdir,
        bot,
        base_slot,
        end_slot,
        mandate,
        env,
        reports,
        reopen_slot: None,
        cast,
        account: Box::new(account),
    })
}

/// A spot session's account: the bot's wallet, and what each venue holds
/// for it, valued at the Pyth SOL/USD price.
struct SpotAccount {
    scenario: Scenario,
    spot: Spot,
    venues: Venues,
    instruments: Vec<InstrumentRef>,
    base_slot: u64,
    steps: u64,
    observed: Observed,
    /// Every market and side the bot had orders resting on, at any step.
    open_orders: Vec<(InstrumentRef, Side)>,
    /// Every pool position the bot held at any step.
    pools: Vec<PoolPositionReport>,
    start: Reading,
    start_value_usd: f64,
    start_price_usd: f64,
    peak_value_usd: f64,
    max_drawdown_usd: f64,
    tracking: bool,
    ledger: Ledger,
    last_scan: u64,
    /// The first of the ledger's entries the last step found.
    first_new: usize,
    /// The last step's reading.
    last: Option<Reading>,
}

impl SpotAccount {
    /// What `reading` shows at step `k` (`None` at the end).
    fn note(&mut self, reading: &Reading, k: Option<u64>) {
        for instrument in &self.instruments {
            self.observed
                .record(instrument, reading.position(instrument));
        }
        for (instrument, position) in &reading.positions {
            let value_usd = grader_f64(
                (position.base_amount + position.fees_base)
                    * Decimal::try_from(reading.wallet.price_usd).unwrap_or_default()
                    + position.quote_amount
                    + position.fees_quote,
            );
            let seen = PoolPositionReport {
                instrument: instrument.clone(),
                id: position.id.clone(),
                first_step: k,
                last_step: k,
                base: grader_f64(position.base_amount),
                quote: grader_f64(position.quote_amount),
                fees_base: grader_f64(position.fees_base),
                fees_quote: grader_f64(position.fees_quote),
                value_usd,
                at_end: k.is_none(),
            };
            match self.pools.iter_mut().find(|p| p.id == seen.id) {
                Some(known) => {
                    *known = PoolPositionReport {
                        first_step: known.first_step,
                        last_step: k.or(known.last_step),
                        ..seen
                    }
                }
                None => self.pools.push(seen),
            }
        }
        for (instrument, side) in &reading.open_orders {
            self.observed.record_open_order(instrument, *side);
            if !self.open_orders.contains(&(instrument.clone(), *side)) {
                self.open_orders.push((instrument.clone(), *side));
            }
        }
    }
}

#[async_trait]
impl Account for SpotAccount {
    fn start_value_usd(&self) -> f64 {
        self.start_value_usd
    }

    fn moment(&self, at: u64) -> Moment {
        // Nothing moves a spot market but its traders: the session's price
        // is where it started.
        Moment {
            at,
            level_usd: self.start_price_usd,
            book_usd: self.start_price_usd,
        }
    }

    async fn begin(&mut self) -> Result<()> {
        if self.tracking || self.scenario.turn_data.is_some() {
            self.venues
                .scan(&mut self.ledger, self.base_slot.saturating_sub(1))
                .await?;
            self.ledger.before = self.ledger.fills.len();
        }
        Ok(())
    }

    async fn step(&mut self, k: u64, slot: u64, at: u64) -> Result<StepRecord> {
        let now = self.venues.read().await?;
        self.note(&now, Some(k));
        self.first_new = if self.tracking {
            let first = self
                .venues
                .scan(&mut self.ledger, self.last_scan.saturating_sub(1))
                .await?;
            self.last_scan = slot;
            first
        } else {
            0
        };
        let value_usd = now.wallet.value_usd;
        self.peak_value_usd = self.peak_value_usd.max(value_usd);
        self.max_drawdown_usd = self.max_drawdown_usd.max(self.peak_value_usd - value_usd);
        let record = StepRecord {
            k,
            at,
            mark_usd: now.wallet.price_usd,
            index_usd: now.wallet.price_usd,
            level_usd: now.wallet.price_usd,
            book_usd: None,
            best_bid_usd: None,
            best_ask_usd: None,
            position: now.position(&self.instruments[0]),
            value_usd,
            pnl_usd: value_usd - self.start_value_usd,
            turn_ms: None,
            answered: None,
            fair_usd: None,
        };
        self.last = Some(now);
        Ok(record)
    }

    async fn turn_data(&mut self, step: &mut Step) -> Result<()> {
        let Some(data) = &self.scenario.turn_data else {
            return Ok(());
        };
        let clock = if data.timestamps || data.depth_levels > 0 {
            Some(books::unix_timestamp(self.venues.chain.as_ref()).await?)
        } else {
            None
        };
        if data.tape {
            step.tape = self.ledger.fills[self.first_new..]
                .iter()
                .map(|seen| trade(seen, data.parties))
                .collect();
        }
        if data.depth_levels > 0 {
            step.depth = self
                .venues
                .depth(data.depth_levels, step.slot, clock.unwrap_or_default())
                .await?;
        }
        if data.timestamps {
            step.unix_timestamp = clock;
        }
        Ok(())
    }

    fn wallet(&self) -> Option<&Wallet> {
        self.last.as_ref().map(|reading| &reading.wallet)
    }

    async fn desk(
        &self,
        host_url: &str,
        bot: &Keypair,
        env: &BotEnv,
        mandate: &Mandate,
    ) -> Result<Desk> {
        // The bot's seats, the scenario's first venue's first, trading
        // through the guard's host side: their sends are the bot's, their
        // reads aren't.
        let mut seats = Vec::new();
        let mut infos = Vec::new();
        for instrument in &self.instruments {
            let market = self.venues.of(instrument);
            seats.push(market.seat(bot.insecure_clone(), host_url).await?);
            infos.push(market.info(true));
        }
        let mut venues = mtx_kit::VenueInfo::of_mandate(mandate, &self.instruments[0], |i| {
            self.instruments.contains(i)
        });
        for info in &mut venues {
            if let Some(market) = infos.iter().find(|m| m.id == info.id) {
                info.kind = market.kind;
                info.address = market.address.clone();
            }
        }
        Ok(Desk {
            markets: venues,
            seats,
            symbol: env.symbol.clone(),
            venue: serde_json::to_string(&books::SessionVenues::of(
                &self.spot.venues,
                &self.scenario.symbol,
            )?)?,
            authority: bot.pubkey().to_string(),
            reader: Reader::Wallet {
                start: self.start.wallet.clone(),
            },
        })
    }

    fn decode(&self, ix: &serde_json::Value, keys: &[serde_json::Value]) -> IxRecord {
        decode_instruction(ix, keys)
    }

    async fn finish(mut self: Box<Self>, done: Done) -> Result<SessionReport> {
        let scenario = &self.scenario.clone();
        let base_slot = self.base_slot;
        let end = self.venues.read().await?;
        self.note(&end, None);
        let after = if self.tracking {
            self.last_scan.saturating_sub(1)
        } else {
            base_slot
        };
        self.venues.scan(&mut self.ledger, after).await?;
        let liquidity_moves = self.venues.liquidity_moves(base_slot).await?;
        let ledger = &self.ledger;
        let fills = ledger.own(base_slot)?;
        let start = &self.start;
        let (start_value_usd, start_price_usd) = (self.start_value_usd, self.start_price_usd);
        let end_value_usd = end.wallet.value_usd;
        let order = scenario.order.as_ref().map(|order| {
            let own: Vec<(Side, f64)> = fills.iter().map(|f| (f.side, f.base)).collect();
            order.report(
                start.wallet.base,
                end.wallet.base,
                start_price_usd,
                end.wallet.price_usd,
                end_value_usd - start_value_usd,
                &own,
            )
        });
        let participation = scenario
            .participation
            .as_ref()
            .map(|p| p.report(&ledger.traded(base_slot), &self.spot.venues));
        let net = scenario.net.as_ref().map(|n| {
            let own: Vec<(String, Side, f64)> = fills
                .iter()
                .map(|f| (f.instrument.venue.clone(), f.side, f.base))
                .collect();
            n.report(
                start.wallet.base,
                end.wallet.base,
                (start_price_usd, end.wallet.price_usd),
                end_value_usd - start_value_usd,
                &own,
                &self.spot.venues,
            )
        });
        let pnl_usd = order.as_ref().map_or_else(
            || {
                net.as_ref()
                    .map_or(end_value_usd - start_value_usd, |n| n.pnl_usd())
            },
            |o| o.pnl_usd(),
        ) - participation.as_ref().map_or(0.0, |p| p.charge_usd);
        let volume = if self.tracking || scenario.turn_data.is_some() {
            ledger.volume(&self.spot.venues, base_slot, self.steps)
        } else {
            Vec::new()
        };
        let instrument = self.instruments[0].clone();
        let observed = std::mem::take(&mut self.observed);
        Ok(SessionReport {
            scenario: scenario.name.clone(),
            start_mark_usd: start_price_usd,
            end_mark_usd: end.wallet.price_usd,
            start_value_usd,
            end_value_usd,
            pnl_usd,
            end_position: end.position(&instrument),
            liquidatable: false,
            refused_calls: done.refused_calls,
            bot_exit: done.bot_exit,
            late_slots: done.late_slots,
            mandate: done.mandate,
            peak_position: observed.range(&instrument).map_or(0.0, PositionRange::peak),
            instrument,
            positions: observed.positions,
            positions_outside_universe: 0,
            max_drawdown_usd: self
                .max_drawdown_usd
                .max(self.peak_value_usd.max(end_value_usd) - end_value_usd),
            valuation: "pyth",
            flow: done.actors.flow,
            competitor: done.actors.competitor,
            faults: done.faults,
            spot: Some(SpotReport {
                start: start.wallet.clone(),
                end: end.wallet,
                fills,
                open_orders: std::mem::take(&mut self.open_orders),
                liquidity_moves,
                volume,
                pools: std::mem::take(&mut self.pools),
                arbitrage: done.actors.arbitrage,
            }),
            order,
            participation,
            net,
            turns: done.turns,
            wall_ms: done.began.elapsed().as_millis() as u64,
            timeline: Timeline {
                start_slot: done.start_slot,
                end_slot: done.end_slot,
                slots_per_step: SLOTS_PER_STEP,
                start_mark_usd: start_price_usd,
                steps: done.steps,
                transactions: done.transactions,
                calls: done.calls,
                taker_orders: done.actors.taker_orders,
            },
        })
    }
}

/// One instruction of a bot's transaction, named by its program.
fn decode_instruction(ix: &serde_json::Value, keys: &[serde_json::Value]) -> IxRecord {
    let program = ix["programIdIndex"]
        .as_u64()
        .and_then(|i| keys.get(i as usize))
        .and_then(|k| k.as_str())
        .unwrap_or_default();
    let name = if program == phoenix_v1::PROGRAM_ID.to_string() {
        "phoenix"
    } else if program == openbook_v2::PROGRAM_ID.to_string() {
        "openbook"
    } else if program == mtx_kit::protocols::raydium::clmm::PROGRAM_ID.to_string() {
        "raydium"
    } else if program == mtx_kit::protocols::meteora::dlmm::PROGRAM_ID.to_string() {
        "meteora"
    } else if program == mtx_kit::protocols::orca::whirlpool::PROGRAM_ID.to_string() {
        "orca"
    } else if program == "ComputeBudget111111111111111111111111111111" {
        "compute_budget"
    } else {
        program
    };
    IxRecord {
        program: name.to_string(),
        name: None,
        order: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scenario() -> Scenario {
        let mut mandate = Mandate::spot(&crate::venue::books::VENUES, SYMBOL, 10.0);
        for allowance in &mut mandate.universe {
            allowance.sides = Some(vec![Side::Sell]);
        }
        Scenario {
            name: "spot".into(),
            description: String::new(),
            symbol: SYMBOL.into(),
            duration_slots: 40,
            mandate,
            path: vec![],
            making: None,
            offhours: None,
            funding: None,
            tape: Vec::new(),
            faults: None,
            fair_path: None,
            raw_sends: None,
            turn_data: None,
            order: None,
            participation: None,
            net: None,
            spot: Some(Spot {
                venues: vec!["phoenix".into(), "openbook".into()],
                liquidity: Vec::new(),
                arbitrage: None,
                base: 10.0,
                quote: 0.0,
                bound_usd: None,
            }),
            reference_pnl_usd: None,
        }
    }

    #[test]
    fn a_spot_scenario_names_its_markets_in_the_mandate() {
        let ok = scenario();
        assert_eq!(ok.validate(), Ok(()));
        assert_eq!(ok.max_position(), 10.0);
        assert_eq!(ok.bound_usd(150.0), 0.0);
        let json = serde_json::to_string(&ok).unwrap();
        assert_eq!(serde_json::from_str::<Scenario>(&json).unwrap(), ok);

        let mut one_venue = scenario();
        one_venue.spot.as_mut().unwrap().venues = vec!["phoenix".into()];
        assert!(one_venue.validate().unwrap_err().contains("exactly"));

        let mut unknown = scenario();
        unknown.spot.as_mut().unwrap().venues[1] = "kamino".into();
        assert!(unknown.validate().unwrap_err().contains("unknown venue"));
        let mut pool = scenario();
        pool.spot.as_mut().unwrap().venues[1] = "raydium".into();
        assert!(pool.validate().unwrap_err().contains("no SOL-USDC market"));

        let mut moving = scenario();
        moving.path.push(crate::scenario::Waypoint::at(4, -1.0));
        assert!(moving.validate().unwrap_err().contains("no path"));

        let mut marked = scenario();
        marked.fair_path = Some(vec![crate::scenario::Waypoint::at(4, -1.0)]);
        assert!(marked.validate().unwrap_err().contains("fair path"));

        let mut other = scenario();
        other.symbol = "SOL".into();
        assert!(other.validate().is_err());

        let mut negative = scenario();
        negative.spot.as_mut().unwrap().quote = -1.0;
        assert!(negative.validate().is_err());

        // Faults are opt-in for a spot session too, and checked the same way.
        let mut faulted = scenario();
        faulted.faults = Some(crate::faults::Faults {
            seed: 1,
            latency: None,
            congestion: None,
            blockhash_expiry: None,
            drop: Some(crate::faults::SendNumbers { sends: vec![1] }),
            lost_confirmation: None,
            rpc_unavailable: None,
        });
        assert_eq!(faulted.validate(), Ok(()));
        let json = serde_json::to_string(&faulted).unwrap();
        assert_eq!(serde_json::from_str::<Scenario>(&json).unwrap(), faulted);
        faulted.faults.as_mut().unwrap().drop = Some(crate::faults::SendNumbers { sends: vec![0] });
        assert!(faulted.validate().unwrap_err().contains("faults"));

        // A spot mandate needs no drawdown limit; a perp scenario still does.
        let mut perp = crate::scenario::ShockFamily::spcx().sample("s", 1);
        perp.mandate.limits.max_drawdown_usd = None;
        assert!(perp.validate().unwrap_err().contains("drawdown"));
    }

    #[test]
    fn turn_data_orders_and_participation_are_checked_and_round_trip() {
        use crate::orders::{ExecutionOrder, Participation};

        let mut full = scenario();
        full.turn_data = Some(TurnData {
            tape: true,
            depth_levels: 5,
            timestamps: true,
            parties: true,
        });
        full.order = Some(ExecutionOrder {
            side: Side::Sell,
            quantity: 10.0,
            haircut_bps: 50.0,
            reduce_only: false,
        });
        full.participation = Some(Participation {
            max_share: 0.2,
            window_slots: 8,
            price_bps: 5.0,
        });
        assert_eq!(full.validate(), Ok(()));
        let json = serde_json::to_string(&full).unwrap();
        assert_eq!(serde_json::from_str::<Scenario>(&json).unwrap(), full);
        // A scenario without them writes none of them.
        let plain = serde_json::to_string(&scenario()).unwrap();
        assert!(!plain.contains("turn_data") && !plain.contains("order"));
        assert!(!plain.contains("participation"));

        let mut empty = full.clone();
        empty.turn_data = Some(TurnData::default());
        assert!(empty.validate().unwrap_err().contains("asks for nothing"));
        let mut parties = full.clone();
        parties.turn_data = Some(TurnData {
            tape: false,
            depth_levels: 2,
            timestamps: false,
            parties: true,
        });
        assert!(parties.validate().is_err());
        let mut deep = full.clone();
        deep.turn_data.as_mut().unwrap().depth_levels = MAX_DEPTH_LEVELS + 1;
        assert!(deep.validate().unwrap_err().contains("depth_levels"));
        let mut short = full.clone();
        short.order.as_mut().unwrap().quantity = 11.0;
        assert!(short.validate().unwrap_err().contains("selling 11"));
        let mut share = full.clone();
        share.participation.as_mut().unwrap().max_share = 0.0;
        assert!(share.validate().is_err());

        // They are for spot scenarios.
        let mut perp = crate::scenario::ShockFamily::spcx().sample("s", 1);
        perp.order = full.order.clone();
        assert!(perp.validate().unwrap_err().contains("spot scenarios"));
    }

    #[test]
    fn base_parked_in_a_pool_position_counts_toward_the_spot_cap() {
        let spot: InstrumentRef = "orca:spot:SPCX-USDC".parse().unwrap();
        let pool: InstrumentRef = "orca:amm:SPCX-USDC".parse().unwrap();
        let other: InstrumentRef = "raydium:amm:SPCX-USDC".parse().unwrap();
        let reading = |wallet: f64, pooled: &[(&InstrumentRef, f64)]| Reading {
            wallet: Wallet {
                base: wallet,
                ..Wallet::default()
            },
            open_orders: Vec::new(),
            pooled: pooled.iter().map(|(i, b)| ((*i).clone(), *b)).collect(),
            positions: Vec::new(),
        };
        // The spot position is the wallet's base and every pool position's;
        // an amm instrument's is its own pool's.
        let held = reading(6.0, &[(&pool, 3.0), (&other, 2.0)]);
        assert_eq!(held.position(&spot), 11.0);
        assert_eq!(held.position(&pool), 3.0);
        assert_eq!(reading(6.0, &[]).position(&spot), 6.0);

        // Under a cap of 10, 6 in the wallet and 5 in positions is a breach
        // that neither shows alone.
        let mandate = Mandate::spot(&["orca"], "SPCX-USDC", 10.0);
        let mut observed = Observed::default();
        observed.begin(&spot, 6.0);
        observed.record(&spot, held.position(&spot));
        assert!(matches!(
            mandate.audit(&observed),
            Some(mtx_core::mandate::Breach::Position { .. })
        ));
        let mut within = Observed::default();
        within.begin(&spot, 6.0);
        within.record(&spot, reading(6.0, &[(&pool, 3.0)]).position(&spot));
        assert_eq!(mandate.audit(&within), None);
    }

    #[test]
    fn the_report_audits_its_fills() {
        let sell = |price| SpotFill {
            at: 4,
            instrument: "phoenix:spot:SOL-USDC".parse().unwrap(),
            side: Side::Sell,
            role: "taker",
            price,
            base: 1.0,
        };
        let report = SpotReport {
            fills: vec![sell(150.0)],
            ..SpotReport::default()
        };
        let mut mandate = scenario().mandate;
        mandate.universe[0].min_sell_price = Some(149.0);
        let observed = Observed {
            fills: report.observed_fills(),
            ..Observed::default()
        };
        assert_eq!(mandate.audit(&observed), None);
        let mut bought = report.clone();
        bought.fills.push(SpotFill {
            side: Side::Buy,
            ..sell(150.0)
        });
        let observed = Observed {
            fills: bought.observed_fills(),
            ..Observed::default()
        };
        assert!(mandate.audit(&observed).is_some());
    }

    /// The pool snapshot offline: every pool's markets read, each priced near
    /// the others and the oracle, a wallet with nothing read as nothing.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_pool_snapshot_prices_every_pool_near_the_oracle() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../snapshots/spcx-pools-2026-11-13")
            .canonicalize()
            .unwrap();
        let (surfnet, _) = snapshot::start_snapshot_stepped(&dir).await.unwrap();
        let internal = Guard::internal(surfnet.rpc_url()).await.unwrap();
        let chain: Arc<dyn ChainClient> = Arc::new(RpcChainClient::new(internal.url()));
        let spot = Spot {
            venues: POOLS.map(Into::into).to_vec(),
            liquidity: POOLS.map(Into::into).to_vec(),
            arbitrage: None,
            base: 0.0,
            quote: 0.0,
            bound_usd: None,
        };
        let venues = Venues::connect(
            chain,
            internal.url(),
            &spot,
            "SPCX-USDC",
            Keypair::new().pubkey(),
        )
        .await
        .unwrap();
        assert_eq!(venues.markets.len(), 2 * POOLS.len());
        let reading = venues.read().await.unwrap();
        assert_eq!((reading.wallet.base, reading.wallet.quote), (0.0, 0.0));
        assert!(reading.positions.is_empty());
        // The copied Pyth account was last pushed weeks before the fork: the
        // pools trade within 10% of it, and within 1% of each other.
        let oracle = reading.wallet.price_usd;
        let mut mids = Vec::new();
        for venue in POOLS {
            let (Some(bid), Some(ask)) = venues.market(venue).top().await.unwrap() else {
                panic!("{venue} has no price");
            };
            assert!(bid < ask, "{venue}: {bid} / {ask}");
            mids.push((bid + ask) / 2.0);
        }
        for mid in &mids {
            assert!((mid / mids[0] - 1.0).abs() < 0.01, "{mids:?}");
        }
        assert!(
            (mids[0] / oracle - 1.0).abs() < 0.1,
            "{mids:?} against {oracle}"
        );
    }

    /// Sign `instructions` as one transaction paid by `payer`, with `others`
    /// signing too, and land it.
    fn land(
        rpc: &RpcClient,
        payer: &Keypair,
        others: &[&Keypair],
        instructions: Vec<mtx_kit::Instruction>,
    ) {
        use mtx_kit::chain::{InstructionGroup, TransactionPlan};

        let plan =
            TransactionPlan::from_group(&InstructionGroup::new("attack", instructions), 1_000_000);
        let blockhash = rpc.get_latest_blockhash().unwrap();
        let mut tx = plan.transaction(&payer.pubkey(), blockhash);
        let mut signers = vec![payer];
        signers.extend(others);
        tx.try_sign(&signers, blockhash).unwrap();
        let simulation = rpc.simulate_transaction(&tx).unwrap().value;
        assert!(
            simulation.err.is_none(),
            "{:?}\n{}",
            simulation.err,
            simulation.logs.unwrap_or_default().join("\n")
        );
        rpc.send_and_confirm_transaction(&tx).unwrap();
    }

    /// The pool snapshot, live: what an account does through each pool's
    /// liquidity is read from the chain instruction by instruction and
    /// owner by owner, whatever transaction carries it and whoever pays
    /// for it, as a process bot or a hand-built transaction could send it.
    #[tokio::test(flavor = "multi_thread")]
    async fn pool_moves_are_read_per_instruction_and_per_owner() {
        use mtx_kit::{AmountUnit, PriceRange, Side as KitSide};

        use crate::venue::pools::{self, Deposit, SwapOrder};

        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../snapshots/spcx-pools-2026-11-13")
            .canonicalize()
            .unwrap();
        let (surfnet, _) = snapshot::start_snapshot(&dir, snapshot::Access::Open)
            .await
            .unwrap();
        let url = surfnet.rpc_url().to_string();
        let chain: Arc<dyn ChainClient> = Arc::new(RpcChainClient::new(&url));
        let rpc = mtx_kit::rpc_client(&url);
        let (owner, payer) = (Keypair::new(), Keypair::new());
        let mut funded = Vec::new();
        for venue in POOLS {
            for named in books::named_markets(venue).unwrap() {
                if !funded.contains(&named.base_mint) {
                    fund_pair(
                        &surfnet,
                        &rpc,
                        &owner,
                        &named.base_mint,
                        10.0,
                        2_000.0,
                        true,
                    )
                    .unwrap();
                    funded.push(named.base_mint);
                }
            }
        }
        for key in [&owner, &payer] {
            surfnet
                .cheatcodes()
                .fund_sol(&key.pubkey(), 10_000_000_000)
                .unwrap();
        }
        let me = owner.pubkey();
        for venue in POOLS {
            let named = &books::named_markets(venue).unwrap()[0];
            let pool = pools::connect(venue, chain.as_ref(), named).await.unwrap();
            let instrument = |kind| InstrumentRef {
                venue: venue.into(),
                kind,
                symbol: named.symbol.into(),
            };
            let spot = spot_venue(venue, &url)
                .unwrap()
                .market(&instrument(Kind::Spot))
                .await
                .unwrap();
            let amm = spot_venue(venue, &url)
                .unwrap()
                .market(&instrument(Kind::Amm))
                .await
                .unwrap();
            let mid = pool.price(chain.as_ref()).await.unwrap().price;
            let range = |lower: f64, upper: f64| {
                Some(PriceRange {
                    lower: mid * lower,
                    upper: mid * upper,
                })
            };
            let deposit = |position: Option<Pubkey>, new: Pubkey, base, quote, range| {
                let pool = pool.clone();
                let chain = chain.clone();
                async move {
                    pool.deposit(
                        chain.as_ref(),
                        &me,
                        position.as_ref(),
                        &new,
                        &Deposit {
                            base,
                            quote,
                            range,
                            shape: None,
                        },
                    )
                    .await
                    .unwrap()
                    .0
                }
            };
            let swap = |side, amount, limit| {
                let pool = pool.clone();
                let chain = chain.clone();
                async move {
                    pool.swap(
                        chain.as_ref(),
                        &me,
                        &SwapOrder {
                            side,
                            amount,
                            unit: AmountUnit::Base,
                            limit,
                            partial: false,
                        },
                    )
                    .await
                    .unwrap()
                }
            };
            let slot = || rpc.get_slot().unwrap();
            let sell_only = {
                let mut mandate = Mandate::spot(&[venue], named.symbol, 100.0);
                mandate.universe.push(mtx_core::mandate::Allowance {
                    instrument: instrument(Kind::Amm),
                    max_position: 100.0,
                    min_position: None,
                    sides: Some(vec![Side::Sell]),
                    min_sell_price: None,
                    max_buy_price: None,
                });
                mandate
            };
            let audit = |moves: Vec<Side>, mandate: &Mandate| {
                mandate.audit(&Observed {
                    liquidity: moves
                        .into_iter()
                        .map(|s| (instrument(Kind::Amm), s))
                        .collect(),
                    ..Observed::default()
                })
            };

            // An honest position, opened and topped up by the owner: a buy,
            // within a mandate that allows both sides.
            let start = slot();
            let opened = Keypair::new();
            let ixs = deposit(None, opened.pubkey(), 0.05, 5.0, range(0.97, 1.03)).await;
            land(&rpc, &owner, &[&opened], ixs);
            let held = pool
                .position(chain.as_ref(), &me, &opened.pubkey())
                .await
                .unwrap()
                .expect("the position opened");
            let moves = amm.liquidity_moves(start, &me).await.unwrap();
            assert_eq!(moves, vec![Side::Buy], "{venue}");
            let open = Mandate::spot(&[venue], named.symbol, 100.0);
            assert_eq!(audit(moves.clone(), &open), None, "{venue}");
            assert!(audit(moves, &sell_only).is_some(), "{venue}");

            // A sale and a deposit in one transaction: the vaults net to a
            // bigger sale, but the deposit is still read, as a buy, and the
            // sale as only what was swapped.
            let after = slot();
            let mut ixs = swap(KitSide::Ask, 0.05, mid * 0.9).await;
            ixs.extend(deposit(Some(held.id), Keypair::new().pubkey(), 0.01, 1.0, None).await);
            land(&rpc, &owner, &[], ixs);
            assert_eq!(
                amm.liquidity_moves(after, &me).await.unwrap(),
                vec![Side::Buy],
                "{venue}: the deposit beside a sale"
            );
            let fills = spot.fills(after, &me).await.unwrap();
            assert_eq!(fills.len(), 1, "{venue}");
            let (fill, ids) = &fills[0];
            assert_eq!(
                (fill.side_for(ids), fill.quantity),
                (Some(Side::Sell), Decimal::new(5, 2)),
                "{venue}"
            );

            // A throwaway pays the fees, the owner co-signs: a purchase and
            // a deposit, both the owner's.
            let after = slot();
            let ixs = swap(KitSide::Bid, 0.02, mid * 1.1).await;
            land(&rpc, &payer, &[&owner], ixs);
            let ixs = deposit(Some(held.id), Keypair::new().pubkey(), 0.01, 1.0, None).await;
            land(&rpc, &payer, &[&owner], ixs);
            let fills = spot.fills(after, &me).await.unwrap();
            assert!(
                fills.len() == 1
                    && fills[0].0.taker == me
                    && fills[0].0.side_for(&fills[0].1) == Some(Side::Buy),
                "{venue}: {fills:?}"
            );
            assert_eq!(
                amm.liquidity_moves(after, &me).await.unwrap(),
                vec![Side::Buy],
                "{venue}: a deposit someone else paid for"
            );

            // Quote alone, in a range below the price: it rests to buy base
            // as the price falls, so a sell-only mandate is broken.
            let after = slot();
            let below = Keypair::new();
            let ixs = deposit(None, below.pubkey(), 0.0, 5.0, range(0.8, 0.9)).await;
            land(&rpc, &owner, &[&below], ixs);
            let moves = amm.liquidity_moves(after, &me).await.unwrap();
            assert_eq!(
                moves,
                vec![Side::Buy],
                "{venue}: quote alone below the price"
            );
            assert!(audit(moves, &sell_only).is_some(), "{venue}");

            // The position's NFT moved to another token account of the
            // owner's, its associated one closed: still the owner's, in the
            // cap and in the value.
            if venue != "meteora" {
                let before = amm.holdings(&me).await.unwrap();
                let value = amm.value_usd(chain.as_ref(), &before).await.unwrap();
                let program = mtx_kit::token::TOKEN_2022_PROGRAM_ID;
                let ata = token::ata_for(&me, &held.id, &program);
                let other = Keypair::new();
                let rent = rpc.get_minimum_balance_for_rent_exemption(165).unwrap();
                land(
                    &rpc,
                    &owner,
                    &[&other],
                    vec![
                        solana_system_interface::instruction::create_account(
                            &me,
                            &other.pubkey(),
                            rent,
                            165,
                            &program,
                        ),
                        spl_token_2022_interface::instruction::initialize_account3(
                            &program,
                            &other.pubkey(),
                            &held.id,
                            &me,
                        )
                        .unwrap(),
                        spl_token_2022_interface::instruction::transfer_checked(
                            &program,
                            &ata,
                            &held.id,
                            &other.pubkey(),
                            &me,
                            &[],
                            1,
                            0,
                        )
                        .unwrap(),
                        spl_token_2022_interface::instruction::close_account(
                            &program,
                            &ata,
                            &me,
                            &me,
                            &[],
                        )
                        .unwrap(),
                    ],
                );
                assert!(
                    rpc.get_account(&ata).is_err(),
                    "{venue}: the NFT's account closed"
                );
                let after = amm.holdings(&me).await.unwrap();
                assert_eq!(after, before, "{venue}");
                assert!(
                    after
                        .iter()
                        .any(|h| matches!(h, Holding::Amm(p) if p.id == held.id.to_string())),
                    "{venue}"
                );
                assert_eq!(
                    amm.value_usd(chain.as_ref(), &after).await.unwrap(),
                    value,
                    "{venue}"
                );
            }
        }
    }

    /// A spot session's setup on a stepped surfnet: funding wraps SOL by a
    /// transaction, which must land through the grader's own client without
    /// a block (the surfnet produces none until the grader steps it).
    #[tokio::test(flavor = "multi_thread")]
    async fn a_stepped_spot_setup_funds_the_wallet_without_a_block() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../snapshots/sol-usdc-spot-2026-11-11")
            .canonicalize()
            .unwrap();
        let (surfnet, _) = snapshot::start_snapshot_stepped(&dir).await.unwrap();
        let internal = Guard::internal(surfnet.rpc_url()).await.unwrap();
        let url = internal.url().to_string();
        let first = mtx_kit::rpc_client(&url).get_slot().unwrap();
        let owner = Keypair::new();
        let wallet = owner.pubkey();
        let funded = tokio::time::timeout(
            Duration::from_secs(60),
            tokio::task::spawn_blocking(move || {
                let rpc = mtx_kit::rpc_client(&url);
                fund(&surfnet, &rpc, &owner, 2.5, 100.0, true)?;
                let wsol = token::ata(&wallet, &token::native_mint());
                let balance = rpc.get_token_account_balance(&wsol)?;
                Ok::<_, Error>((rpc.get_slot()?, balance.amount))
            }),
        )
        .await
        .expect("funding a stepped surfnet's wallet hung")
        .unwrap()
        .unwrap();
        assert_eq!(funded, (first, "2500000000".to_string()));
    }
}
