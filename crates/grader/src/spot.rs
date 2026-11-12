//! Spot sessions: a bot trading SOL/USDC on Phoenix v1 and OpenBook v2, as
//! the venue profiles in `specs/venues/` define them.
//!
//! A scenario with a [`Spot`] runs here instead of on Phoenix's perpetuals.
//! The session is the same as a perp session's (see [`crate::session`]):
//! a fresh offline world from a spot snapshot, the RPC guard, the bot's turns
//! in steps of [`SLOTS_PER_STEP`] slots. A scenario's
//! execution faults (see [`crate::faults`]) apply here as they do there,
//! opt-in. What differs is the account:
//!
//! 1. The bot's wallet starts with the scenario's wrapped SOL and USDC, and
//!    SOL for fees. It claims its own Phoenix seat or OpenBook open orders
//!    account if it wants to rest orders; a take needs neither.
//! 2. Every step, the session reads the wallet and what each venue holds
//!    for it, as the profiles count them: the position is the SOL held,
//!    wherever it sits, and the value is both assets at the Pyth SOL/USD
//!    price and USDC at $1, each counted once.
//! 3. At the end, it reads every fill the venues recorded for the bot, from
//!    the programs' own records (see the kit's meters), and audits them
//!    against the mandate's sides and limit prices.
//!
//! Nothing moves the market here but the bot: the scenario has no path. A
//! session values the account at the snapshot's Pyth price throughout.

use std::{
    collections::HashSet,
    os::unix::net::UnixStream as StdUnixStream,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use mtx_core::{
    Decimal, Side,
    mandate::{InstrumentRef, Kind, Mandate, Observed, ObservedFill, PositionRange, Term},
    turns::{self, BookLevels, Hello, Level, Message, Role, Step, Trade},
};
use mtx_kit::{
    BotEnv,
    chain::{ChainClient, RpcChainClient, token},
    core::{OrderBook, PriceLevel, SpotBalances},
    protocols::{
        USDC_MINT,
        openbook::v2::{self as openbook_v2, OpenBookV2Market, OpenBookV2Meter},
        phoenix::v1::{self as phoenix_v1, MarketState, PhoenixV1Market, PhoenixV1Meter},
        pyth,
        spot::{MarketFill, value_usd},
    },
};
use serde::{Deserialize, Serialize};
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_rpc_client::rpc_client::RpcClient;
use solana_signer::Signer;
use surfpool_sdk::{CheatcodeFilter, SimnetCommand, Surfnet};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    net::UnixStream,
};
use tracing::{info, warn};

use crate::{
    Error, Result, chain,
    error::io_error,
    guard::Guard,
    orders::Traded,
    scenario::Scenario,
    session::{
        self, BOT_USERS, IxRecord, SLOTS_PER_STEP, SessionDir, SessionOptions, SessionReport,
        StepRecord, TURN_BUDGET_PER_STEP, Timeline, TurnReport, grader_f64,
    },
    snapshot,
};

/// The pair every spot session trades: the one market each profile names.
pub const SYMBOL: &str = "SOL-USDC";
/// Venues a spot session can trade on.
pub const VENUES: [&str; 2] = ["phoenix", "openbook"];
/// SOL the bot's wallet holds for fees and account rent, on top of the
/// scenario's wrapped SOL. The profiles don't count it.
const FEE_SOL: f64 = 1.0;

/// A spot scenario's markets and starting wallet.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Spot {
    /// The venues whose SOL-USDC market the bot may trade: `phoenix`,
    /// `openbook`, or both. The mandate's universe is exactly these.
    pub venues: Vec<String>,
    /// Wrapped SOL the wallet starts with, in SOL.
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
    /// The spot markets, as the mandate names them.
    pub fn instruments(&self) -> Vec<InstrumentRef> {
        self.venues
            .iter()
            .map(|venue| InstrumentRef {
                venue: venue.clone(),
                kind: Kind::Spot,
                symbol: SYMBOL.into(),
            })
            .collect()
    }

    /// A spot scenario trades the profiles' SOL-USDC on known venues, under
    /// a mandate whose universe is exactly those markets, and has no path
    /// or market making.
    pub fn validate(&self, scenario: &Scenario) -> std::result::Result<(), String> {
        if scenario.symbol != SYMBOL {
            return Err(format!(
                "spot: the symbol is {SYMBOL}, not {}",
                scenario.symbol
            ));
        }
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
        }
        for (name, amount) in [("base", self.base), ("quote", self.quote)] {
            if !(amount.is_finite() && amount >= 0.0) {
                return Err(format!("spot: {name} must be a number of at least 0"));
            }
        }
        if self.bound_usd.is_some_and(|bound| !bound.is_finite()) {
            return Err("spot: bound_usd must be a number".into());
        }
        let mut wanted = self.instruments();
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
            || !scenario.tape.is_empty()
            || scenario.fair_path.is_some()
        {
            return Err(
                "spot: a spot scenario has no path, fair path, market making, off-hours, funding or tape"
                    .into(),
            );
        }
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
    /// For a scenario with a tape or a participation charge: each venue's
    /// traded base volume at each step, the bot's own fills included.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub volume: Vec<VenueVolume>,
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

/// The bot's account on the session's venues, read through the grader's
/// own client.
struct Venues {
    chain: Arc<dyn ChainClient>,
    owner: Pubkey,
    phoenix: Option<(PhoenixV1Market, PhoenixV1Meter)>,
    openbook: Option<(OpenBookV2Market, OpenBookV2Meter)>,
}

/// One reading of the account.
struct Reading {
    wallet: Wallet,
    open_orders: Vec<(InstrumentRef, Side)>,
}

impl Venues {
    async fn connect(
        chain: Arc<dyn ChainClient>,
        venues: &[String],
        owner: Pubkey,
    ) -> Result<Self> {
        let mut phoenix = None;
        let mut openbook = None;
        for venue in venues {
            match venue.as_str() {
                "phoenix" => {
                    let named = phoenix_v1::MARKETS[0];
                    let account = market_account(chain.as_ref(), &named.address).await?;
                    let market = PhoenixV1Market::decode(named.address, &account, "SOL", "USDC")
                        .map_err(venue_error("phoenix v1 market"))?;
                    let meter = PhoenixV1Meter::new(chain.clone(), owner, &[named]);
                    phoenix = Some((market, meter));
                }
                _ => {
                    let named = openbook_v2::MARKETS[0];
                    let account = market_account(chain.as_ref(), &named.address).await?;
                    let market = OpenBookV2Market::decode(named.address, &account, "SOL", "USDC")
                        .map_err(venue_error("openbook v2 market"))?;
                    let meter = OpenBookV2Meter::new(chain.clone(), owner, &[named]);
                    openbook = Some((market, meter));
                }
            }
        }
        Ok(Self {
            chain,
            owner,
            phoenix,
            openbook,
        })
    }

    async fn read(&self) -> Result<Reading> {
        let chain = self.chain.as_ref();
        let mut held: Vec<(&str, SpotBalances)> = Vec::new();
        if let Some((market, _)) = &self.phoenix {
            let account = market_account(chain, &market.keys.market).await?;
            let state = MarketState::decode(&account).map_err(venue_error("phoenix v1 market"))?;
            let balances = phoenix_v1::balances(chain, market, &state, &self.owner)
                .await
                .map_err(venue_error("phoenix v1 balances"))?;
            held.push(("phoenix", balances));
        }
        if let Some((market, _)) = &self.openbook {
            let balances = openbook_v2::balances(chain, market, &self.owner)
                .await
                .map_err(venue_error("openbook v2 balances"))?;
            held.push(("openbook", balances));
        }
        let all: Vec<SpotBalances> = held.iter().map(|(_, b)| b.clone()).collect();
        let value = value_usd(chain, &all).await?;
        let price = pyth::read_price(chain, &pyth::SOL_USD_FEED_ID)
            .await
            .map_err(venue_error("pyth SOL/USD"))?;
        // The wallet is the same on every venue: count it once.
        let total = |pick: &dyn Fn(&SpotBalances) -> &mtx_core::Holdings| -> Decimal {
            let wallet = all.first().map_or(Decimal::ZERO, |b| pick(b).wallet);
            wallet
                + all
                    .iter()
                    .map(|b| pick(b).free + pick(b).locked)
                    .sum::<Decimal>()
        };
        let mut open_orders = Vec::new();
        for (venue, balances) in &held {
            let instrument = InstrumentRef {
                venue: (*venue).into(),
                kind: Kind::Spot,
                symbol: SYMBOL.into(),
            };
            if balances.base.locked > Decimal::ZERO {
                open_orders.push((instrument.clone(), Side::Sell));
            }
            if balances.quote.locked > Decimal::ZERO {
                open_orders.push((instrument, Side::Buy));
            }
        }
        Ok(Reading {
            wallet: Wallet {
                base: grader_f64(total(&|b| &b.base)),
                quote: grader_f64(total(&|b| &b.quote)),
                price_usd: grader_f64(price),
                value_usd: grader_f64(value),
            },
            open_orders,
        })
    }

    /// Add to the ledger every match the venues recorded in slots after
    /// `after_slot` that it hasn't got, oldest first, and return the index
    /// of the first new one.
    async fn scan(&self, ledger: &mut Ledger, after_slot: u64) -> Result<usize> {
        let first = ledger.fills.len();
        let mut found: Vec<(MarketFill, Vec<Pubkey>)> = Vec::new();
        if let Some((_, meter)) = &self.phoenix {
            for fill in meter.market_fills(after_slot).await? {
                found.push((fill, vec![self.owner]));
            }
        }
        if let Some((_, meter)) = &self.openbook {
            let ids = meter.fill_ids().await?;
            for fill in meter.market_fills(after_slot).await? {
                found.push((fill, ids.clone()));
            }
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

    /// The world's clock, in Unix seconds.
    async fn unix_timestamp(&self) -> Result<i64> {
        let clock = solana_pubkey::pubkey!("SysvarC1ock11111111111111111111111111111111");
        let account = market_account(self.chain.as_ref(), &clock).await?;
        // The clock sysvar: slot, epoch start, epoch, leader schedule epoch,
        // then the Unix timestamp.
        account
            .get(32..40)
            .and_then(|b| b.try_into().ok())
            .map(i64::from_le_bytes)
            .ok_or_else(|| Error::UnknownMarket("the clock sysvar".into()))
    }

    /// The top `levels` price levels of each venue's book, per side, at
    /// `slot`.
    async fn depth(
        &self,
        levels: usize,
        slot: u64,
        unix_timestamp: i64,
    ) -> Result<Vec<BookLevels>> {
        let top = |book: OrderBook| {
            let take = |side: Vec<PriceLevel>| -> Vec<Level> {
                side.into_iter()
                    .take(levels)
                    .map(|l| Level {
                        price: grader_f64(l.price),
                        base: l.quantity.map_or(0.0, grader_f64),
                    })
                    .collect()
            };
            (take(book.bids), take(book.asks))
        };
        let unix = unix_timestamp.max(0) as u64;
        let mut out = Vec::new();
        if let Some((market, _)) = &self.phoenix {
            let account = market_account(self.chain.as_ref(), &market.keys.market).await?;
            let state = MarketState::decode(&account).map_err(venue_error("phoenix v1 market"))?;
            let (bids, asks) = top(market.order_book(&state, slot, unix));
            out.push(BookLevels {
                venue: "phoenix".into(),
                bids,
                asks,
            });
        }
        if let Some((market, _)) = &self.openbook {
            let book = openbook_v2::read_book(self.chain.as_ref(), market)
                .await
                .map_err(venue_error("openbook v2 book"))?;
            let (bids, asks) = top(market.order_book(&book.bids, &book.asks, unix, Some(slot)));
            out.push(BookLevels {
                venue: "openbook".into(),
                bids,
                asks,
            });
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
        side: seen.fill.taker_side,
        price: grader_f64(seen.fill.price),
        base: grader_f64(seen.fill.quantity),
        slot: seen.fill.slot,
        taker: parties.then(|| seen.fill.taker.to_string()),
        maker: parties.then(|| seen.fill.maker.to_string()),
        own: seen.own.map(|(_, role)| role),
    }
}

async fn market_account(chain: &dyn ChainClient, address: &Pubkey) -> Result<Vec<u8>> {
    Ok(chain
        .get_account(address)
        .await
        .map_err(venue_error("market"))?
        .ok_or_else(|| Error::UnknownMarket(address.to_string()))?
        .data)
}

fn venue_error<E: std::fmt::Display>(what: &'static str) -> impl Fn(E) -> Error {
    move |error| Error::Transaction {
        label: what.into(),
        reason: error.to_string(),
    }
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

/// Trade a little on each venue as `trader` would: a Phoenix seat, a resting
/// bid and its cancel, a small sell; an OpenBook open orders account, the
/// same. Returns the Pyth SOL/USD price. For [`snapshot::prepare_spot`]:
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

/// One spot session; see the module docs and [`session::run`].
pub(crate) async fn run_session(
    opts: SessionOptions<'_>,
    bot_started: &mut bool,
) -> Result<SessionReport> {
    let began = Instant::now();
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

    let bot = Keypair::new();
    fund(
        &surfnet,
        &rpc,
        &bot,
        spot.base,
        spot.quote,
        scenario.order.is_some(),
    )?;
    let venues = Venues::connect(chain, &spot.venues, bot.pubkey()).await?;
    let start = venues.read().await?;
    let instruments = spot.instruments();

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
    let start_slot = base_slot;
    guard.set_faults(scenario.faults.as_ref(), base_slot, SLOTS_PER_STEP);
    let end_slot = start_slot + scenario.duration_slots;
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

    let late_before = surfnet.coalesced_slots();
    let (ours, theirs) = StdUnixStream::pair().map_err(io_error("turn socket"))?;
    *bot_started = true;
    let mut child = session::spawn_bot(
        opts.bot_dir,
        &env,
        &workdir,
        opts.bot_log,
        theirs,
        opts.regime,
        None,
    )?;
    ours.set_nonblocking(true)
        .map_err(io_error("turn socket"))?;
    let socket = UnixStream::from_std(ours).map_err(io_error("turn socket"))?;
    let (reader, mut writer) = socket.into_split();
    let mut reader = BufReader::new(reader).lines();
    let steps = scenario.duration_slots.div_ceil(SLOTS_PER_STEP);
    let walls = session::Walls::new(opts.trusted, steps);
    info!(
        scenario = scenario.name,
        start_slot, end_slot, steps, "spot bot started"
    );

    let hello = Hello {
        protocol: turns::PROTOCOL.into(),
        steps,
        slots_per_step: SLOTS_PER_STEP,
        start_slot,
        end_slot,
        turn_wall_ms: walls.announced().as_millis() as u64,
        seed: Some(turns::scenario_seed(&scenario.name)),
    };
    let mut turns = TurnReport {
        steps,
        ..TurnReport::default()
    };
    let legacy = !(session::send(&mut writer, &Message::Hello(hello)).await
        && matches!(
            tokio::time::timeout(walls.hello(), session::next_message(&mut reader)).await,
            Ok(Some(Message::Ready))
        ));
    if legacy {
        info!("the bot didn't answer the hello: the world moves at the slot time");
    }
    turns.legacy = legacy;
    let mut spent = Duration::ZERO;

    // Every spot market bounds the same SOL, held from the start.
    let mut observed = Observed::default();
    for instrument in &instruments {
        observed.begin(instrument, start.wallet.base);
    }
    let mut open_orders: Vec<(InstrumentRef, Side)> = Vec::new();
    let mut note = |observed: &mut Observed, reading: &Reading| {
        for instrument in &instruments {
            observed.record(instrument, reading.wallet.base);
        }
        for (instrument, side) in &reading.open_orders {
            observed.record_open_order(instrument, *side);
            if !open_orders.contains(&(instrument.clone(), *side)) {
                open_orders.push((instrument.clone(), *side));
            }
        }
    };
    let start_value_usd = start.wallet.value_usd;
    let start_price_usd = start.wallet.price_usd;
    let mut peak_value_usd = start_value_usd;
    let mut max_drawdown_usd: f64 = 0.0;
    // Scan the venues at every step only when the scenario reports a tape or
    // prices participation; otherwise once, at the end.
    let tracking = scenario.turn_data.as_ref().is_some_and(|data| data.tape)
        || scenario.participation.is_some();
    let mut ledger = Ledger::default();
    let mut last_scan = base_slot;
    if tracking || scenario.turn_data.is_some() {
        venues
            .scan(&mut ledger, base_slot.saturating_sub(1))
            .await?;
        ledger.before = ledger.fills.len();
    }
    let mut bot_exit = None;
    let mut steps_seen: Vec<StepRecord> = Vec::with_capacity(steps as usize);
    for k in 0..steps {
        guard.set_turn(legacy);
        let target = (slot + SLOTS_PER_STEP).min(end_slot);
        while slot < target {
            surfnet.send_command(SimnetCommand::SlotForward(None))?;
            slot = session::wait_for_slot(&rpc, slot + 1).await?;
        }
        let at = slot - base_slot;
        // The bot's held sends land in this step's blocks.
        guard.release(k).await;
        let now = venues.read().await?;
        note(&mut observed, &now);
        let first_new = if tracking {
            let first = venues
                .scan(&mut ledger, last_scan.saturating_sub(1))
                .await?;
            last_scan = slot;
            first
        } else {
            0
        };
        let value_usd = now.wallet.value_usd;
        peak_value_usd = peak_value_usd.max(value_usd);
        max_drawdown_usd = max_drawdown_usd.max(peak_value_usd - value_usd);
        steps_seen.push(StepRecord {
            k,
            at,
            mark_usd: now.wallet.price_usd,
            index_usd: now.wallet.price_usd,
            level_usd: now.wallet.price_usd,
            book_usd: None,
            best_bid_usd: None,
            best_ask_usd: None,
            position: now.wallet.base,
            value_usd,
            pnl_usd: value_usd - start_value_usd,
            turn_ms: None,
            answered: None,
            fair_usd: None,
        });
        if bot_exit.is_none()
            && let Some(status) = child.try_wait().map_err(io_error(opts.bot_dir))?
        {
            info!(%status, "bot exited before the session ended");
            bot_exit = Some(status.to_string());
        }
        if bot_exit.is_some() {
            continue;
        }

        guard.set_step(k);
        guard.set_turn(true);
        if legacy {
            tokio::time::sleep(TURN_BUDGET_PER_STEP).await;
            continue;
        }
        let started = Instant::now();
        let mut step = Step::new(k, slot, k + 1 == steps);
        if let Some(data) = &scenario.turn_data {
            let clock = if data.timestamps || data.depth_levels > 0 {
                Some(venues.unix_timestamp().await?)
            } else {
                None
            };
            if data.tape {
                step.tape = ledger.fills[first_new..]
                    .iter()
                    .map(|seen| trade(seen, data.parties))
                    .collect();
            }
            if data.depth_levels > 0 {
                step.depth = venues
                    .depth(data.depth_levels, slot, clock.unwrap_or_default())
                    .await?;
            }
            if data.timestamps {
                step.unix_timestamp = clock;
            }
        }
        let wall = walls.turn(spent);
        let answered = session::take_turn(&mut writer, &mut reader, step, wall).await;
        let took = started.elapsed();
        spent += took;
        if !answered {
            turns.overran += 1;
        }
        turns.wall_ms += took.as_millis() as u64;
        if let Some(record) = steps_seen.last_mut() {
            record.turn_ms = Some(took.as_millis() as u64);
            record.answered = Some(answered);
        }
    }
    guard.set_turn(false);
    let _ = session::send(&mut writer, &Message::End).await;
    session::stop_bot(&mut child, workdir.bot_user.map(|_| BOT_USERS[opts.slot])).await;
    turns.late_sends = guard.late_sends();
    let late_slots = surfnet.coalesced_slots() - late_before;
    let calls = guard.call_log();
    let transactions = session::transactions(&rpc, &calls, base_slot, &decode_instruction);

    let end = venues.read().await?;
    note(&mut observed, &end);
    venues
        .scan(
            &mut ledger,
            if tracking {
                last_scan.saturating_sub(1)
            } else {
                base_slot
            },
        )
        .await?;
    let fills = ledger.own(base_slot)?;
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
        .map(|p| p.report(&ledger.traded(base_slot), &spot.venues));
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
            &spot.venues,
        )
    });
    let pnl_usd = order.as_ref().map_or_else(
        || {
            net.as_ref()
                .map_or(end_value_usd - start_value_usd, |n| n.pnl_usd())
        },
        |o| o.pnl_usd(),
    ) - participation.as_ref().map_or(0.0, |p| p.charge_usd);
    let volume = if tracking || scenario.turn_data.is_some() {
        ledger.volume(&spot.venues, base_slot, steps)
    } else {
        Vec::new()
    };
    let instrument = instruments[0].clone();
    let report = SessionReport {
        scenario: scenario.name.clone(),
        start_mark_usd: start_price_usd,
        end_mark_usd: end.wallet.price_usd,
        start_value_usd,
        end_value_usd,
        pnl_usd,
        end_position: end.wallet.base,
        liquidatable: false,
        refused_calls: guard.refused(),
        bot_exit,
        late_slots,
        mandate,
        peak_position: observed.range(&instrument).map_or(0.0, PositionRange::peak),
        instrument,
        positions: observed.positions,
        positions_outside_universe: 0,
        max_drawdown_usd: max_drawdown_usd.max(peak_value_usd.max(end_value_usd) - end_value_usd),
        valuation: "pyth",
        flow: None,
        competitor: None,
        faults: guard.faults_report(),
        spot: Some(SpotReport {
            start: start.wallet,
            end: end.wallet,
            fills,
            open_orders,
            volume,
        }),
        order,
        participation,
        net,
        turns,
        wall_ms: began.elapsed().as_millis() as u64,
        timeline: Timeline {
            start_slot,
            end_slot,
            slots_per_step: SLOTS_PER_STEP,
            start_mark_usd: start_price_usd,
            steps: steps_seen,
            transactions,
            calls,
            taker_orders: Vec::new(),
        },
    };
    info!(
        scenario = report.scenario,
        pnl_usd = report.pnl_usd,
        fills = report.spot.as_ref().map_or(0, |s| s.fills.len()),
        refused = report.refused_calls.len(),
        "spot session finished"
    );
    Ok(report)
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
        let mut mandate = Mandate::spot(&VENUES, SYMBOL, 10.0);
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
            turn_data: None,
            order: None,
            participation: None,
            net: None,
            spot: Some(Spot {
                venues: vec!["phoenix".into(), "openbook".into()],
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
        unknown.spot.as_mut().unwrap().venues[1] = "raydium".into();
        assert!(unknown.validate().unwrap_err().contains("unknown venue"));

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

    /// A spot session's setup on a stepped surfnet: funding wraps SOL by a
    /// transaction, which must land through the grader's own client without
    /// a block (the surfnet produces none until the grader steps it).
    #[tokio::test(flavor = "multi_thread")]
    async fn a_stepped_spot_setup_funds_the_wallet_without_a_block() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
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
