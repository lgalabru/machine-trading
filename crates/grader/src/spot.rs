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
//! Without actors, nothing moves the market here but the bot: the scenario
//! has no path, and the account is valued at the snapshot's Pyth price
//! throughout. With them (see [`crate::actors`]), seats on each venue quote
//! and trade around the scenario's level, which the path steps from the
//! snapshot's Pyth price at each waypoint; a tape's takers trade at the
//! start of each step; OpenBook is cranked after each step's blocks. With
//! `oracle`, Pyth's SOL/USD price steps along the path too, and the account
//! is valued there.

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
    turns::{self, Hello, Message, Step},
};
use mtx_kit::{
    BotEnv,
    chain::{ChainClient, RpcChainClient, token},
    core::SpotBalances,
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
use solana_signer::Signer;
use surfpool_sdk::{BlockProductionMode, CheatcodeFilter, SimnetCommand, Surfnet};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    net::UnixStream,
};
use tracing::{info, warn};

use crate::{
    Error, Result,
    actors::{Actors, Cast, FamilySetup, Seat, StepFlow, VenueFlow, family},
    chain,
    error::io_error,
    guard::Guard,
    making::level_at,
    scenario::Scenario,
    session::{
        self, BOT_USERS, HELLO_WALL, IxRecord, SLOTS_PER_STEP, SessionDir, SessionOptions,
        SessionReport, StepRecord, TURN_BUDGET_PER_STEP, TURN_WALL, Timeline, TurnReport,
        grader_f64,
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
        let actors = scenario.actors.is_some() || scenario.family_actors.is_some();
        if ((!scenario.path.is_empty() || !scenario.tape.is_empty()) && !actors)
            || scenario.making.is_some()
            || scenario.offhours.is_some()
            || scenario.funding.is_some()
            || scenario.fair_path.is_some()
        {
            return Err(
                "spot: a spot scenario has no fair path, market making, off-hours or funding, \
                 and a path or a tape only with actors"
                    .into(),
            );
        }
        let mut after = 0;
        for (i, order) in scenario.tape.iter().enumerate() {
            let venue = order.venue.map_or(self.venues[0].as_str(), |v| v.name());
            if order.slot < after
                || order.slot >= scenario.duration_slots
                || !(order.size.is_finite() && order.size > 0.0)
                || !(order.limit_bps.is_finite() && order.limit_bps >= 0.0)
                || !self.venues.iter().any(|v| v == venue)
            {
                return Err(format!(
                    "spot: tape order {i} must come in slot order, inside the session, with a \
                     size and a limit, on one of the session's venues"
                ));
            }
            after = order.slot;
        }
        // The actors' level, and Pyth's on its own path, step to each
        // waypoint at its slot.
        let oracle_path = scenario
            .actors
            .as_ref()
            .map_or(&[][..], |a| &a.oracle_path[..]);
        for (name, path) in [
            ("waypoint", &scenario.path[..]),
            ("oracle waypoint", oracle_path),
        ] {
            let mut after = None;
            for (i, waypoint) in path.iter().enumerate() {
                if after.is_some_and(|slot| waypoint.slot <= slot)
                    || waypoint.slot >= scenario.duration_slots
                    || !(waypoint.level_pct.is_finite() && waypoint.level_pct > -100.0)
                    || waypoint.premium_pct != 0.0
                {
                    return Err(format!(
                        "spot: {name} {i} must come after the last, inside the session, above \
                         -100% and with no premium"
                    ));
                }
                after = Some(waypoint.slot);
            }
        }
        if scenario.duration_slots == 0 {
            return Err("spot: the session has no slots".into());
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

    /// Every fill the venues recorded for the owner in slots after
    /// `after_slot`, each once, oldest first.
    async fn fills(&self, after_slot: u64, base_slot: u64) -> Result<Vec<SpotFill>> {
        let mut found: Vec<(MarketFill, Side, &'static str)> = Vec::new();
        let mut seen: HashSet<(String, u128)> = HashSet::new();
        let mut keep = |fills: Vec<MarketFill>, ids: &[Pubkey]| {
            for fill in fills {
                let Some(side) = fill.side_for(ids) else {
                    continue;
                };
                if seen.insert((fill.instrument.0.clone(), fill.sequence)) {
                    let role = if ids.contains(&fill.taker) {
                        "taker"
                    } else {
                        "maker"
                    };
                    found.push((fill, side, role));
                }
            }
        };
        if let Some((_, meter)) = &self.phoenix {
            keep(meter.market_fills(after_slot).await?, &[self.owner]);
        }
        if let Some((_, meter)) = &self.openbook {
            let ids = meter.fill_ids().await?;
            keep(meter.market_fills(after_slot).await?, &ids);
        }
        found.sort_by_key(|(fill, _, _)| (fill.slot, fill.instrument.0.clone(), fill.sequence));
        found
            .into_iter()
            .map(|(fill, side, role)| {
                Ok(SpotFill {
                    at: fill.slot.saturating_sub(base_slot),
                    instrument: fill.instrument.0.parse().map_err(|reason: String| {
                        Error::Transaction {
                            label: "a fill's market".into(),
                            reason,
                        }
                    })?,
                    side,
                    role,
                    price: grader_f64(fill.price),
                    base: grader_f64(fill.quantity),
                })
            })
            .collect()
    }
}

impl Venues {
    /// What traded in fills recorded after `after_slot` that aren't in
    /// `seen`: the bot's, and each seat's, by seat number.
    async fn step_flow(
        &self,
        after_slot: u64,
        seen: &mut HashSet<(String, u128)>,
        seats: &[Arc<Seat>],
    ) -> Result<StepFlow> {
        let mut fills = Vec::new();
        let mut bot = vec![self.owner];
        if let Some((_, meter)) = &self.phoenix {
            fills.extend(meter.market_fills(after_slot).await?);
        }
        if let Some((_, meter)) = &self.openbook {
            bot = meter.fill_ids().await?;
            fills.extend(meter.market_fills(after_slot).await?);
        }
        let ids: Vec<Vec<Pubkey>> = seats.iter().map(|s| s.ids()).collect();
        let mut flow = StepFlow {
            bot: Vec::new(),
            bot_passive: Vec::new(),
            seats: seats
                .iter()
                .map(|s| VenueFlow {
                    venue: s.venue.clone(),
                    ..VenueFlow::default()
                })
                .collect(),
        };
        for fill in fills {
            if !seen.insert((fill.instrument.0.clone(), fill.sequence)) {
                continue;
            }
            let venue = fill.instrument.0.split(':').next().unwrap_or_default();
            let base = grader_f64(fill.quantity);
            if let Some(side) = fill.side_for(&bot) {
                flow.add_bot(venue, side, base);
                if !bot.contains(&fill.taker) {
                    flow.add_bot_passive(venue, side, base);
                }
            }
            for (n, ids) in ids.iter().enumerate() {
                match fill.side_for(ids) {
                    Some(Side::Buy) => flow.seats[n].bought += base,
                    Some(Side::Sell) => flow.seats[n].sold += base,
                    None => {}
                }
            }
        }
        Ok(flow)
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
/// transaction, so its token account is a real native one.
pub fn fund(surfnet: &Surfnet, owner: &Keypair, base: f64, quote: f64) -> Result<()> {
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
    if wrapped > 0 {
        let (_, instructions) = token::wrap_sol(&owner.pubkey(), wrapped, true);
        mtx_kit::send(&chain::rpc_client(surfnet), owner, instructions, "wrap SOL")?;
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
    let (surfnet, _) = snapshot::start_snapshot(&snapshot_dir, snapshot::Access::Private).await?;
    let internal = Guard::internal(surfnet.rpc_url()).await?;
    let rpc = mtx_kit::rpc_client(internal.url());
    let setup_from = rpc.get_slot()?;
    let chain: Arc<dyn ChainClient> = Arc::new(RpcChainClient::new(internal.url()));

    let bot = Keypair::new();
    fund(&surfnet, &bot, spot.base, spot.quote)?;
    let venues = Venues::connect(chain, &spot.venues, bot.pubkey()).await?;
    let start = venues.read().await?;
    let instruments = spot.instruments();
    // The actors' seats, onboarded in every session of the scenario; the
    // makers quote around the level before the bot starts.
    let start_level_usd = start.wallet.price_usd;
    let level = |at: u64| start_level_usd * (1.0 + level_at(&scenario.path, at) / 100.0);
    // A family's own actors join them, from the family's plan.
    let mut hook = None;
    let mut cast = if scenario.actors.is_some() || scenario.family_actors.is_some() {
        let rpc_url = internal.url().to_string();
        let surfnet = &surfnet;
        let mut cast = Cast::prepare(
            scenario.actors.as_ref().unwrap_or(&Actors::default()),
            &spot.venues,
            &scenario.tape,
            |venue| {
                let rpc_url = rpc_url.clone();
                async move { Seat::spot(surfnet, &venue, &rpc_url, None).await }
            },
            level(0),
        )
        .await?;
        if let Some(family) = &scenario.family_actors {
            let build = family::builder(&family.family)?;
            hook = build(FamilySetup {
                surfnet,
                rpc_url: &rpc_url,
                scenario,
                plan: &family.plan,
                reference_usd: level(0),
                cast: &mut cast,
            })
            .await?;
        }
        Some(cast)
    } else {
        None
    };

    let guard = Guard::start(surfnet.rpc_url()).await?;
    if let Some(hook) = hook {
        guard.set_send_hook(hook);
    }
    let workdir = SessionDir::create(&bot, BOT_USERS[opts.slot])?;
    surfnet.send_command(SimnetCommand::UpdateBlockProductionMode(
        BlockProductionMode::Manual,
    ))?;
    let mut slot = session::manual_slot(&rpc).await?;
    let first_slot = setup_from + scenario.setup_slots();
    if slot > first_slot {
        warn!(
            slot,
            first_slot, "setup ran past the session's first slot: this session won't replay"
        );
    }
    // Pyth follows the path, or its own: each waypoint's level lands on its
    // slot.
    let no_path = Vec::new();
    let oracle_path = match &scenario.actors {
        Some(actors) if actors.oracle && !actors.oracle_path.is_empty() => &actors.oracle_path,
        Some(actors) if actors.oracle => &scenario.path,
        _ => &no_path,
    };
    if !oracle_path.is_empty() {
        let feed = pyth::feed_account(0, &pyth::SOL_USD_FEED_ID);
        let account = rpc.get_account(&feed)?;
        let update = pyth::PriceUpdate::decode_feed(&account.data, &pyth::SOL_USD_FEED_ID)
            .map_err(venue_error("pyth SOL/USD"))?;
        let scale = 10f64.powi(update.exponent);
        let overrides = oracle_path
            .iter()
            .map(|waypoint| {
                let usd = start_level_usd * (1.0 + waypoint.level_pct / 100.0);
                let raw = (usd / scale).round() as i64;
                chain::Override {
                    template_id: "pyth-price-feed-v2",
                    account: feed,
                    values: serde_json::json!({
                        "feed_id": "sol-usd",
                        "price_message.price": raw,
                        "price_message.ema_price": raw,
                    }),
                    slot: first_slot.saturating_sub(slot).max(1) + waypoint.slot,
                    label: format!("SOL/USD at {}", waypoint.slot),
                }
            })
            .collect();
        chain::register_scenario(&rpc, "oracle", overrides)?;
    }
    while slot < first_slot {
        surfnet.send_command(SimnetCommand::SlotForward(None))?;
        slot = session::wait_for_slot(&rpc, slot + 1).await?;
    }
    surfnet.cheatcodes().lock(CheatcodeFilter::all())?;
    let base_slot = rpc.get_slot()?;
    // The setup's fills (the makers clearing stale orders) aren't the
    // session's.
    let mut seen_fills = HashSet::new();
    let mut read_from = base_slot;
    if let Some(cast) = &cast {
        venues
            .step_flow(setup_from, &mut seen_fills, cast.seats())
            .await?;
    }
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
        turn_wall_ms: TURN_WALL.as_millis() as u64,
    };
    let mut turns = TurnReport {
        steps,
        ..TurnReport::default()
    };
    let legacy = !(session::send(&mut writer, &Message::Hello(hello)).await
        && matches!(
            tokio::time::timeout(HELLO_WALL, session::next_message(&mut reader)).await,
            Ok(Some(Message::Ready))
        ));
    if legacy {
        info!("the bot didn't answer the hello: the world moves at the slot time");
    }
    turns.legacy = legacy;
    let budget = TURN_BUDGET_PER_STEP * steps as u32;
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
    let mut bot_exit = None;
    let mut steps_seen: Vec<StepRecord> = Vec::with_capacity(steps as usize);
    for k in 0..steps {
        guard.set_turn(legacy);
        if let Some(cast) = &mut cast {
            let at = slot - base_slot;
            cast.before_flow(at, level(at)).await;
        }
        let target = (slot + SLOTS_PER_STEP).min(end_slot);
        while slot < target {
            surfnet.send_command(SimnetCommand::SlotForward(None))?;
            slot = session::wait_for_slot(&rpc, slot + 1).await?;
        }
        let at = slot - base_slot;
        // The bot's held sends land in this step's blocks.
        guard.release(k).await;
        let mut level_usd = None;
        if let Some(cast) = &mut cast {
            cast.crank().await;
            // A slot back, in case a transaction landed in the last read's
            // slot after it: each fill counts once either way.
            let flow = venues
                .step_flow(read_from.saturating_sub(1), &mut seen_fills, cast.seats())
                .await?;
            read_from = slot;
            cast.after_blocks(at, level(at), &flow).await;
            level_usd = Some(cast.moment(at, level(at)).reference_usd);
        }
        let now = venues.read().await?;
        note(&mut observed, &now);
        let value_usd = now.wallet.value_usd;
        peak_value_usd = peak_value_usd.max(value_usd);
        max_drawdown_usd = max_drawdown_usd.max(peak_value_usd - value_usd);
        steps_seen.push(StepRecord {
            k,
            at,
            mark_usd: now.wallet.price_usd,
            index_usd: now.wallet.price_usd,
            level_usd: level_usd.unwrap_or(now.wallet.price_usd),
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
        let step = Step {
            k,
            slot,
            last: k + 1 == steps,
        };
        let wall = TURN_WALL.min(budget.saturating_sub(spent));
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
    let fills = venues.fills(base_slot, base_slot).await?;
    let end_value_usd = end.wallet.value_usd;
    let instrument = instruments[0].clone();
    let actors = match &cast {
        Some(cast) => Some(cast.report().await),
        None => None,
    };
    let report = SessionReport {
        scenario: scenario.name.clone(),
        start_mark_usd: start_price_usd,
        end_mark_usd: end.wallet.price_usd,
        start_value_usd,
        end_value_usd,
        pnl_usd: end_value_usd - start_value_usd,
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
        }),
        actors,
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
            spot: Some(Spot {
                venues: vec!["phoenix".into(), "openbook".into()],
                base: 10.0,
                quote: 0.0,
                bound_usd: None,
            }),
            actors: None,
            family_actors: None,
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
        assert!(moving.validate().unwrap_err().contains("only with actors"));
        // With actors, a path and a tape.
        moving.actors = Some(crate::actors::Actors::default());
        moving.tape.push(crate::making::TakerOrder {
            slot: 6,
            side: crate::making::TakerSide::Sell,
            size: 1.0,
            limit_bps: 50.0,
            account: 0,
            venue: Some(crate::making::TakerVenue::Openbook),
        });
        assert_eq!(moving.validate(), Ok(()));
        let mut late = moving.clone();
        late.tape[0].slot = 40;
        assert!(late.validate().unwrap_err().contains("tape order 0"));
        let mut back = moving.clone();
        back.path.push(crate::scenario::Waypoint::at(2, 1.0));
        assert!(back.validate().unwrap_err().contains("waypoint 1"));
        let mut oracle = moving.clone();
        let actors = oracle.actors.as_mut().unwrap();
        actors.oracle = true;
        actors.oracle_path = vec![crate::scenario::Waypoint::at(6, -1.0)];
        assert_eq!(oracle.validate(), Ok(()));
        oracle.actors.as_mut().unwrap().oracle_path[0].slot = 40;
        assert!(oracle.validate().unwrap_err().contains("oracle waypoint 0"));
        let mut elsewhere = moving;
        elsewhere.spot.as_mut().unwrap().venues = vec!["phoenix".into()];
        elsewhere.mandate.universe.truncate(1);
        assert!(elsewhere.validate().unwrap_err().contains("tape order 0"));

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
}
