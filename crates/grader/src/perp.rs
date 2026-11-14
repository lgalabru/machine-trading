//! Perp sessions: a bot trading one of Phoenix Eternal's perpetuals, the
//! scenario's market, on a fresh offline world from the snapshot (see
//! [`crate::session`] for the session they share with spot sessions).
//!
//! Setup onboards a fresh trader with [`DEPOSIT_USDC`] of collateral, gives
//! it the scenario's starting position, if any, has the scenario's actors
//! ready (a market-making session's takers and the grader's own maker, an
//! off-hours session's or a tape's takers), and registers the scenario's
//! moves. Every step the session reads the account from Phoenix, never from
//! the bot: the position, the value as the scenario values it
//! ([`Scenario::valuation`]), and, where the mandate has a sides or price
//! rule or the scenario an order, every fill Phoenix recorded for it.
//!
//! A scenario's [`ExecutionOrder`](crate::orders::ExecutionOrder) has a
//! broker take what the bot leaves of it at the close, and a
//! [`CloseOut`](crate::orders::CloseOut) any position left in its market;
//! both price the close at the scenario's own level, which no bot can push.

use std::{collections::HashSet, sync::Arc};

use async_trait::async_trait;
use mtx_core::{
    AccountMeter, FillId, InstrumentId, Measurement, MeterError, Side as CoreSide,
    mandate::{InstrumentRef, Mandate, Observed, PositionRange, Term},
    turns::Step,
};
use mtx_kit::{
    BotEnv, Side,
    chain::RpcChainClient,
    phoenix_rise::{ix::PhoenixInstruction, types::exchange::ExchangeMarketConfig},
    protocols::phoenix::eternal::{PhoenixEternalMeter, PhoenixExchange},
};
use serde::{Deserialize, Serialize};
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use surfpool_sdk::{CheatcodeFilter, SimnetCommand};

use crate::{
    Error, Result,
    actors::{
        Cast, Competitor, FlowWatch, Moment, OnMarket, Pricing, SendReader, SentOrder, Takers,
        library::{
            DeskReport, RfqBoard, RfqClients, RfqSeat, Scale, Setting, Swaps, Token, WalletSeat,
        },
    },
    chain,
    guard::Guard,
    making::{TAKER_ACCOUNTS, book_at, level_at},
    orders::{CloseOutReport, OrderReport},
    phoenix::{self, Phoenix},
    risk::{CloseOrder, Closing, MAX_CLOSE_ORDERS_PER_STEP, is_flat},
    scenario::{MarketScenario, STOP_OUT_LIMIT_BPS, Scenario},
    session::{
        Account, BOT_USERS, DEPOSIT_USDC, Done, INTERNAL_RPC_TIMEOUT, IxRecord, MarketReport,
        MarketStep, OrderRecord, Prepared, SessionDir, SessionOptions, SessionReport, StepRecord,
        Timeline, World, grader_f64, patiently, wait_for_slot,
    },
    snapshot,
    venue::{Market, Seat, perps::PerpMarket, phoenix::Phoenix as PhoenixVenue},
    wasm::{Desk, Reader},
};

/// Each taker's collateral, and the grader's maker's: far more than their
/// orders need, so none fails for margin.
const ACTOR_DEPOSIT_USDC: f64 = 100_000.0;

/// How far past the start mark the bot's seeding order may fill, as a
/// share of the mark: it fills at the counterparty's price, at the mark, or
/// better.
const SEED_LIMIT: f64 = 0.02;

/// One market of a perp session, as setup found it: the scenario's part
/// for it, Phoenix's market, and where it started.
#[derive(Clone)]
struct MarketState {
    scenario: MarketScenario,
    instrument: InstrumentRef,
    id: InstrumentId,
    market: ExchangeMarketConfig,
    perp: Arc<PerpMarket>,
    start_mark_ticks: u64,
    start_mark_usd: f64,
    start_position: f64,
    /// How its position is valued (see [`Scenario::valuation`]).
    valuation: &'static str,
}

impl MarketState {
    /// The market's moment at `at` slots into the session.
    fn moment(&self, at: u64) -> Moment {
        let (path, start) = (&self.scenario.path, self.start_mark_usd);
        Moment {
            at,
            level_usd: start * (1.0 + level_at(path, at) / 100.0),
            book_usd: start * (1.0 + book_at(path, at) / 100.0),
            fair_usd: self.scenario.fair_value_usd(start, at),
        }
    }

    /// [`MarketState::moment`], as a function the actors keep.
    fn moments(&self) -> Arc<dyn Fn(u64) -> Moment + Send + Sync> {
        let market = self.clone();
        Arc::new(move |at| market.moment(at))
    }
}

/// Set a perp session up, up to the bot's start.
pub(crate) async fn prepare(opts: &SessionOptions<'_>) -> Result<Prepared> {
    let scenario = opts.scenario;
    let world = snapshot::start_offline_stepped(opts.snapshot_dir).await?;
    let wanted = scenario.market_scenarios();
    for market in wanted.iter() {
        if !world.manifest.holds(&market.symbol) {
            return Err(Error::NotWarmed {
                snapshot: opts.snapshot_dir.display().to_string(),
                symbol: market.symbol.clone(),
                held: world.manifest.symbols().join(", "),
            });
        }
    }
    let surfnet = &world.surfnet;
    // The grader's own clients, served the way a stepped world needs: a
    // transaction is final once it executed (see [`Guard::internal`]).
    let internal = Guard::internal(surfnet.rpc_url()).await?;
    let rpc = mtx_kit::rpc_client_with_timeout(internal.url(), INTERNAL_RPC_TIMEOUT);
    let setup_from = rpc.get_slot()?;
    let phoenix = Phoenix::new(world.exchange.clone(), internal.url());

    let bot = opts.keys.keypair("bot", 0);
    let trader =
        phoenix::onboard_traders_stepped(surfnet, &phoenix, &rpc, &[&bot], DEPOSIT_USDC).await?[0];
    if let Some(layer) = &scenario.tx_layer {
        crate::txlayer::setup(surfnet, layer, &bot.pubkey())?;
    }
    let hawkeye = phoenix.hawkeye(bot.pubkey());
    // Each market, in symbol order (one, for a session on one market), as
    // Phoenix offers it, and its prices at the start.
    let venue = PhoenixVenue::with_exchange(
        internal.url(),
        world.exchange.clone(),
        world.exchange_path.clone(),
        bot.pubkey(),
    );
    let mut markets = Vec::with_capacity(wanted.len());
    let mut start_quotes = std::collections::BTreeMap::new();
    for wanted in wanted.iter() {
        let symbol = &wanted.symbol;
        let market = phoenix.market(symbol)?.clone();
        let start_bbo = patiently("the book", async || hawkeye.view_bbo(symbol).await)
            .await?
            .value;
        let start_mark_ticks = start_bbo.mark_price_ticks;
        let start_mark_usd = phoenix.ticks_to_usd(&market, start_mark_ticks);
        crate::oracles::check_index_gap(
            symbol,
            start_mark_ticks as f64,
            start_bbo.index_price_ticks as f64,
        )?;
        let instrument = wanted.instrument();
        start_quotes.insert(
            instrument.clone(),
            crate::oracles::MarketQuote {
                mark: Some(start_mark_usd),
                index: Some(phoenix.ticks_to_usd(&market, start_bbo.index_price_ticks)),
                mid: crate::oracles::MarketQuote::mid_of(
                    start_bbo
                        .best_bid_ticks()
                        .map(|t| phoenix.ticks_to_usd(&market, t)),
                    start_bbo
                        .best_ask_ticks()
                        .map(|t| phoenix.ticks_to_usd(&market, t)),
                ),
            },
        );
        markets.push(MarketState {
            id: instrument.id(),
            instrument,
            perp: Arc::new(venue.perp(symbol)?),
            start_mark_ticks,
            start_mark_usd,
            start_position: scenario.start_positions.get(symbol).copied().unwrap_or(0.0),
            valuation: wanted.valuation(scenario.prices_the_close()),
            market,
            scenario: wanted.clone(),
        });
    }
    let primary = markets
        .iter()
        .position(|m| m.scenario.symbol == scenario.symbol)
        .expect("the primary market is one of the session's");
    // The account, as an enforcer reads it: from Phoenix, never from the bot,
    // on every market of the session. A scenario that sets funding values
    // it with the funding it has accrued and not settled yet; others as
    // Phoenix's collateral and unrealized PnL alone.
    let symbols: Vec<&str> = markets.iter().map(|m| m.scenario.symbol.as_str()).collect();
    let funded = markets.iter().any(|m| m.scenario.funding.is_some());
    let meter = PhoenixEternalMeter::for_markets(
        Arc::new(PhoenixExchange::new(world.exchange.clone())),
        internal.url(),
        Arc::new(RpcChainClient::with_timeout(
            internal.url(),
            INTERNAL_RPC_TIMEOUT,
        )),
        bot.pubkey(),
        &symbols,
    )
    .map_err(mtx_kit::Error::from)?
    .with_unsettled_funding(funded);
    if meter.trader() != trader {
        return Err(Error::InvalidExchangeKey(format!(
            "the meter would read trader account {}, but the bot was onboarded at {trader}",
            meter.trader()
        )));
    }
    let universe: Vec<InstrumentId> = scenario
        .mandate
        .universe
        .iter()
        .map(|allowance| allowance.instrument.id())
        .collect();
    for market in &markets {
        if let Some(mark) = market
            .scenario
            .funding
            .as_ref()
            .and_then(|f| f.mark.as_ref())
        {
            phoenix::set_mark(surfnet, &phoenix, &market.scenario.symbol, mark)?;
        }
    }
    for (i, market) in markets.iter().enumerate() {
        if market.start_position != 0.0 {
            let seed = Seed {
                world: &world,
                phoenix: &phoenix,
                perp: &market.perp,
                n: i as u32,
                market_id: &market.id,
                rpc: &rpc,
                rpc_url: internal.url(),
            };
            seed.position(
                opts,
                &bot,
                &meter,
                &universe,
                market.start_mark_usd,
                market.start_position,
            )
            .await?;
        }
    }
    // The actors, set up while cheatcodes still work: market making's
    // takers, with the makers widened, and a competing maker; off hours'
    // takers, with the makers widened and no competitor; a tape's takers,
    // with the makers as they are. On several markets, each market's, with
    // one pool of taker accounts for them all, as one firm trading every
    // market would; a market's actors other than the primary's act at that
    // market's moments ([`OnMarket`]).
    let mut cast = Cast::default();
    let mut flows = Vec::new();
    for market in &markets {
        let symbol = &market.scenario.symbol;
        let widen = |half_spread_bps: f64| -> Result<u64> {
            let half_spread_ticks =
                (market.start_mark_ticks as f64 * half_spread_bps / 10_000.0).round() as u64;
            phoenix::widen_makers(surfnet, &phoenix, symbol, half_spread_ticks)
        };
        let pricing = market.scenario.takers_pricing;
        let offhours = scenario
            .offhours
            .as_ref()
            .filter(|_| scenario.markets.is_empty());
        let flow = match (&market.scenario.making, offhours) {
            (Some(making), _) => Some((
                widen(making.competitor_bps)?,
                making.takers.clone(),
                pricing.unwrap_or(Pricing::Level),
            )),
            (None, Some(off)) => Some((
                widen(off.half_spread_bps)?,
                off.takers.clone(),
                pricing.unwrap_or(Pricing::Level),
            )),
            (None, None) if !market.scenario.tape.is_empty() => Some((
                0,
                market.scenario.tape.clone(),
                pricing.unwrap_or(Pricing::Book),
            )),
            (None, None) => None,
        };
        flows.push(flow);
    }
    let on_market =
        |i: usize, actor: Box<dyn crate::actors::Actor>| -> Box<dyn crate::actors::Actor> {
            if i == primary {
                actor
            } else {
                Box::new(OnMarket::new(
                    markets[i].scenario.symbol.clone(),
                    actor,
                    markets[i].moments(),
                ))
            }
        };
    if let Some(first) = flows.iter().position(Option::is_some) {
        let keypairs: Vec<Keypair> = (0..TAKER_ACCOUNTS)
            .map(|i| opts.keys.keypair("taker", i as u32))
            .collect();
        let pool: Vec<Keypair> = keypairs.iter().map(Keypair::insecure_clone).collect();
        let mut seats = Some(
            onboard(
                &world,
                &phoenix,
                &markets[first].perp,
                &rpc,
                internal.url(),
                keypairs,
            )
            .await?,
        );
        for (i, flow) in flows.iter().enumerate() {
            let Some((widened_ticks, orders, pricing)) = flow else {
                continue;
            };
            let seats = match seats.take().filter(|_| i == first) {
                Some(seats) => seats,
                None => market_seats(&markets[i].perp, &pool, internal.url()).await?,
            };
            tracing::info!(
                market = markets[i].scenario.symbol,
                takers = seats.len(),
                widened_ticks,
                "takers ready"
            );
            cast.add(on_market(
                i,
                Box::new(Takers::new(seats, orders.clone(), *pricing, *widened_ticks)),
            ));
        }
    }
    for (i, market) in markets.iter().enumerate() {
        if let Some(making) = &market.scenario.making {
            let keypair = opts.keys.keypair("competitor", i as u32);
            let seat = onboard(
                &world,
                &phoenix,
                &market.perp,
                &rpc,
                internal.url(),
                vec![keypair],
            )
            .await?
            .remove(0);
            cast.add(on_market(
                i,
                Box::new(Competitor::new(
                    seat,
                    making,
                    opts.keys.seed("competitor", i as u32),
                )),
            ));
        }
    }
    let own = &markets[primary];
    // The library's actors: a wallet each, onboarded like the takers. They
    // are built once the session's first slot is known. They trade the
    // primary market.
    let library = match &scenario.actors {
        Some(library) if !library.wallets().is_empty() => {
            let keypairs: Vec<Keypair> = library
                .wallets()
                .into_iter()
                .map(|(role, n)| opts.keys.keypair(role, n))
                .collect();
            let pubkeys: Vec<Pubkey> = keypairs.iter().map(Signer::pubkey).collect();
            let seats =
                onboard_unique(&world, &phoenix, &own.perp, &rpc, internal.url(), keypairs).await?;
            tracing::info!(wallets = seats.len(), "library actors ready");
            let seats: Vec<WalletSeat> = pubkeys
                .into_iter()
                .zip(seats)
                .map(|(pubkey, seat)| WalletSeat { pubkey, seat })
                .collect();
            Some((library, seats))
        }
        _ => None,
    };
    // RFQ fills settle as swaps of the asset's spot token for USDC, between
    // the clients' wallets and the bot's: they hold both, funded here.
    let swaps = match library.as_ref().and_then(|(l, _)| l.rfq_clients.as_ref()) {
        Some(rfq) => {
            let scale = library.as_ref().map(|(l, _)| l.scale).unwrap_or_default();
            let setup = RfqSetup {
                symbol: &scenario.symbol,
                rfq,
                scale,
                price_usd: own.start_mark_usd,
            };
            Some(fund_rfq(opts, surfnet, &rpc, internal.url(), &bot, setup)?)
        }
        None => None,
    };
    // The account's value at the start, once setup has funded everything
    // the bot holds.
    let start_value_usd = grader_f64(
        patiently("the meter", async || meter.measure(&universe).await)
            .await?
            .value_usd,
    );

    let guard = Guard::start(surfnet.rpc_url()).await?;
    let user = opts.slot.map(|slot| BOT_USERS[slot]);
    let workdir = SessionDir::create(&bot, user)?;
    let mut slot = rpc.get_slot()?;
    // Slots from the surfnet's start to the session's. Setup produces a few
    // of its own (each onboarding, one), the same ones every run; the world
    // then steps to this one, so every session of a scenario kind starts at
    // the same distance from the snapshot: state that depends on the slot
    // (Phoenix's funding, the oracle's age) replays the same.
    let first_slot = setup_from + scenario.setup_slots();
    if slot > first_slot {
        return Err(Error::SetupSlots {
            scenario: scenario.name.clone(),
            used: slot - setup_from,
            budget: scenario.setup_slots(),
        });
    }
    // Each market's funding parameters land on the session's first slot,
    // so they're in force, and their intervals start at the same point,
    // before the bot starts.
    let perp_asset_map = phoenix.perp_asset_map()?;
    let funding: Vec<_> = markets
        .iter()
        .filter_map(|m| Some((m.scenario.symbol.as_str(), m.scenario.funding.as_ref()?)))
        .collect();
    if !funding.is_empty() {
        chain::register_scenario(
            &rpc,
            "funding",
            funding
                .iter()
                .map(|(symbol, funding)| {
                    crate::scenario::funding_override(
                        perp_asset_map,
                        symbol,
                        funding,
                        first_slot.saturating_sub(slot).max(1),
                    )
                })
                .collect(),
        )?;
    }
    // Surfpool restamps Phoenix's oracle readings, and the slot its mark's
    // EMA last stepped at, with the current slot whenever it plays a Phoenix
    // override (an onboarding's capabilities, say), and the EMA's next step
    // runs from there. Setup's slots no longer move, but one more override on
    // the session's first slot restamps them all to it, so the session's
    // mark doesn't depend on how many onboardings setup ran either. A
    // funding scenario's own override, played there, does the same.
    if funding.is_empty() {
        chain::register_scenario(
            &rpc,
            "readings",
            vec![phoenix::enable_trader(
                trader,
                first_slot.saturating_sub(slot).max(1),
            )],
        )?;
    }
    while slot < first_slot {
        surfnet.send_command(SimnetCommand::SlotForward(None))?;
        slot = wait_for_slot(&rpc, slot + 1).await?;
    }
    // The scenario is the grader's last cheatcode. Lock them all before the
    // bot starts: the bot's only way to the surfnet is the guard, and even
    // the guard's upstream has no cheatcodes, airdrops, or way to stop it.
    // Overrides count from the slot they were registered at; so does the
    // takers' flow.
    if !funding.is_empty() {
        while slot < first_slot + 1 {
            surfnet.send_command(SimnetCommand::SlotForward(None))?;
            slot = wait_for_slot(&rpc, slot + 1).await?;
        }
        for (symbol, funding) in &funding {
            phoenix::check_funding(&rpc, &phoenix, symbol, funding)?;
        }
    }
    // Each market's moves, in symbol order. A move's overrides carry their
    // slots, so the order they are registered in doesn't change when they
    // land.
    let overrides = if scenario.markets.is_empty() {
        scenario.overrides(perp_asset_map, own.start_mark_ticks)
    } else {
        markets
            .iter()
            .flat_map(|m| m.scenario.overrides(perp_asset_map, m.start_mark_ticks))
            .collect()
    };
    chain::register_scenario(&rpc, &scenario.name, overrides)?;
    let oracle_starts = match &scenario.oracles {
        Some(oracles) => crate::oracles::register(
            &rpc,
            oracles,
            scenario.duration_slots,
            &crate::oracles::World {
                phoenix: Some((perp_asset_map, own.start_mark_ticks)),
                phoenix_marks: markets
                    .iter()
                    .map(|m| (m.scenario.symbol.clone(), m.start_mark_ticks))
                    .collect(),
                prices: start_quotes,
            },
        )?,
        None => Default::default(),
    };
    let base_slot = rpc.get_slot()?;
    surfnet.cheatcodes().lock(CheatcodeFilter::all())?;
    let end_slot = base_slot + scenario.duration_slots;
    let mut rfqs = None;
    if let Some((library, seats)) = library {
        let path = own.moments();
        // Front-runners read the bot's trading through a seat on its
        // account, and its sends as they arrive.
        let send_reader = (!library.front_runners.is_empty())
            .then(|| send_reader(own.market.clone(), internal.url().to_string()));
        if !library.front_runners.is_empty() {
            cast.watch(FlowWatch::new(vec![
                own.perp.seat(bot.insecure_clone(), internal.url()).await?,
            ]));
        }
        let built = library.build(
            seats,
            Setting {
                symbol: scenario.symbol.clone(),
                base_slot,
                path,
                settlement: swaps
                    .as_ref()
                    .map(|(swaps, _)| swaps.clone() as Arc<dyn crate::actors::library::Settlement>),
                send_reader,
                liquidations: None,
                rpc_url: Some(internal.url().to_string()),
            },
            &|role, n| opts.keys.seed(role, n),
        );
        for actor in built.actors {
            cast.add(actor);
        }
        rfqs = built.rfq;
    }
    // The bot gets a copy to plan by; the audit uses this one, which the bot
    // can't reach.
    let mandate = Mandate {
        term: Some(Term {
            until_slot: end_slot,
        }),
        ..scenario.mandate.clone()
    };
    let env = BotEnv {
        rpc_url: guard.url().to_string(),
        keypair_path: workdir.keypair_path(),
        exchange_path: world.exchange_path.clone(),
        symbol: scenario.symbol.clone(),
        end_slot,
        max_position: scenario.max_position(),
        max_drawdown_usd: mandate
            .limits
            .max_drawdown_usd
            .expect("a perp scenario's mandate has a drawdown limit"),
        mandate_path: Some(workdir.write_mandate(&mandate)?),
        // The primary first, then the rest in symbol order.
        markets: if scenario.markets.is_empty() {
            Vec::new()
        } else {
            std::iter::once(scenario.symbol.clone())
                .chain(
                    markets
                        .iter()
                        .map(|m| m.scenario.symbol.clone())
                        .filter(|s| *s != scenario.symbol),
                )
                .collect()
        },
    };
    let reads_fills = scenario.order.is_some()
        || scenario.mandate.universe.iter().any(|allowance| {
            allowance.sides.is_some()
                || allowance.min_sell_price.is_some()
                || allowance.max_buy_price.is_some()
        });
    let mut observed = Observed::default();
    for market in &markets {
        if market.start_position != 0.0 {
            observed.begin(&market.instrument, market.start_position);
        }
    }
    let vars = session_vars(scenario)?;
    let mut account = PerpAccount {
        scenario: scenario.clone(),
        phoenix,
        bot: bot.pubkey(),
        meter,
        rpc_url: internal.url().to_string(),
        universe,
        markets,
        primary,
        start_value_usd,
        base_slot,
        observed,
        fills: reads_fills.then(|| PerpFills {
            last_scan: base_slot,
            ..PerpFills::default()
        }),
        oracle_starts,
        peak_value_usd: start_value_usd,
        max_drawdown_usd: 0.0,
        positions_outside_universe: 0,
        rfqs,
        desk: swaps.map(|(_, desk)| desk),
        desk_start: None,
    };
    // A starting position is worth what the session values it at, from
    // the start: Phoenix's mark at the seeding is not the scenario's level.
    let starts: Vec<f64> = account.markets.iter().map(|m| m.start_position).collect();
    if starts.iter().any(|&start| start != 0.0) {
        let raw = grader_f64(account.measure().await?.value_usd);
        account.start_value_usd = account.value(raw, &starts, base_slot).await?;
        account.peak_value_usd = account.start_value_usd;
    }
    // The desk's asset counts toward the account from the start.
    if let Some(start) = account.desk_holdings().await? {
        account.start_value_usd += start.base_usd();
        account.peak_value_usd = account.start_value_usd;
        account.desk_start = Some(start);
    }
    Ok(Prepared {
        world: World::Offline(Box::new(world)),
        internal,
        rpc,
        guard,
        workdir,
        bot,
        base_slot,
        end_slot,
        mandate,
        env,
        reports: None,
        // Off hours: the slot the external market reopens at, from the
        // venue's calendar, as `MTX_REOPEN_SLOT`.
        reopen_slot: scenario
            .offhours
            .as_ref()
            .map(|off| base_slot + off.reopen_slot),
        vars,
        cast,
        cranks: Vec::new(),
        account: Box::new(account),
    })
}

/// A seat on `perp` for each of `keypairs`, already onboarded, trading
/// through `rpc_url`: a pool's accounts on another of the session's
/// markets.
async fn market_seats(
    perp: &PerpMarket,
    keypairs: &[Keypair],
    rpc_url: &str,
) -> Result<Vec<Box<dyn Seat>>> {
    let mut seats = Vec::with_capacity(keypairs.len());
    for keypair in keypairs {
        seats.push(perp.seat(keypair.insecure_clone(), rpc_url).await?);
    }
    Ok(seats)
}

/// Onboard an account on the perp for each of `keypairs`, with
/// [`ACTOR_DEPOSIT_USDC`] of collateral, and a seat for each trading
/// through `rpc_url`, the grader's own. Setup, on a stepped surfnet:
/// cheatcodes must still be unlocked.
async fn onboard(
    world: &snapshot::OfflineWorld,
    phoenix: &Phoenix,
    perp: &PerpMarket,
    rpc: &solana_rpc_client::rpc_client::RpcClient,
    rpc_url: &str,
    keypairs: Vec<Keypair>,
) -> Result<Vec<Box<dyn Seat>>> {
    phoenix::onboard_traders_stepped(
        &world.surfnet,
        phoenix,
        rpc,
        &keypairs.iter().collect::<Vec<_>>(),
        ACTOR_DEPOSIT_USDC,
    )
    .await?;
    let mut seats = Vec::with_capacity(keypairs.len());
    for keypair in keypairs {
        seats.push(perp.seat(keypair, rpc_url).await?);
    }
    Ok(seats)
}

/// As [`onboard`], with seats whose every transaction is unique among their
/// own ([`crate::venue::perps::UniquePerpSeat`]): the library's actors'.
async fn onboard_unique(
    world: &snapshot::OfflineWorld,
    phoenix: &Phoenix,
    perp: &PerpMarket,
    rpc: &solana_rpc_client::rpc_client::RpcClient,
    rpc_url: &str,
    keypairs: Vec<Keypair>,
) -> Result<Vec<Box<dyn Seat>>> {
    phoenix::onboard_traders_stepped(
        &world.surfnet,
        phoenix,
        rpc,
        &keypairs.iter().collect::<Vec<_>>(),
        ACTOR_DEPOSIT_USDC,
    )
    .await?;
    keypairs
        .into_iter()
        .map(|keypair| perp.unique_seat(keypair, rpc_url))
        .collect()
}

/// What setup gives a starting position with: on `perp`, the session's
/// `n`th market, whose id is `market_id`.
struct Seed<'a> {
    world: &'a snapshot::OfflineWorld,
    phoenix: &'a Phoenix,
    perp: &'a PerpMarket,
    n: u32,
    market_id: &'a InstrumentId,
    rpc: &'a solana_rpc_client::rpc_client::RpcClient,
    rpc_url: &'a str,
}

impl Seed<'_> {
    /// Give the bot `position` base units of the seeder's market, before
    /// it starts: a real fill on Phoenix, so Phoenix's margin, funding and
    /// liquidation see the position. A counterparty of the grader's (role
    /// `seed`, numbered by the market's place in the session) rests the
    /// other side post-only at the start mark,
    /// `mark_usd`; the grader sends the bot's immediate-or-cancel order for
    /// the position with the bot's own key, which it holds; the counterparty
    /// cancels what is left and never trades again. The fill's fee is paid
    /// before the session measures its start value. Phoenix refuses a
    /// position the collateral can't margin, and setup fails then, as it
    /// does if the bot doesn't end up holding the position to the lot or
    /// could be liquidated. The fill lands a slot before the session reads
    /// any fills. Setup: cheatcodes must still work.
    async fn position(
        &self,
        opts: &SessionOptions<'_>,
        bot: &Keypair,
        meter: &PhoenixEternalMeter,
        universe: &[InstrumentId],
        mark_usd: f64,
        position: f64,
    ) -> Result<()> {
        let failed = Error::StartPosition;
        let counterparty = opts.keys.keypair("seed", self.n);
        phoenix::onboard_traders_stepped(
            &self.world.surfnet,
            self.phoenix,
            self.rpc,
            &[&counterparty],
            ACTOR_DEPOSIT_USDC,
        )
        .await?;
        let counterparty = self
            .perp
            .trader(counterparty, self.rpc_url)?
            .with_timeout(INTERNAL_RPC_TIMEOUT);
        let own = self
            .perp
            .trader(bot.insecure_clone(), self.rpc_url)?
            .with_timeout(INTERNAL_RPC_TIMEOUT);
        let size = position.abs();
        let (side, limit) = if position > 0.0 {
            (mtx_kit::Side::Bid, mark_usd * (1.0 + SEED_LIMIT))
        } else {
            (mtx_kit::Side::Ask, mark_usd * (1.0 - SEED_LIMIT))
        };
        let other = match side {
            mtx_kit::Side::Bid => mtx_kit::Side::Ask,
            mtx_kit::Side::Ask => mtx_kit::Side::Bid,
        };
        counterparty
            .place_post_only(other, mark_usd, size)
            .await
            .map_err(|e| failed(format!("the counterparty's order: {e}")))?;
        own.place_market(side, size, limit)
            .await
            .map_err(|e| failed(format!("the bot's order for {position}: {e}")))?;
        counterparty
            .cancel_all()
            .map_err(|e| failed(format!("the counterparty's cancel: {e}")))?;
        let held = position_in(
            &patiently("the meter", || meter.measure(universe)).await?,
            self.market_id,
        );
        let market = self.phoenix.market(&self.perp.instrument().symbol)?;
        let lot = mtx_kit::venue::lots_to_base_units(market, 1);
        if (held - position).abs() > lot / 2.0 {
            return Err(failed(format!(
                "the bot holds {held}, not {position}: the book or the collateral didn't take it"
            )));
        }
        if patiently("the meter", || meter.liquidatable()).await? {
            return Err(failed(format!(
                "{position} is more than the collateral margins"
            )));
        }
        chain::next_slot(&self.world.surfnet, self.rpc).await?;
        Ok(())
    }
}

/// What the scenario tells a perp session's bot beyond the runtime
/// contract, as session variables: `MTX_START_POSITIONS`, its starting
/// positions (`{symbol: base units}`); `MTX_ORDER`, its order without the
/// haircut, with `MTX_BROKER_HAIRCUT_BPS`; and `MTX_CLOSE_OUT`, each
/// close-out's haircut (`{symbol: bps}`). Each only when the scenario has
/// it.
fn session_vars(scenario: &Scenario) -> Result<Vec<(String, String)>> {
    let mut vars = Vec::new();
    if !scenario.start_positions.is_empty() {
        vars.push((
            "MTX_START_POSITIONS".to_string(),
            serde_json::to_string(&scenario.start_positions)?,
        ));
    }
    if let Some(order) = &scenario.order {
        let mut order = order.clone();
        order
            .instrument
            .get_or_insert_with(|| scenario.instrument());
        let mut told = serde_json::to_value(&order)?;
        if let Some(fields) = told.as_object_mut() {
            fields.remove("haircut_bps");
        }
        vars.push(("MTX_ORDER".to_string(), told.to_string()));
        vars.push((
            "MTX_BROKER_HAIRCUT_BPS".to_string(),
            order.haircut_bps.to_string(),
        ));
    }
    if !scenario.close_out.is_empty() {
        let haircuts: std::collections::BTreeMap<&String, f64> = scenario
            .close_out
            .iter()
            .map(|(symbol, close_out)| (symbol, close_out.haircut_bps))
            .collect();
        vars.push((
            "MTX_CLOSE_OUT".to_string(),
            serde_json::to_string(&haircuts)?,
        ));
    }
    Ok(vars)
}

/// One of the bot's fills on a perp market, as Phoenix recorded it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PerpFillRecord {
    /// Slots into the session it landed at.
    pub at: u64,
    pub instrument: InstrumentRef,
    /// The bot's side, as the taker or the maker.
    pub side: CoreSide,
    /// Price in USDC per base unit, before fees, and size in base units.
    pub price: f64,
    pub base: f64,
    /// A fill of the risk desk's forced close (the scenario's `stop_out`),
    /// sent in the bot's account but not by the bot: it counts toward the
    /// position and the PnL, and the sides and price rules leave it out.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub forced: bool,
}

/// The bot's fills and resting sides, read as the session goes.
#[derive(Default)]
struct PerpFills {
    /// The slot fills were last read at. Each read starts a slot earlier,
    /// for what landed in that slot after it, and skips fills it has.
    last_scan: u64,
    seen: HashSet<FillId>,
    fills: Vec<PerpFillRecord>,
    /// Each fill's id, as Phoenix's events give it, in the same order.
    ids: Vec<FillId>,
    open_orders: Vec<(InstrumentRef, CoreSide)>,
}

/// A perp session's account: the bot's Phoenix trader account, metered on
/// every market of the session.
struct PerpAccount {
    scenario: Scenario,
    phoenix: Phoenix,
    bot: Pubkey,
    meter: PhoenixEternalMeter,
    /// The grader's own endpoint, for the risk desk's transactions.
    rpc_url: String,
    universe: Vec<InstrumentId>,
    /// The session's markets, in symbol order, and which is the primary:
    /// the scenario's `symbol`, the one the report's one-market fields and
    /// the actors' moments are.
    markets: Vec<MarketState>,
    primary: usize,
    oracle_starts: std::collections::BTreeMap<String, crate::oracles::OracleStart>,
    start_value_usd: f64,
    base_slot: u64,
    /// Sampled every step: a fill persists until the bot trades out of it,
    /// so a position over the cap is caught. With a starting position, the
    /// range starts there.
    observed: Observed,
    /// The bot's fills, where the session reads them.
    fills: Option<PerpFills>,
    /// The account's value, sampled at the same cadence: its running peak,
    /// and the deepest it has fallen below it.
    peak_value_usd: f64,
    max_drawdown_usd: f64,
    /// Phoenix positions (or orders) in markets other than the session's.
    positions_outside_universe: u32,
    /// The requests for quotes open to the bot, if RFQ clients send any.
    rfqs: Option<RfqBoard>,
    /// What its RFQ fills settle in, if they do, and what the desk held at
    /// the start.
    desk: Option<RfqDesk>,
    desk_start: Option<DeskHoldings>,
}

/// The tokens a bot's RFQ fills move in its wallet, read at `rpc_url`.
struct RfqDesk {
    base: Token,
    quote: Token,
    rpc_url: String,
}

/// What the bot's wallet holds for its RFQ desk at one moment: the asset
/// and USDC, and the asset's reference price.
#[derive(Debug, Clone, Copy)]
struct DeskHoldings {
    base: f64,
    usdc: f64,
    index_usd: f64,
}

impl DeskHoldings {
    fn base_usd(&self) -> f64 {
        self.base * self.index_usd
    }
}

/// What a session's RFQ clients trade, and at about what price.
struct RfqSetup<'a> {
    symbol: &'a str,
    rfq: &'a RfqClients,
    scale: Scale,
    price_usd: f64,
}

/// The spot token an RFQ on a perp's asset settles in: the asset's own
/// mint. SPCX is Backpack's (a Token-2022 mint with 6 decimals).
fn spot_token(symbol: &str) -> Option<Token> {
    match symbol {
        "SPCX" => Some(Token {
            mint: solana_pubkey::pubkey!("SPCXxcqXj6e5dJDVNovHN8744zkbhM2bYudU45BimGb"),
            program: mtx_kit::token::TOKEN_2022_PROGRAM_ID,
            decimals: 6,
        }),
        _ => None,
    }
}

/// Fund an RFQ session's wallets, while cheatcodes work: each client with
/// the asset and USDC for a hundred requests of the median size, and the
/// bot with the desk the scenario gives it (`desk_base` of the asset, and
/// `desk_usdc` on top of the USDC its wallet holds). The snapshot doesn't
/// carry the asset's mint, so setup writes a plain one at its address,
/// without the real mint's extensions (a transfer hook, accounts frozen by
/// default, pausing, a permanent delegate): a swap between two wallets
/// needs none of them. The swaps the clients settle with, and the bot's
/// desk.
fn fund_rfq(
    opts: &SessionOptions<'_>,
    surfnet: &surfpool_sdk::Surfnet,
    rpc: &solana_rpc_client::rpc_client::RpcClient,
    rpc_url: &str,
    bot: &Keypair,
    setup: RfqSetup<'_>,
) -> Result<(Arc<Swaps>, RfqDesk)> {
    let base = spot_token(setup.symbol).ok_or_else(|| Error::Transaction {
        label: "RFQ clients".into(),
        reason: format!("no spot token for {} to settle in", setup.symbol),
    })?;
    let quote = Token {
        mint: phoenix::USDC_MINT,
        program: mtx_kit::token::TOKEN_PROGRAM_ID,
        decimals: 6,
    };
    let cheats = surfnet.cheatcodes();
    if rpc.get_account(&base.mint).is_err() {
        cheats.set_account(
            &base.mint,
            crate::simulated::MINT_LAMPORTS,
            &crate::simulated::mint_data(base.decimals),
            &base.program,
        )?;
    }
    let atoms =
        |units: f64, token: &Token| (units * 10f64.powi(token.decimals as i32)).round() as u64;
    let request = setup.scale.trade_size * setup.rfq.size_mult;
    let clients: Vec<Keypair> = (0..setup.rfq.clients)
        .map(|i| opts.keys.keypair("rfq-client", i))
        .collect();
    for client in &clients {
        let owner = client.pubkey();
        cheats.fund_token(
            &owner,
            &base.mint,
            atoms(100.0 * request, &base),
            Some(&base.program),
        )?;
        let usdc = atoms(100.0 * request * setup.price_usd, &quote);
        cheats.fund_token(&owner, &quote.mint, usdc, Some(&quote.program))?;
    }
    let owner = bot.pubkey();
    let held: u64 = rpc
        .get_token_account_balance(&quote.account(&owner))?
        .amount
        .parse()
        .unwrap_or(0);
    cheats.fund_token(
        &owner,
        &base.mint,
        atoms(setup.rfq.desk_base, &base),
        Some(&base.program),
    )?;
    let usdc = held + atoms(setup.rfq.desk_usdc, &quote);
    cheats.fund_token(&owner, &quote.mint, usdc, Some(&quote.program))?;
    let swaps = Swaps::new(
        rpc_url.to_string(),
        bot.insecure_clone(),
        clients,
        base,
        quote,
    );
    let desk = RfqDesk {
        base,
        quote,
        rpc_url: rpc_url.to_string(),
    };
    Ok((Arc::new(swaps), desk))
}

/// What `owner`'s token accounts of `token` hold, in whole units, as the
/// meter counts a wallet's USDC: none once it has closed them.
async fn wallet_holds(
    chain: &dyn mtx_kit::chain::ChainClient,
    owner: &Pubkey,
    token: &Token,
) -> Result<f64> {
    let atoms = mtx_kit::chain::token::owned_amount(chain, owner, &token.mint)
        .await
        .map_err(|e| Error::Transaction {
            label: "the RFQ desk's balances".into(),
            reason: e.to_string(),
        })?;
    Ok(atoms as f64 / 10f64.powi(token.decimals as i32))
}

impl PerpAccount {
    fn own(&self) -> &MarketState {
        &self.markets[self.primary]
    }

    async fn measure(&self) -> Result<Measurement> {
        Ok(patiently("the meter", || self.meter.measure(&self.universe)).await?)
    }

    /// Each market's position in `measurement`, in the markets' order.
    fn positions(&self, measurement: &Measurement) -> Vec<f64> {
        self.markets
            .iter()
            .map(|m| position_in(measurement, &m.id))
            .collect()
    }

    /// Mark the fills of the risk desk's forced closes: each close order's
    /// transaction, by the signature the desk recorded, read from the chain
    /// and decoded as the meter decodes fills, on every market of the
    /// session, so the ids match.
    async fn mark_forced(&mut self, closes: &[crate::risk::ForcedClose]) -> Result<()> {
        let Some(read) = self.fills.as_mut() else {
            return Ok(());
        };
        let signatures: Vec<&String> = closes
            .iter()
            .flat_map(|close| &close.orders)
            .filter_map(|order| order.signature.as_ref())
            .collect();
        if signatures.is_empty() {
            return Ok(());
        }
        let failed = |reason: String| Error::Transaction {
            label: "the stop-out's fills".into(),
            reason,
        };
        let chain = RpcChainClient::with_timeout(&self.rpc_url, INTERNAL_RPC_TIMEOUT);
        // A close's transaction may trade any of the session's markets.
        let markets = self
            .markets
            .iter()
            .map(|m| self.phoenix.exchange().market(&m.scenario.symbol))
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(mtx_kit::Error::from)?;
        let mut records = mtx_kit::protocols::phoenix::eternal::PerpMarketRecords::default();
        for signature in signatures {
            let parsed = signature
                .parse()
                .map_err(|e| failed(format!("signature {signature}: {e}")))?;
            let summary = mtx_kit::chain::ChainClient::get_transaction(&chain, &parsed)
                .await
                .map_err(|e| failed(e.to_string()))?
                .ok_or_else(|| failed(format!("transaction {signature} not found")))?;
            for market in &markets {
                records.add_transaction(&summary, market);
            }
        }
        let forced: HashSet<FillId> = records
            .fills_for(&[self.meter.trader()])
            .into_iter()
            .map(|recorded| recorded.fill.id)
            .collect();
        for (fill, id) in read.fills.iter_mut().zip(&read.ids) {
            fill.forced = forced.contains(id);
        }
        Ok(())
    }

    /// Read the fills Phoenix recorded for the bot, and the orders it left
    /// resting, since the last read, at `slot`. Nothing where the session
    /// doesn't read them.
    async fn scan(&mut self, slot: u64) -> Result<()> {
        let Some(read) = self.fills.as_mut() else {
            return Ok(());
        };
        let after = read.last_scan.saturating_sub(1);
        let records = patiently("the fills", || self.meter.records(after)).await?;
        let named = |id: &InstrumentId| {
            self.markets
                .iter()
                .find(|m| m.id == *id)
                .map_or_else(|| self.scenario.instrument(), |m| m.instrument.clone())
        };
        for recorded in records.fills {
            if read.seen.insert(recorded.fill.id.clone()) {
                read.fills.push(PerpFillRecord {
                    at: recorded.slot.saturating_sub(self.base_slot),
                    instrument: named(&recorded.fill.instrument),
                    side: recorded.fill.side,
                    price: grader_f64(recorded.fill.price),
                    base: grader_f64(recorded.fill.quantity),
                    forced: false,
                });
                read.ids.push(recorded.fill.id);
            }
        }
        for placed in records.placed {
            let seen = (named(&placed.instrument), placed.side);
            if !read.open_orders.contains(&seen) {
                read.open_orders.push(seen);
            }
        }
        read.last_scan = slot;
        Ok(())
    }

    /// What the bot's wallet holds for its RFQ desk, and the asset's
    /// reference price, Phoenix's index for the perp. Nothing without a
    /// desk. The wallet holds what its token accounts of each mint hold, as
    /// the meter counts its USDC: none if the bot closed them.
    async fn desk_holdings(&self) -> Result<Option<DeskHoldings>> {
        let Some(desk) = &self.desk else {
            return Ok(None);
        };
        let chain = RpcChainClient::with_timeout(&desk.rpc_url, INTERNAL_RPC_TIMEOUT);
        let base = wallet_holds(&chain, &self.bot, &desk.base).await?;
        let usdc = wallet_holds(&chain, &self.bot, &desk.quote).await?;
        let hawkeye = self.phoenix.hawkeye(self.bot);
        let symbol = &self.scenario.symbol;
        let index = patiently("the book", || hawkeye.view_bbo(symbol))
            .await?
            .value
            .index_price_ticks;
        Ok(Some(DeskHoldings {
            base,
            usdc,
            index_usd: self.phoenix.ticks_to_usd(&self.own().market, index),
        }))
    }

    /// What the desk adds to the account's value: its asset, as the spot
    /// profile values holdings, at the reference price. Its USDC is already
    /// in the meter's value, with the rest of the wallet's.
    async fn desk_usd(&self) -> Result<f64> {
        Ok(self
            .desk_holdings()
            .await?
            .map_or(0.0, |held| held.base_usd()))
    }

    async fn mark_ticks(&self, market: &MarketState) -> Result<u64> {
        let hawkeye = self.phoenix.hawkeye(self.bot);
        let symbol = &market.scenario.symbol;
        Ok(patiently("the book", || hawkeye.view_bbo(symbol))
            .await?
            .value
            .mark_price_ticks)
    }

    /// What the account is worth at `slot`, given Phoenix's value
    /// `value_usd` and each market's position (`units`, in the markets'
    /// order): each position at its market's price. A market-making
    /// market's is the scenario's own oracle level there, which quoting
    /// can't push the way it can push Phoenix's mark; where the book trades
    /// at a premium, the scenario's book level: what closing the position
    /// would fetch, which Phoenix's mark, a blend of the book and the index,
    /// is not. Otherwise the mark, as Phoenix reports it.
    async fn value(&self, value_usd: f64, units: &[f64], slot: u64) -> Result<f64> {
        let mut value = value_usd;
        for (market, &units) in self.markets.iter().zip(units) {
            if market.valuation == "mark" {
                continue;
            }
            let price = self.price_usd(market, slot).await?;
            let mark = self.mark_ticks(market).await?;
            value += units * (price - self.phoenix.ticks_to_usd(&market.market, mark));
        }
        Ok(value)
    }

    /// The price `market`'s position is valued at, at `slot`, in USD, by
    /// its own rule: the scenario's level (`oracle`), its book level
    /// (`book`), or Phoenix's mark. Every market of the session has one: it
    /// is what a rule that prices positions across markets reads.
    async fn price_usd(&self, market: &MarketState, slot: u64) -> Result<f64> {
        let at = slot.saturating_sub(self.base_slot);
        let level = match market.valuation {
            "oracle" => level_at(&market.scenario.path, at),
            "book" => book_at(&market.scenario.path, at),
            _ => {
                let mark = self.mark_ticks(market).await?;
                return Ok(self.phoenix.ticks_to_usd(&market.market, mark));
            }
        };
        Ok(market.start_mark_usd * (1.0 + level / 100.0))
    }

    /// [`PerpAccount::value`], and the RFQ desk's wallet.
    async fn value_with_desk(&self, value_usd: f64, units: &[f64], slot: u64) -> Result<f64> {
        Ok(self.value(value_usd, units, slot).await? + self.desk_usd().await?)
    }

    /// Every market at a step, the primary included, in symbol order:
    /// nothing for a session on one market, whose step is its one market's.
    async fn market_steps(&self, at: u64, positions: &[f64]) -> Result<Vec<MarketStep>> {
        if self.markets.len() < 2 {
            return Ok(Vec::new());
        }
        let hawkeye = self.phoenix.hawkeye(self.bot);
        let mut steps = Vec::with_capacity(self.markets.len());
        for (market, &position) in self.markets.iter().zip(positions) {
            let symbol = &market.scenario.symbol;
            let bbo = patiently("the book", || hawkeye.view_bbo(symbol))
                .await?
                .value;
            let usd = |ticks: u64| self.phoenix.ticks_to_usd(&market.market, ticks);
            let moment = market.moment(at);
            steps.push(MarketStep {
                symbol: symbol.clone(),
                mark_usd: usd(bbo.mark_price_ticks),
                index_usd: usd(bbo.index_price_ticks),
                level_usd: moment.level_usd,
                book_usd: (market.valuation == "book").then_some(moment.book_usd),
                fair_usd: moment.fair_usd,
                best_bid_usd: bbo.best_bid_ticks().map(usd),
                best_ask_usd: bbo.best_ask_ticks().map(usd),
                position,
            });
        }
        Ok(steps)
    }
}

#[async_trait]
impl Account for PerpAccount {
    fn start_value_usd(&self) -> f64 {
        self.start_value_usd
    }

    fn reads_fills(&self) -> bool {
        self.fills.is_some()
    }

    fn moment(&self, at: u64) -> Moment {
        self.own().moment(at)
    }

    async fn step(&mut self, k: u64, slot: u64, at: u64) -> Result<StepRecord> {
        self.scan(slot).await?;
        let now = self.measure().await?;
        record(&mut self.observed, &now)?;
        let positions = self.positions(&now);
        let position = positions[self.primary];
        self.positions_outside_universe = self.positions_outside_universe.max(now.outside_universe);
        let value_usd = self
            .value_with_desk(grader_f64(now.value_usd), &positions, slot)
            .await?;
        self.peak_value_usd = self.peak_value_usd.max(value_usd);
        self.max_drawdown_usd = self.max_drawdown_usd.max(self.peak_value_usd - value_usd);
        let own = &self.markets[self.primary];
        let hawkeye = self.phoenix.hawkeye(self.bot);
        let bbo = patiently("the book", || hawkeye.view_bbo(&own.scenario.symbol))
            .await?
            .value;
        let usd = |ticks: u64| self.phoenix.ticks_to_usd(&own.market, ticks);
        let start_mark_usd = own.start_mark_usd;
        let path = &own.scenario.path;
        Ok(StepRecord {
            k,
            at,
            mark_usd: usd(bbo.mark_price_ticks),
            index_usd: usd(bbo.index_price_ticks),
            level_usd: start_mark_usd * (1.0 + level_at(path, at) / 100.0),
            book_usd: (own.valuation == "book")
                .then(|| start_mark_usd * (1.0 + book_at(path, at) / 100.0)),
            fair_usd: own.scenario.fair_value_usd(start_mark_usd, at),
            best_bid_usd: bbo.best_bid_ticks().map(usd),
            best_ask_usd: bbo.best_ask_ticks().map(usd),
            position,
            value_usd,
            pnl_usd: value_usd - self.start_value_usd,
            turn_ms: None,
            answered: None,
            markets: self.market_steps(at, &positions).await?,
        })
    }

    async fn turn_data(&mut self, _step: &mut Step) -> Result<()> {
        Ok(())
    }

    async fn sample(&mut self, slot: u64) -> Result<()> {
        let now = self.measure().await?;
        record(&mut self.observed, &now)?;
        let positions = self.positions(&now);
        self.positions_outside_universe = self.positions_outside_universe.max(now.outside_universe);
        let value_usd = self
            .value_with_desk(grader_f64(now.value_usd), &positions, slot)
            .await?;
        self.peak_value_usd = self.peak_value_usd.max(value_usd);
        self.max_drawdown_usd = self.max_drawdown_usd.max(self.peak_value_usd - value_usd);
        Ok(())
    }

    fn wallet(&self) -> Option<&crate::spot::Wallet> {
        None
    }

    async fn desk(
        &self,
        host_url: &str,
        bot: &Keypair,
        env: &BotEnv,
        mandate: &Mandate,
        tx: Option<Arc<crate::txlayer::TxEngine>>,
    ) -> Result<Desk> {
        // The bot's own trader on each market, through the guard's host
        // side: its sends are the bot's, its reads aren't. Under the
        // transaction layer they go to its pool, with each line's options.
        // The primary market's seat is first: a line that names no market
        // goes there.
        let pool = tx.is_some().then(|| {
            Arc::new(crate::txlayer::client::PoolClient::new(
                host_url,
                bot.insecure_clone(),
            ))
        });
        let pooled = |trader: mtx_kit::Trader| match &pool {
            Some(pool) => mtx_kit::Trader {
                inner: trader.inner.with_chain(pool.clone()),
                ..trader
            },
            None => trader,
        };
        let own = self.own();
        let trader = Arc::new(pooled(own.perp.trader(bot.insecure_clone(), host_url)?));
        let venue = std::fs::read_to_string(&env.exchange_path)
            .map_err(crate::error::io_error(&env.exchange_path))?;
        let mut seats: Vec<Box<dyn Seat>> = vec![Box::new(
            crate::venue::perps::PerpSeat::new(own.perp.instrument().to_string(), trader.clone())
                .pooled(pool.is_some()),
        )];
        for (i, market) in self.markets.iter().enumerate() {
            if i != self.primary {
                let trader = pooled(market.perp.trader(bot.insecure_clone(), host_url)?);
                seats.push(Box::new(
                    crate::venue::perps::PerpSeat::new(
                        market.perp.instrument().to_string(),
                        Arc::new(trader),
                    )
                    .pooled(pool.is_some()),
                ));
            }
        }
        if let Some(board) = &self.rfqs {
            seats.push(Box::new(RfqSeat::new(board.clone())));
        }
        let markets = if self.markets.len() < 2 {
            mtx_kit::VenueInfo::of_perp_session(
                mandate,
                &env.symbol,
                mtx_kit::phoenix_market_address(&venue, &env.symbol),
            )
        } else {
            let addresses = self
                .markets
                .iter()
                .map(|m| (m.scenario.symbol.clone(), m.market.market_pubkey.clone()))
                .collect();
            mtx_kit::VenueInfo::of_perp_markets(mandate, &env.symbol, &addresses)
        };
        Ok(Desk {
            rfqs: self.rfqs.clone(),
            markets,
            seats,
            symbol: env.symbol.clone(),
            venue,
            authority: bot.pubkey().to_string(),
            reader: Reader::Perp(trader),
            pool,
            tx,
        })
    }

    fn decode(&self, ix: &serde_json::Value, keys: &[serde_json::Value]) -> IxRecord {
        // The market the instruction names, if it names one of the
        // session's: an order's price and size are in its own units.
        let named: Vec<&str> = ix["accounts"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|i| keys.get(i.as_u64()? as usize)?.as_str())
            .collect();
        let market = self
            .markets
            .iter()
            .find(|m| named.contains(&m.market.market_pubkey.as_str()))
            .unwrap_or_else(|| self.own());
        decode_instruction(ix, keys, &market.market)
    }

    /// Cancel the bot's orders on every market, then, market by market in
    /// symbol order, sell a long (buy back a short) with market orders
    /// through the book, each at most `slice`, limited
    /// [`STOP_OUT_LIMIT_BPS`] through the best price, reading the position
    /// back from Phoenix after each: at most
    /// [`MAX_CLOSE_ORDERS_PER_STEP`] orders in all. The position it reports
    /// is the largest one left; on several markets, each order names its
    /// market.
    async fn force_close(
        &mut self,
        bot: &Keypair,
        rpc_url: &str,
        slice: Option<f64>,
        at: u64,
    ) -> Result<Closing> {
        let mut seats = Vec::with_capacity(self.markets.len());
        for market in &self.markets {
            let seat = market.perp.seat(bot.insecure_clone(), rpc_url).await?;
            if let Err(error) = seat.cancel_all().await {
                tracing::warn!(%error, market = market.scenario.symbol, "stop-out: cancelling the bot's orders failed");
            }
            seats.push(seat);
        }
        let several = self.markets.len() > 1;
        let mut orders = Vec::new();
        let mut left = 0.0_f64;
        for (market, seat) in self.markets.iter().zip(&seats) {
            let mut position = position_in(&self.measure().await?, &market.id);
            while !is_flat(position) && orders.len() < MAX_CLOSE_ORDERS_PER_STEP {
                let (side, name) = if position > 0.0 {
                    (Side::Ask, "sell")
                } else {
                    (Side::Bid, "buy")
                };
                let size = slice.map_or(position.abs(), |s| s.min(position.abs()));
                let (bid, ask) = seat.top().await.map_err(Error::StopOut)?;
                let through = STOP_OUT_LIMIT_BPS / 10_000.0;
                let limit_usd = match side {
                    Side::Ask => bid.unwrap_or(market.start_mark_usd) * (1.0 - through),
                    Side::Bid => ask.unwrap_or(market.start_mark_usd) * (1.0 + through),
                };
                let sent = seat.take(side, size, limit_usd).await;
                let signature = sent.as_ref().ok().and_then(|()| seat.last_signature());
                let after = position_in(&self.measure().await?, &market.id);
                let moved = (after - position).abs() > 0.0;
                orders.push(CloseOrder {
                    at,
                    market: several.then(|| market.instrument.clone()),
                    side: name.into(),
                    size,
                    limit_usd,
                    position_after: after,
                    ok: sent.is_ok(),
                    error: sent.err(),
                    signature,
                });
                position = after;
                // A book that took nothing won't take more this step.
                if !moved {
                    break;
                }
            }
            if position.abs() > left.abs() {
                left = position;
            }
        }
        Ok(Closing {
            orders,
            position: left,
        })
    }

    async fn finish(mut self: Box<Self>, done: Done) -> Result<SessionReport> {
        self.scan(done.end_slot).await?;
        self.mark_forced(&done.forced_closes).await?;
        let end = self.measure().await?;
        record(&mut self.observed, &end)?;
        let end_positions = self.positions(&end);
        let end_desk = self.desk_holdings().await?;
        let end_value_usd = self
            .value(grader_f64(end.value_usd), &end_positions, done.end_slot)
            .await?
            + end_desk.map_or(0.0, |held| held.base_usd());
        let positions_outside_universe = self.positions_outside_universe.max(end.outside_universe);
        let liquidatable = self.meter.liquidatable().await?;
        let mut end_marks = Vec::with_capacity(self.markets.len());
        for market in &self.markets {
            end_marks.push(
                self.phoenix
                    .ticks_to_usd(&market.market, self.mark_ticks(market).await?),
            );
        }
        let observed = std::mem::take(&mut self.observed);
        let fills = self.fills.take().unwrap_or_default();
        let at = done.end_slot.saturating_sub(self.base_slot);
        let legs: std::collections::BTreeMap<String, Leg> = self
            .markets
            .iter()
            .zip(&end_positions)
            .map(|(market, &end)| {
                let leg = Leg {
                    start: market.start_position,
                    end,
                    start_usd: market.start_mark_usd,
                    close_usd: market.moment(at).level_usd,
                };
                (market.scenario.symbol.clone(), leg)
            })
            .collect();
        let scenario = &self.scenario;
        let settled = settle(
            scenario,
            &legs,
            end_value_usd - self.start_value_usd,
            &fills.fills,
        );
        let oracle_starts = std::mem::take(&mut self.oracle_starts);
        let own = self.own();
        let instrument = own.instrument.clone();
        let mut actors = done.actors;
        let markets = if self.markets.len() < 2 {
            Vec::new()
        } else {
            self.markets
                .iter()
                .enumerate()
                .map(|(i, market)| {
                    let (flow, competitor) = if i == self.primary {
                        (actors.flow.clone(), actors.competitor.clone())
                    } else {
                        let theirs = actors
                            .markets
                            .get(&market.scenario.symbol)
                            .cloned()
                            .unwrap_or_default();
                        (theirs.flow, theirs.competitor)
                    };
                    MarketReport {
                        symbol: market.scenario.symbol.clone(),
                        start_mark_usd: market.start_mark_usd,
                        end_mark_usd: end_marks[i],
                        start_position: market.start_position,
                        end_position: end_positions[i],
                        valuation: market.valuation,
                        flow,
                        competitor,
                    }
                })
                .collect()
        };
        let market_taker_orders = std::mem::take(&mut actors.markets)
            .into_iter()
            .filter(|(_, report)| !report.taker_orders.is_empty())
            .map(|(symbol, report)| (symbol, report.taker_orders))
            .collect();
        Ok(SessionReport {
            scenario: scenario.name.clone(),
            start_mark_usd: own.start_mark_usd,
            end_mark_usd: end_marks[self.primary],
            start_value_usd: self.start_value_usd,
            end_value_usd,
            pnl_usd: settled.pnl_usd,
            end_position: end_positions[self.primary],
            liquidatable,
            refused_calls: done.refused_calls,
            bot_exit: done.bot_exit,
            late_slots: done.late_slots,
            mandate: done.mandate,
            peak_position: observed.range(&instrument).map_or(0.0, PositionRange::peak),
            instrument,
            positions: observed.positions,
            positions_outside_universe,
            max_drawdown_usd: self
                .max_drawdown_usd
                .max(self.peak_value_usd.max(end_value_usd) - end_value_usd),
            valuation: own.valuation,
            flow: actors.flow,
            competitor: actors.competitor,
            actors: actors.library.map(|mut library| {
                library.desk = self
                    .desk_start
                    .zip(end_desk)
                    .map(|(start, end)| DeskReport {
                        start_base: start.base,
                        start_usdc: start.usdc,
                        start_index_usd: start.index_usd,
                        end_base: end.base,
                        end_usdc: end.usdc,
                        end_index_usd: end.index_usd,
                    });
                library
            }),
            spot: None,
            faults: done.faults,
            order: settled.order,
            participation: None,
            net: None,
            start_position: own.start_position,
            close_outs: settled.close_outs,
            perp_fills: fills.fills,
            perp_open_orders: fills.open_orders,
            tx_layer: None,
            lending: None,
            crank_refusals: done.crank_refusals,
            forced_closes: done.forced_closes,
            oracle_starts,
            markets,
            turns: done.turns,
            wall_ms: done.began.elapsed().as_millis() as u64,
            timeline: Timeline {
                start_slot: done.start_slot,
                end_slot: done.end_slot,
                slots_per_step: crate::session::SLOTS_PER_STEP,
                start_mark_usd: own.start_mark_usd,
                steps: done.steps,
                transactions: done.transactions,
                calls: done.calls,
                taker_orders: actors.taker_orders,
                market_taker_orders,
            },
        })
    }
}

/// One market's part of a session's settlement: the position at the start
/// and the end, in base units, and the start mark and the scenario's level
/// at the end, in USD.
#[derive(Debug, Clone, Copy)]
struct Leg {
    start: f64,
    end: f64,
    start_usd: f64,
    close_usd: f64,
}

/// What a perp session's PnL comes to, with its order and close-outs.
struct Settled {
    pnl_usd: f64,
    order: Option<OrderReport>,
    close_outs: Vec<CloseOutReport>,
}

/// Settle a session from each market's [`Leg`] (`legs`, by symbol), with
/// the account's value changing by `value_change_usd`. An order is scored
/// against its broker on its market's leg, as on spot (`fills`, the bot's
/// on that market, say whether it traded the other way); a close-out
/// charges the haircut on what is left in its market. Without either, the
/// PnL is the value change.
fn settle(
    scenario: &Scenario,
    legs: &std::collections::BTreeMap<String, Leg>,
    value_change_usd: f64,
    fills: &[PerpFillRecord],
) -> Settled {
    let flat = Leg {
        start: 0.0,
        end: 0.0,
        start_usd: 0.0,
        close_usd: 0.0,
    };
    let leg = |symbol: &str| legs.get(symbol).copied().unwrap_or(flat);
    let order = scenario.order.as_ref().map(|order| {
        let mut order = order.clone();
        let instrument = order
            .instrument
            .get_or_insert_with(|| scenario.instrument())
            .clone();
        let own: Vec<(CoreSide, f64)> = fills
            .iter()
            .filter(|f| f.instrument == instrument && !f.forced)
            .map(|f| (f.side, f.base))
            .collect();
        let at = leg(&instrument.symbol);
        order.report(
            at.start,
            at.end,
            at.start_usd,
            at.close_usd,
            value_change_usd,
            &own,
        )
    });
    let close_outs: Vec<CloseOutReport> = scenario
        .close_out
        .iter()
        .map(|(symbol, close_out)| {
            let at = leg(symbol);
            close_out.report(symbol, at.end, at.close_usd)
        })
        .collect();
    let pnl_usd = order
        .as_ref()
        .map_or(value_change_usd, OrderReport::pnl_usd)
        - close_outs.iter().map(|c| c.charge_usd).sum::<f64>();
    Settled {
        pnl_usd,
        order,
        close_outs,
    }
}

/// Counts every holding in `measurement` toward the session's position
/// ranges, under the mandate's names for them.
fn record(observed: &mut Observed, measurement: &Measurement) -> Result<()> {
    for (id, holding) in &measurement.holdings {
        let instrument = InstrumentRef::of_holding(id, holding).map_err(MeterError::Unsupported)?;
        observed.record(&instrument, grader_f64(holding.position()));
    }
    Ok(())
}

/// The account's position in `instrument`, in base units; flat if the meter
/// doesn't report it.
fn position_in(measurement: &Measurement, instrument: &InstrumentId) -> f64 {
    measurement
        .holding(instrument)
        .map_or(0.0, |holding| grader_f64(holding.position()))
}

const COMPUTE_BUDGET: &str = "ComputeBudget111111111111111111111111111111";

/// Decode one instruction of a `getTransaction` (`json` encoding).
fn decode_instruction(
    ix: &serde_json::Value,
    keys: &[serde_json::Value],
    market: &ExchangeMarketConfig,
) -> IxRecord {
    let program = ix["programIdIndex"]
        .as_u64()
        .and_then(|i| keys.get(i as usize))
        .and_then(|k| k.as_str())
        .unwrap_or_default()
        .to_string();
    let data = ix["data"]
        .as_str()
        .and_then(|d| bs58::decode(d).into_vec().ok())
        .unwrap_or_default();
    if program == COMPUTE_BUDGET {
        return IxRecord {
            program: "compute_budget".into(),
            name: None,
            order: None,
        };
    }
    let Some(instruction) = phoenix_instruction(&data) else {
        return IxRecord {
            program,
            name: None,
            order: None,
        };
    };
    IxRecord {
        program: "phoenix".into(),
        name: Some(format!("{instruction:?}")),
        order: placed_order(instruction, &data, market),
    }
}

fn phoenix_instruction(data: &[u8]) -> Option<PhoenixInstruction> {
    PhoenixInstruction::from_discriminant(data.get(..8)?.try_into().ok()?)
}

/// The order a Phoenix instruction places, if it places one.
fn placed_order(
    instruction: PhoenixInstruction,
    data: &[u8],
    market: &ExchangeMarketConfig,
) -> Option<OrderRecord> {
    use mtx_kit::phoenix_rise::ix::order_packet::OrderPacket;
    if !matches!(
        instruction,
        PhoenixInstruction::PlaceLimitOrder | PhoenixInstruction::PlaceMarketOrder
    ) {
        return None;
    }
    let packet = <OrderPacket as borsh::BorshDeserialize>::deserialize(&mut &data[8..]).ok()?;
    // The packet's kind is private to phoenix-rise; its serde form isn't.
    let value = serde_json::to_value(&packet).ok()?;
    let (kind, fields) = value["kind"].as_object()?.iter().next()?;
    let side = fields["side"].as_str().unwrap_or_default().to_lowercase();
    Some(OrderRecord {
        kind: kind.clone(),
        side,
        price_usd: fields["price_in_ticks"]
            .as_u64()
            .map(|ticks| mtx_kit::venue::ticks_to_usd(market, ticks)),
        size: mtx_kit::venue::lots_to_base_units(market, fields["num_base_lots"].as_i64()?),
    })
}

/// What the actors that act on the bot's orders as they arrive read of its
/// sends ([`SendReader`]): the orders each places on `market`, when a
/// simulation at the grader's endpoint `rpc_url` says the chain would run
/// it. A cancel, an order elsewhere, or a transaction that would fail shows
/// none.
fn send_reader(market: ExchangeMarketConfig, rpc_url: String) -> SendReader {
    let client = reqwest::Client::new();
    Arc::new(move |call: serde_json::Value| {
        let (market, rpc_url, client) = (market.clone(), rpc_url.clone(), client.clone());
        Box::pin(async move {
            let orders = orders_in_send(&call, &market);
            if orders.is_empty() || !would_run(&client, &rpc_url, &call).await {
                return Vec::new();
            }
            orders
        })
    })
}

/// The orders a `sendTransaction` call places on `market`.
fn orders_in_send(call: &serde_json::Value, market: &ExchangeMarketConfig) -> Vec<SentOrder> {
    let (Some(tx), Ok(address)) = (
        crate::faults::WireTransaction::parse(call),
        market.market_pubkey.parse::<Pubkey>(),
    ) else {
        return Vec::new();
    };
    tx.instructions
        .iter()
        .filter(|ix| ix.accounts.contains(&Some(address)))
        .filter_map(|ix| {
            let instruction = phoenix_instruction(&ix.data)?;
            let order = placed_order(instruction, &ix.data, market)?;
            let side = match order.side.as_str() {
                "bid" => mtx_kit::Side::Bid,
                "ask" => mtx_kit::Side::Ask,
                _ => return None,
            };
            Some(SentOrder {
                side,
                size: order.size,
                price: order.price_usd,
                immediate: matches!(instruction, PhoenixInstruction::PlaceMarketOrder)
                    || order.kind == "ImmediateOrCancel",
            })
        })
        .collect()
}

/// Whether a simulation of `call`'s transaction runs without error.
async fn would_run(client: &reqwest::Client, rpc_url: &str, call: &serde_json::Value) -> bool {
    let encoding = call["params"][1]["encoding"].as_str().unwrap_or("base58");
    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "simulateTransaction",
        "params": [
            call["params"][0],
            { "encoding": encoding, "sigVerify": false, "commitment": "processed" }
        ]
    });
    let reply = match client
        .post(rpc_url)
        .json(&request)
        .timeout(INTERNAL_RPC_TIMEOUT)
        .send()
        .await
    {
        Ok(response) => response.json::<serde_json::Value>().await.ok(),
        Err(_) => None,
    };
    reply.is_some_and(|reply| {
        reply["result"]["value"].is_object() && reply["result"]["value"]["err"].is_null()
    })
}

#[cfg(test)]
mod tests {
    use mtx_kit::chain::mock::MockChainClient;
    use solana_account::Account;

    use super::*;

    /// [`settle`] on the scenario's one market.
    fn settle_one(
        scenario: &Scenario,
        start: f64,
        end: f64,
        (start_usd, close_usd): (f64, f64),
        value_change_usd: f64,
        fills: &[PerpFillRecord],
    ) -> Settled {
        let leg = Leg {
            start,
            end,
            start_usd,
            close_usd,
        };
        settle(
            scenario,
            &[(scenario.symbol.clone(), leg)].into(),
            value_change_usd,
            fills,
        )
    }

    #[test]
    fn each_market_settles_on_its_own_leg() {
        let mut scenario = crate::scenario::ShockFamily::spcx().sample("s", 1);
        scenario.order = Some(crate::orders::ExecutionOrder {
            instrument: Some("phoenix:perp:NVDA".parse().unwrap()),
            side: CoreSide::Buy,
            quantity: 10.0,
            haircut_bps: 50.0,
            reduce_only: false,
        });
        scenario.close_out.insert(
            "SPCX".into(),
            crate::orders::CloseOut { haircut_bps: 100.0 },
        );
        let legs = [
            (
                "NVDA".to_string(),
                Leg {
                    start: 0.0,
                    end: 6.0,
                    start_usd: 180.0,
                    close_usd: 190.0,
                },
            ),
            (
                "SPCX".to_string(),
                Leg {
                    start: 0.0,
                    end: -4.0,
                    start_usd: 100.0,
                    close_usd: 200.0,
                },
            ),
        ]
        .into();
        // A sell on SPCX is not a sell against the NVDA order, nor is the
        // risk desk's forced sell on NVDA.
        let fills = [
            PerpFillRecord {
                instrument: "phoenix:perp:NVDA".parse().unwrap(),
                ..fill(CoreSide::Buy, 6.0)
            },
            fill(CoreSide::Sell, 4.0),
            PerpFillRecord {
                instrument: "phoenix:perp:NVDA".parse().unwrap(),
                forced: true,
                ..fill(CoreSide::Sell, 2.0)
            },
        ];
        let settled = settle(&scenario, &legs, 70.0, &fills);
        let order = settled.order.unwrap();
        assert_eq!(order.breach(), None);
        let alone = crate::orders::ExecutionOrder {
            instrument: Some("phoenix:perp:NVDA".parse().unwrap()),
            side: CoreSide::Buy,
            quantity: 10.0,
            haircut_bps: 50.0,
            reduce_only: false,
        }
        .report(0.0, 6.0, 180.0, 190.0, 70.0, &[(CoreSide::Buy, 6.0)]);
        // The SPCX residual short is charged at its own level.
        assert_eq!(settled.close_outs.len(), 1);
        assert!((settled.close_outs[0].broker_usd - 202.0).abs() < 1e-9);
        assert!((settled.pnl_usd - (alone.pnl_usd() - 8.0)).abs() < 1e-9);
    }

    fn fill(side: CoreSide, base: f64) -> PerpFillRecord {
        PerpFillRecord {
            at: 2,
            instrument: "phoenix:perp:SPCX".parse().unwrap(),
            side,
            price: 100.0,
            base,
            forced: false,
        }
    }

    #[test]
    fn a_perp_order_settles_as_the_spot_formula_does() {
        let mut scenario = crate::scenario::ShockFamily::spcx().sample("s", 1);
        let order = crate::orders::ExecutionOrder {
            instrument: None,
            side: CoreSide::Buy,
            quantity: 10.0,
            haircut_bps: 50.0,
            reduce_only: false,
        };
        scenario.order = Some(order.clone());
        // Doing nothing scores 0 whatever the close.
        let nop = settle_one(&scenario, 0.0, 0.0, (100.0, 110.0), 0.0, &[]);
        assert_eq!(nop.pnl_usd, 0.0);
        // Bought 6 at 101 against a close of 110, 1 of fees: the same PnL
        // as the spot formula, and the order names its market.
        let fills = [fill(CoreSide::Buy, 6.0)];
        let value_change = 6.0 * (110.0 - 101.0) - 1.0;
        let bought = settle_one(&scenario, 0.0, 6.0, (100.0, 110.0), value_change, &fills);
        let spot = order.report(
            0.0,
            6.0,
            100.0,
            110.0,
            value_change,
            &[(CoreSide::Buy, 6.0)],
        );
        assert_eq!(bought.pnl_usd, spot.pnl_usd());
        assert!((bought.pnl_usd - (value_change + 6.0 * 110.0 * 0.005)).abs() < 1e-9);
        let report = bought.order.unwrap();
        assert_eq!(report.instrument, Some(scenario.instrument()));
        assert_eq!(report.breach(), None);
        // A sell inside the session is caught from the fills, though the
        // position never went below where it started.
        let both = [fill(CoreSide::Buy, 6.0), fill(CoreSide::Sell, 1.0)];
        let sold = settle_one(&scenario, 0.0, 5.0, (100.0, 110.0), 0.0, &both);
        assert!(
            sold.order
                .unwrap()
                .breach()
                .unwrap()
                .contains("sold 1 SPCX")
        );
        // The risk desk's forced sell isn't the bot's.
        let desk = [
            fill(CoreSide::Buy, 6.0),
            PerpFillRecord {
                forced: true,
                ..fill(CoreSide::Sell, 1.0)
            },
        ];
        let closed = settle_one(&scenario, 0.0, 5.0, (100.0, 110.0), 0.0, &desk);
        assert_eq!(closed.order.unwrap().breach(), None);
        // A start long sold down: the drift on what it started with is
        // taken off.
        scenario.order = Some(crate::orders::ExecutionOrder {
            side: CoreSide::Sell,
            reduce_only: true,
            ..order
        });
        let held = settle_one(&scenario, 10.0, 10.0, (100.0, 90.0), -100.0, &[]);
        assert_eq!(held.pnl_usd, 0.0);
    }

    #[test]
    fn a_close_out_charges_the_residual_either_way() {
        let mut scenario = crate::scenario::ShockFamily::spcx().sample("s", 1);
        scenario.close_out.insert(
            "SPCX".into(),
            crate::orders::CloseOut { haircut_bps: 100.0 },
        );
        let long = settle_one(&scenario, 0.0, 4.0, (100.0, 200.0), 50.0, &[]);
        assert!((long.pnl_usd - (50.0 - 4.0 * 2.0)).abs() < 1e-9);
        assert!((long.close_outs[0].broker_usd - 198.0).abs() < 1e-9);
        let short = settle_one(&scenario, 0.0, -4.0, (100.0, 200.0), 50.0, &[]);
        assert!((short.close_outs[0].broker_usd - 202.0).abs() < 1e-9);
        assert!((short.pnl_usd - (50.0 - 8.0)).abs() < 1e-9);
        let flat = settle_one(&scenario, 3.0, 0.0, (100.0, 200.0), 50.0, &[]);
        assert_eq!(flat.pnl_usd, 50.0);
        // Without an order or a close-out, the PnL is the value change.
        let plain = crate::scenario::ShockFamily::spcx().sample("s", 1);
        let settled = settle_one(&plain, 0.0, 4.0, (100.0, 200.0), 50.0, &[]);
        assert_eq!(settled.pnl_usd, 50.0);
        assert!(settled.order.is_none() && settled.close_outs.is_empty());
    }

    #[test]
    fn the_bot_is_told_its_start_order_and_close_outs() {
        let mut scenario = crate::scenario::ShockFamily::spcx().sample("s", 1);
        assert!(session_vars(&scenario).unwrap().is_empty());
        scenario.start_positions.insert("SPCX".into(), -2.5);
        scenario.order = Some(crate::orders::ExecutionOrder {
            instrument: None,
            side: CoreSide::Buy,
            quantity: 2.5,
            haircut_bps: 30.0,
            reduce_only: true,
        });
        let vars: std::collections::BTreeMap<String, String> =
            session_vars(&scenario).unwrap().into_iter().collect();
        assert_eq!(vars["MTX_START_POSITIONS"], r#"{"SPCX":-2.5}"#);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&vars["MTX_ORDER"]).unwrap(),
            serde_json::json!({
                "instrument": "phoenix:perp:SPCX",
                "side": "buy",
                "quantity": 2.5,
                "reduce_only": true
            })
        );
        assert_eq!(vars["MTX_BROKER_HAIRCUT_BPS"], "30");
        scenario.order = None;
        scenario
            .close_out
            .insert("SPCX".into(), crate::orders::CloseOut { haircut_bps: 12.5 });
        let vars: std::collections::BTreeMap<String, String> =
            session_vars(&scenario).unwrap().into_iter().collect();
        assert_eq!(vars["MTX_CLOSE_OUT"], r#"{"SPCX":12.5}"#);
    }

    #[tokio::test]
    async fn a_desk_whose_account_the_bot_closed_holds_nothing() {
        let usdc = Token {
            mint: phoenix::USDC_MINT,
            program: mtx_kit::token::TOKEN_PROGRAM_ID,
            decimals: 6,
        };
        let bot = Pubkey::new_unique();
        let mut data = vec![0; 165];
        data[..32].copy_from_slice(&usdc.mint.to_bytes());
        data[32..64].copy_from_slice(&bot.to_bytes());
        data[64..72].copy_from_slice(&2_500_000u64.to_le_bytes());
        data[108] = 1;
        let chain = MockChainClient::new();
        chain.set_account(
            usdc.account(&bot),
            Account {
                lamports: 2_039_280,
                data,
                owner: usdc.program,
                executable: false,
                rent_epoch: 0,
            },
        );
        assert_eq!(wallet_holds(&chain, &bot, &usdc).await.unwrap(), 2.5);
        chain.remove_account(&usdc.account(&bot));
        assert_eq!(wallet_holds(&chain, &bot, &usdc).await.unwrap(), 0.0);
    }
}
