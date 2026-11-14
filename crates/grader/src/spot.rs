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
    actors::{Cast, Moment, Pricing, Takers},
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
    venue::{Market, books, price_usd, spot_venue, value_usd},
    wasm::{Desk, Reader},
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
    /// The `symbol` spot markets, as the mandate names them.
    pub fn instruments(&self, symbol: &str) -> Vec<InstrumentRef> {
        self.venues
            .iter()
            .map(|venue| InstrumentRef {
                venue: venue.clone(),
                kind: Kind::Spot,
                symbol: symbol.into(),
            })
            .collect()
    }
}

/// A spot session's account of the bot's wallet and trading.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub volume: Vec<VenueVolume>,
}

/// One venue's traded base volume, step by step.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Wallet {
    /// SOL and USDC, in the wallet and on every venue, free or locked.
    pub base: f64,
    pub quote: f64,
    /// The Pyth SOL/USD price it was valued at, and its value.
    pub price_usd: f64,
    pub value_usd: f64,
}

impl Wallet {
    /// Value the wallet at `price` if it is one, as the valuation feed's
    /// anchor makes the start: the held SOL is worth what the feed will say
    /// from the first slot, so the shift isn't PnL.
    fn reprice(&mut self, price: Option<f64>) {
        if let Some(price) = price {
            self.value_usd += self.base * (price - self.price_usd);
            self.price_usd = price;
        }
    }
}

/// The price a feed anchors the valuation feed (the Pyth SOL/USD the account
/// is valued at) to, if one does.
fn valuation_anchor(
    oracles: &crate::oracles::Oracles,
    starts: &std::collections::BTreeMap<String, crate::oracles::OracleStart>,
) -> Option<f64> {
    let valuation =
        mtx_kit::protocols::pyth::feed_account(0, &mtx_kit::protocols::pyth::SOL_USD_FEED_ID);
    oracles
        .feeds
        .iter()
        .find(|f| {
            matches!(&f.source, crate::oracles::Source::Pyth(p) if p.address().ok() == Some(valuation))
        })
        .and_then(|f| starts.get(&f.name))
        .map(|start| start.anchored_price)
}

/// One of the bot's fills.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SpotFill {
    /// Slots into the session it landed at (or, waiting in OpenBook's event
    /// heap, was read at).
    pub at: u64,
    pub instrument: InstrumentRef,
    /// The bot's side, and whether it took or made.
    pub side: Side,
    #[serde(deserialize_with = "session::static_name")]
    pub role: session::StaticName,
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
}

impl Venues {
    /// The `symbol` market of each of `venues`, as its venue offers it.
    async fn connect(
        chain: Arc<dyn ChainClient>,
        rpc_url: &str,
        venues: &[String],
        symbol: &str,
        owner: Pubkey,
    ) -> Result<Self> {
        let mut markets = Vec::new();
        for venue in VENUES.iter().filter(|v| venues.iter().any(|w| w == *v)) {
            let instrument = InstrumentRef {
                venue: (*venue).into(),
                kind: Kind::Spot,
                symbol: symbol.into(),
            };
            markets.push(spot_venue(venue, rpc_url)?.market(&instrument).await?);
        }
        Ok(Self {
            chain,
            owner,
            markets,
        })
    }

    /// The market of `venue`.
    fn market(&self, venue: &str) -> &dyn Market {
        self.markets
            .iter()
            .find(|m| m.instrument().venue == venue)
            .expect("a market on each of the session's venues")
            .as_ref()
    }

    /// The account across the markets: each asset counted once in the
    /// wallet and on every venue (the spot profile's position), and valued
    /// at the reference prices, the base asset's also the session's price.
    async fn read(&self) -> Result<Reading> {
        let chain = self.chain.as_ref();
        let mut all: Vec<Holding> = Vec::new();
        let mut open_orders = Vec::new();
        for market in &self.markets {
            let held = market.holdings(&self.owner).await?;
            // An ask locks the base, a bid the quote.
            for (holding, side) in held.iter().zip([Side::Sell, Side::Buy]) {
                if let Holding::Spot(asset) = holding
                    && asset.locked > Decimal::ZERO
                {
                    open_orders.push((market.instrument().clone(), side));
                }
            }
            all.extend(held);
        }
        let value = value_usd(chain, &all).await?;
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
        })
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
    fund(
        &surfnet,
        &rpc,
        &bot,
        spot.base,
        spot.quote,
        scenario.order.is_some(),
    )?;
    let venues = Venues::connect(
        chain.clone(),
        internal.url(),
        &spot.venues,
        &scenario.symbol,
        bot.pubkey(),
    )
    .await?;
    let mut start = venues.read().await?;
    let instruments = spot.instruments(&scenario.symbol);
    // A tape's takers, each a funded wallet trading the session's first
    // venue.
    let mut cast = Cast::default();
    if !scenario.tape.is_empty() {
        let first = venues.market(&spot.venues[0]);
        let mut seats = Vec::new();
        for i in 0..TAKER_ACCOUNTS {
            let keypair = opts.keys.keypair("taker", u32::from(i));
            fund(&surfnet, &rpc, &keypair, TAKER_BASE, TAKER_QUOTE, true)?;
            seats.push(first.seat(keypair, internal.url()).await?);
        }
        cast.add(Box::new(Takers::new(
            seats,
            scenario.tape.clone(),
            Pricing::Book,
            0,
        )));
    }

    let guard = Guard::start(surfnet.rpc_url()).await?;
    let user = opts.slot.map(|slot| BOT_USERS[slot]);
    let workdir = SessionDir::create(&bot, user)?;
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
    let oracle_starts = match &scenario.oracles {
        Some(oracles) => {
            let mut prices = std::collections::BTreeMap::new();
            for instrument in &instruments {
                let (bid, ask) = venues.market(&instrument.venue).top().await?;
                prices.insert(
                    instrument.clone(),
                    crate::oracles::MarketQuote {
                        mid: crate::oracles::MarketQuote::mid_of(bid, ask),
                        ..Default::default()
                    },
                );
            }
            crate::oracles::register(
                &rpc,
                oracles,
                scenario.duration_slots,
                &crate::oracles::World {
                    prices,
                    ..Default::default()
                },
            )?
        }
        None => Default::default(),
    };
    if let Some(oracles) = &scenario.oracles {
        start
            .wallet
            .reprice(valuation_anchor(oracles, &oracle_starts));
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
    // Every spot market bounds the same SOL, held from the start.
    let mut observed = Observed::default();
    for instrument in &instruments {
        observed.begin(instrument, start.wallet.base);
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
        start_value_usd: start.wallet.value_usd,
        start_price_usd: start.wallet.price_usd,
        oracle_starts,
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
        vars: Vec::new(),
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
    start: Reading,
    start_value_usd: f64,
    start_price_usd: f64,
    oracle_starts: std::collections::BTreeMap<String, crate::oracles::OracleStart>,
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
    fn note(&mut self, reading: &Reading) {
        for instrument in &self.instruments {
            self.observed.record(instrument, reading.wallet.base);
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

    /// The session's last scan reads every fill the venues listed, for the
    /// report, the order and the audit.
    fn reads_fills(&self) -> bool {
        true
    }

    fn moment(&self, at: u64) -> Moment {
        // Nothing moves a spot market but its traders: the session's price
        // is where it started.
        Moment {
            at,
            level_usd: self.start_price_usd,
            book_usd: self.start_price_usd,
            fair_usd: None,
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
        self.note(&now);
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
            position: now.wallet.base,
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
        let mut addresses = Vec::new();
        for venue in &self.spot.venues {
            let market = self.venues.market(venue);
            seats.push(market.seat(bot.insecure_clone(), host_url).await?);
            addresses.push((
                market.instrument().to_string(),
                market.address().to_string(),
            ));
        }
        let mut venues = mtx_kit::VenueInfo::of_mandate(mandate, &self.instruments[0], |i| {
            self.instruments.contains(i)
        });
        for info in &mut venues {
            info.address = addresses
                .iter()
                .find(|(id, _)| *id == info.id)
                .map(|(_, address)| address.clone());
        }
        Ok(Desk {
            rfqs: None,
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
        self.note(&end);
        let after = if self.tracking {
            self.last_scan.saturating_sub(1)
        } else {
            base_slot
        };
        self.venues.scan(&mut self.ledger, after).await?;
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
            end_position: end.wallet.base,
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
            actors: done.actors.library,
            faults: done.faults,
            spot: Some(SpotReport {
                start: start.wallet.clone(),
                end: end.wallet,
                fills,
                open_orders: std::mem::take(&mut self.open_orders),
                volume,
            }),
            order,
            participation,
            net,
            close_outs: Vec::new(),
            perp_fills: Vec::new(),
            perp_open_orders: Vec::new(),
            crank_refusals: done.crank_refusals,
            oracle_starts: std::mem::take(&mut self.oracle_starts),
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
    use crate::scenario::Scenario;

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
            actors: None,
            takers_pricing: None,
            fair_path: None,
            market_model: None,
            raw_sends: None,
            start_positions: Default::default(),
            close_out: Default::default(),
            turn_data: None,
            oracles: None,
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
            informed_pnl_usd: None,
        }
    }

    #[test]
    fn an_anchored_valuation_feed_values_the_start_at_its_price() {
        use crate::oracles::*;
        let mut wallet = Wallet {
            base: 10.0,
            quote: 100.0,
            price_usd: 83.0,
            value_usd: 930.0,
        };
        let sol = "ef0d8b6fda2ceba41da15d4095d1da392a0d2f8ed0c6c7bc0f4cfac8c280b56d";
        let feed = |id: &str, name: &str| Feed {
            name: name.into(),
            source: Source::Pyth(PythOracle {
                feed_id: id.into(),
                shard: 0,
                exponent: -8,
                account: None,
            }),
            anchor: Some(Anchor::Price { usd: 77.0 }),
            cadence_slots: 4,
            path: vec![],
            conf_bps: 2.0,
            failures: vec![],
        };
        let oracles = Oracles {
            seed: 1,
            feeds: vec![feed(&"11".repeat(32), "other"), feed(sol, "sol")],
        };
        let starts = |name: &str| {
            [(
                name.to_string(),
                OracleStart {
                    source_price: 83.0,
                    anchored_price: 77.0,
                },
            )]
            .into()
        };
        // An anchored non-valuation feed moves nothing.
        wallet.reprice(valuation_anchor(&oracles, &starts("other")));
        assert_eq!((wallet.price_usd, wallet.value_usd), (83.0, 930.0));
        wallet.reprice(valuation_anchor(&oracles, &starts("sol")));
        assert_eq!(wallet.price_usd, 77.0);
        assert!((wallet.value_usd - 870.0).abs() < 1e-9);
    }

    #[test]
    fn a_spot_session_values_at_a_feed_that_can_move_but_not_fail() {
        use crate::oracles::*;
        let feed = |feed_id: &str, failing: bool| Oracles {
            seed: 1,
            feeds: vec![Feed {
                name: "sol".into(),
                source: Source::Pyth(PythOracle {
                    feed_id: feed_id.into(),
                    shard: 0,
                    exponent: -8,
                    account: None,
                }),
                anchor: None,
                cadence_slots: 4,
                path: vec![Level {
                    slot: 40,
                    level_pct: -5.0,
                }],
                conf_bps: 2.0,
                failures: if failing {
                    vec![Failure {
                        mode: Mode::OffPrice { bias_pct: 5.0 },
                        window: Window {
                            from_slot: 4,
                            from_slot_max: None,
                            slots: Some(8),
                        },
                    }]
                } else {
                    Vec::new()
                },
            }],
        };
        let sol = "ef0d8b6fda2ceba41da15d4095d1da392a0d2f8ed0c6c7bc0f4cfac8c280b56d";
        let other = "eaa020c61cc479712813461ce153894a96a6c00b21ed0cfc2798d1f9a9e9c94a";
        let with = |oracles| Scenario {
            oracles: Some(oracles),
            ..scenario()
        };
        with(feed(sol, false)).validate().unwrap();
        with(feed(other, true)).validate().unwrap();
        let error = with(feed(sol, true)).validate().unwrap_err();
        assert!(error.contains("values the account"), "{error}");
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
            instrument: None,
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

        // Turn data and participation are for spot scenarios; an order is
        // for either.
        let mut perp = crate::scenario::ShockFamily::spcx().sample("s", 1);
        perp.participation = full.participation.clone();
        assert!(perp.validate().unwrap_err().contains("spot scenarios"));
        // A spot wallet can't go short: no reduce-only buy.
        let mut buy = full.clone();
        buy.order = Some(ExecutionOrder {
            side: Side::Buy,
            reduce_only: true,
            ..full.order.clone().unwrap()
        });
        assert!(buy.validate().unwrap_err().contains("reduce-only"));
        // Starting positions and close-outs are a perp's.
        let mut started = full.clone();
        started.start_positions.insert("SOL-USDC".into(), 1.0);
        assert!(started.validate().unwrap_err().contains("perp scenarios"));
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
