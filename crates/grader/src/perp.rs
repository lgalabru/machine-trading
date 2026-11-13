//! Perp sessions: a bot trading one of Phoenix Eternal's perpetuals, the
//! scenario's market, on a fresh offline world from the snapshot (see
//! [`crate::session`] for the session they share with spot sessions).
//!
//! Setup onboards a fresh trader with [`DEPOSIT_USDC`] of collateral, has
//! the scenario's actors ready (a market-making session's takers and the
//! grader's own maker, an off-hours session's or a tape's takers), and
//! registers the scenario's moves. Every step the session reads the account
//! from Phoenix, never from the bot: the position, and the value as the
//! scenario values it ([`Scenario::valuation`]).

use std::sync::Arc;

use async_trait::async_trait;
use mtx_core::{
    AccountMeter, InstrumentId, Measurement, MeterError,
    mandate::{InstrumentRef, Mandate, Observed, PositionRange, Term},
    turns::Step,
};
use mtx_kit::{
    BotEnv,
    chain::RpcChainClient,
    phoenix_rise::{ix::PhoenixInstruction, types::exchange::ExchangeMarketConfig},
    protocols::phoenix::eternal::{PhoenixEternalMeter, PhoenixExchange},
};
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use surfpool_sdk::{CheatcodeFilter, SimnetCommand};

use crate::{
    Error, Result,
    actors::{
        Cast, Competitor, FlowWatch, Moment, Pricing, SendReader, SentOrder, Takers,
        library::{
            DeskReport, RfqBoard, RfqClients, RfqSeat, Scale, Setting, Swaps, Token, WalletSeat,
        },
    },
    chain,
    guard::Guard,
    making::{TAKER_ACCOUNTS, book_at, level_at},
    phoenix::{self, Phoenix},
    scenario::Scenario,
    session::{
        Account, BOT_USERS, DEPOSIT_USDC, Done, INTERNAL_RPC_TIMEOUT, IxRecord, OrderRecord,
        Prepared, SessionDir, SessionOptions, SessionReport, StepRecord, Timeline, World,
        grader_f64, patiently, wait_for_slot,
    },
    snapshot,
    venue::{Market, Seat, perps::PerpMarket, phoenix::Phoenix as PhoenixVenue},
    wasm::{Desk, Reader},
};

/// Each taker's collateral, and the grader's maker's: far more than their
/// orders need, so none fails for margin.
const ACTOR_DEPOSIT_USDC: f64 = 100_000.0;

/// Set a perp session up, up to the bot's start.
pub(crate) async fn prepare(opts: &SessionOptions<'_>) -> Result<Prepared> {
    let scenario = opts.scenario;
    let world = snapshot::start_offline_stepped(opts.snapshot_dir).await?;
    if !world.manifest.holds(&scenario.symbol) {
        return Err(Error::NotWarmed {
            snapshot: opts.snapshot_dir.display().to_string(),
            symbol: scenario.symbol.clone(),
            held: world.manifest.symbols().join(", "),
        });
    }
    let surfnet = &world.surfnet;
    // The grader's own clients, served the way a stepped world needs: a
    // transaction is final once it executed (see [`Guard::internal`]).
    let internal = Guard::internal(surfnet.rpc_url()).await?;
    let rpc = mtx_kit::rpc_client_with_timeout(internal.url(), INTERNAL_RPC_TIMEOUT);
    let setup_from = rpc.get_slot()?;
    let phoenix = Phoenix::new(world.exchange.clone(), internal.url());
    let market = phoenix.market(&scenario.symbol)?.clone();

    let bot = opts.keys.keypair("bot", 0);
    let trader =
        phoenix::onboard_traders_stepped(surfnet, &phoenix, &rpc, &[&bot], DEPOSIT_USDC).await?[0];
    let hawkeye = phoenix.hawkeye(bot.pubkey());
    let start_bbo = patiently("the book", async || {
        hawkeye.view_bbo(&scenario.symbol).await
    })
    .await?
    .value;
    let start_mark_ticks = start_bbo.mark_price_ticks;
    let start_mark_usd = phoenix.ticks_to_usd(&market, start_mark_ticks);
    crate::oracles::check_index_gap(
        &scenario.symbol,
        start_mark_ticks as f64,
        start_bbo.index_price_ticks as f64,
    )?;
    let start_quote = crate::oracles::MarketQuote {
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
    };
    // The account, as an enforcer reads it: from Phoenix, never from the bot.
    // A scenario that sets funding values it with the funding it has accrued
    // and not settled yet; others as Phoenix's collateral and unrealized PnL
    // alone.
    let meter = PhoenixEternalMeter::new(
        Arc::new(PhoenixExchange::new(world.exchange.clone())),
        internal.url(),
        Arc::new(RpcChainClient::with_timeout(
            internal.url(),
            INTERNAL_RPC_TIMEOUT,
        )),
        bot.pubkey(),
        &scenario.symbol,
    )
    .map_err(mtx_kit::Error::from)?
    .with_unsettled_funding(scenario.funding.is_some());
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
    if let Some(mark) = scenario.funding.as_ref().and_then(|f| f.mark.as_ref()) {
        phoenix::set_mark(surfnet, &phoenix, &scenario.symbol, mark)?;
    }
    // The scenario's perp, as Phoenix offers it.
    let perp = PhoenixVenue::with_exchange(
        internal.url(),
        world.exchange.clone(),
        world.exchange_path.clone(),
        bot.pubkey(),
    )
    .perp(&scenario.symbol)?;
    // The actors, set up while cheatcodes still work: market making's
    // takers, with the makers widened, and a competing maker; off hours'
    // takers, with the makers widened and no competitor; a tape's takers,
    // with the makers as they are.
    let mut cast = Cast::default();
    let widen = |half_spread_bps: f64| -> Result<u64> {
        let half_spread_ticks =
            (start_mark_ticks as f64 * half_spread_bps / 10_000.0).round() as u64;
        phoenix::widen_makers(surfnet, &phoenix, &scenario.symbol, half_spread_ticks)
    };
    let takers = match (&scenario.making, &scenario.offhours) {
        (Some(making), _) => Some((
            widen(making.competitor_bps)?,
            &making.takers,
            scenario.takers_pricing.unwrap_or(Pricing::Level),
        )),
        (None, Some(off)) => Some((
            widen(off.half_spread_bps)?,
            &off.takers,
            scenario.takers_pricing.unwrap_or(Pricing::Level),
        )),
        (None, None) if !scenario.tape.is_empty() => Some((
            0,
            &scenario.tape,
            scenario.takers_pricing.unwrap_or(Pricing::Book),
        )),
        (None, None) => None,
    };
    if let Some((widened_ticks, orders, pricing)) = takers {
        let keypairs: Vec<Keypair> = (0..TAKER_ACCOUNTS)
            .map(|i| opts.keys.keypair("taker", i as u32))
            .collect();
        let seats = onboard(&world, &phoenix, &perp, &rpc, internal.url(), keypairs).await?;
        tracing::info!(takers = seats.len(), widened_ticks, "takers ready");
        cast.add(Box::new(Takers::new(
            seats,
            orders.clone(),
            pricing,
            widened_ticks,
        )));
    }
    if let Some(making) = &scenario.making {
        let keypair = opts.keys.keypair("competitor", 0);
        let seat = onboard(&world, &phoenix, &perp, &rpc, internal.url(), vec![keypair])
            .await?
            .remove(0);
        cast.add(Box::new(Competitor::new(seat, making)));
    }
    // The library's actors: a wallet each, onboarded like the takers. They
    // are built once the session's first slot is known.
    let library = match &scenario.actors {
        Some(library) if !library.wallets().is_empty() => {
            let keypairs: Vec<Keypair> = library
                .wallets()
                .into_iter()
                .map(|(role, n)| opts.keys.keypair(role, n))
                .collect();
            let pubkeys: Vec<Pubkey> = keypairs.iter().map(Signer::pubkey).collect();
            let seats =
                onboard_unique(&world, &phoenix, &perp, &rpc, internal.url(), keypairs).await?;
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
                price_usd: start_mark_usd,
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
    // The market's funding parameters land on the session's first slot, so
    // they're in force, and their intervals start at the same point, before
    // the bot starts.
    let perp_asset_map = phoenix.perp_asset_map()?;
    if let Some(funding) = &scenario.funding {
        chain::register_scenario(
            &rpc,
            "funding",
            vec![crate::scenario::funding_override(
                perp_asset_map,
                &scenario.symbol,
                funding,
                first_slot.saturating_sub(slot).max(1),
            )],
        )?;
    }
    // Surfpool restamps Phoenix's oracle readings, and the slot its mark's
    // EMA last stepped at, with the current slot whenever it plays a Phoenix
    // override (an onboarding's capabilities, say), and the EMA's next step
    // runs from there. Setup's slots no longer move, but one more override on
    // the session's first slot restamps them all to it, so the session's
    // mark doesn't depend on how many onboardings setup ran either. A
    // funding scenario's own override, played there, does the same.
    if scenario.funding.is_none() {
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
    if let Some(funding) = &scenario.funding {
        while slot < first_slot + 1 {
            surfnet.send_command(SimnetCommand::SlotForward(None))?;
            slot = wait_for_slot(&rpc, slot + 1).await?;
        }
        phoenix::check_funding(&rpc, &phoenix, &scenario.symbol, funding)?;
    }
    chain::register_scenario(
        &rpc,
        &scenario.name,
        scenario.overrides(perp_asset_map, start_mark_ticks),
    )?;
    let oracle_starts = match &scenario.oracles {
        Some(oracles) => crate::oracles::register(
            &rpc,
            oracles,
            scenario.duration_slots,
            &crate::oracles::World {
                phoenix: Some((perp_asset_map, start_mark_ticks)),
                prices: [(scenario.instrument(), start_quote)].into(),
            },
        )?,
        None => Default::default(),
    };
    let base_slot = rpc.get_slot()?;
    surfnet.cheatcodes().lock(CheatcodeFilter::all())?;
    let end_slot = base_slot + scenario.duration_slots;
    let mut rfqs = None;
    if let Some((library, seats)) = library {
        let (path, start) = (scenario.path.clone(), start_mark_usd);
        let fair_path = scenario.fair_path.clone();
        let path = Arc::new(move |at| Moment {
            at,
            level_usd: start * (1.0 + level_at(&path, at) / 100.0),
            book_usd: start * (1.0 + book_at(&path, at) / 100.0),
            fair_usd: fair_path
                .as_ref()
                .map(|fair| start * (1.0 + level_at(fair, at) / 100.0)),
        });
        // Front-runners read the bot's trading through a seat on its
        // account, and its sends as they arrive.
        let send_reader = (!library.front_runners.is_empty())
            .then(|| send_reader(market.clone(), internal.url().to_string()));
        if !library.front_runners.is_empty() {
            cast.watch(FlowWatch::new(vec![
                perp.seat(bot.insecure_clone(), internal.url()).await?,
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
    };
    let mut account = PerpAccount {
        scenario: scenario.clone(),
        phoenix,
        perp,
        market,
        bot: bot.pubkey(),
        meter,
        universe,
        market_id: scenario.instrument().id(),
        start_mark_usd,
        start_value_usd,
        base_slot,
        observed: Observed::default(),
        oracle_starts,
        peak_value_usd: start_value_usd,
        max_drawdown_usd: 0.0,
        positions_outside_universe: 0,
        rfqs,
        desk: swaps.map(|(_, desk)| desk),
        desk_start: None,
    };
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
        cast,
        cranks: Vec::new(),
        account: Box::new(account),
    })
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

/// A perp session's account: the bot's Phoenix trader account, metered.
struct PerpAccount {
    scenario: Scenario,
    phoenix: Phoenix,
    perp: PerpMarket,
    market: ExchangeMarketConfig,
    bot: Pubkey,
    meter: PhoenixEternalMeter,
    universe: Vec<InstrumentId>,
    market_id: InstrumentId,
    start_mark_usd: f64,
    oracle_starts: std::collections::BTreeMap<String, crate::oracles::OracleStart>,
    start_value_usd: f64,
    base_slot: u64,
    /// Sampled every step: a fill persists until the bot trades out of it,
    /// so a position over the cap is caught.
    observed: Observed,
    /// The account's value, sampled at the same cadence: its running peak,
    /// and the deepest it has fallen below it.
    peak_value_usd: f64,
    max_drawdown_usd: f64,
    /// Phoenix positions (or orders) in markets other than the scenario's.
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
    async fn measure(&self) -> Result<Measurement> {
        Ok(patiently("the meter", || self.meter.measure(&self.universe)).await?)
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
            index_usd: self.phoenix.ticks_to_usd(&self.market, index),
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

    async fn mark_ticks(&self) -> Result<u64> {
        let hawkeye = self.phoenix.hawkeye(self.bot);
        let symbol = &self.scenario.symbol;
        Ok(patiently("the book", || hawkeye.view_bbo(symbol))
            .await?
            .value
            .mark_price_ticks)
    }

    /// What a market-making account's position is worth: the scenario's
    /// own oracle level at `slot`, which quoting can't push the way it can
    /// push Phoenix's mark. Otherwise the mark, as Phoenix reports it.
    /// Where the book trades at a premium, the scenario's book level: what
    /// closing the position would fetch, which Phoenix's mark, a blend of
    /// the book and the index, is not.
    async fn value(&self, value_usd: f64, units: f64, slot: u64) -> Result<f64> {
        let scenario = &self.scenario;
        let at = slot.saturating_sub(self.base_slot);
        let level = match scenario.valuation() {
            "oracle" => level_at(&scenario.path, at),
            "book" => book_at(&scenario.path, at),
            _ => return Ok(value_usd),
        };
        let mark = self.mark_ticks().await?;
        let price = self.start_mark_usd * (1.0 + level / 100.0);
        Ok(value_usd + units * (price - self.phoenix.ticks_to_usd(&self.market, mark)))
    }

    /// [`PerpAccount::value`], and the RFQ desk's wallet.
    async fn value_with_desk(&self, value_usd: f64, units: f64, slot: u64) -> Result<f64> {
        Ok(self.value(value_usd, units, slot).await? + self.desk_usd().await?)
    }
}

#[async_trait]
impl Account for PerpAccount {
    fn start_value_usd(&self) -> f64 {
        self.start_value_usd
    }

    fn moment(&self, at: u64) -> Moment {
        let path = &self.scenario.path;
        Moment {
            at,
            level_usd: self.start_mark_usd * (1.0 + level_at(path, at) / 100.0),
            book_usd: self.start_mark_usd * (1.0 + book_at(path, at) / 100.0),
            fair_usd: self.scenario.fair_value_usd(self.start_mark_usd, at),
        }
    }

    async fn step(&mut self, k: u64, slot: u64, at: u64) -> Result<StepRecord> {
        let scenario = &self.scenario;
        let now = self.measure().await?;
        record(&mut self.observed, &now)?;
        let position = position_in(&now, &self.market_id);
        self.positions_outside_universe = self.positions_outside_universe.max(now.outside_universe);
        let value_usd = self
            .value_with_desk(grader_f64(now.value_usd), position, slot)
            .await?;
        self.peak_value_usd = self.peak_value_usd.max(value_usd);
        self.max_drawdown_usd = self.max_drawdown_usd.max(self.peak_value_usd - value_usd);
        let hawkeye = self.phoenix.hawkeye(self.bot);
        let bbo = patiently("the book", || hawkeye.view_bbo(&scenario.symbol))
            .await?
            .value;
        let usd = |ticks: u64| self.phoenix.ticks_to_usd(&self.market, ticks);
        let start_mark_usd = self.start_mark_usd;
        Ok(StepRecord {
            k,
            at,
            mark_usd: usd(bbo.mark_price_ticks),
            index_usd: usd(bbo.index_price_ticks),
            level_usd: start_mark_usd * (1.0 + level_at(&scenario.path, at) / 100.0),
            book_usd: (scenario.valuation() == "book")
                .then(|| start_mark_usd * (1.0 + book_at(&scenario.path, at) / 100.0)),
            fair_usd: scenario.fair_value_usd(start_mark_usd, at),
            best_bid_usd: bbo.best_bid_ticks().map(usd),
            best_ask_usd: bbo.best_ask_ticks().map(usd),
            position,
            value_usd,
            pnl_usd: value_usd - self.start_value_usd,
            turn_ms: None,
            answered: None,
        })
    }

    async fn turn_data(&mut self, _step: &mut Step) -> Result<()> {
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
    ) -> Result<Desk> {
        // The bot's own trader, through the guard's host side: its sends
        // are the bot's, its reads aren't.
        let trader = Arc::new(self.perp.trader(bot.insecure_clone(), host_url)?);
        let venue = std::fs::read_to_string(&env.exchange_path)
            .map_err(crate::error::io_error(&env.exchange_path))?;
        let mut seats: Vec<Box<dyn Seat>> = vec![Box::new(crate::venue::perps::PerpSeat::new(
            self.perp.instrument().to_string(),
            trader.clone(),
        ))];
        if let Some(board) = &self.rfqs {
            seats.push(Box::new(RfqSeat::new(board.clone())));
        }
        Ok(Desk {
            rfqs: self.rfqs.clone(),
            markets: mtx_kit::VenueInfo::of_perp_session(
                mandate,
                &env.symbol,
                mtx_kit::phoenix_market_address(&venue, &env.symbol),
            ),
            seats,
            symbol: env.symbol.clone(),
            venue,
            authority: bot.pubkey().to_string(),
            reader: Reader::Perp(trader),
        })
    }

    fn decode(&self, ix: &serde_json::Value, keys: &[serde_json::Value]) -> IxRecord {
        decode_instruction(ix, keys, &self.market)
    }

    async fn finish(mut self: Box<Self>, done: Done) -> Result<SessionReport> {
        let scenario = &self.scenario;
        let end = self.measure().await?;
        record(&mut self.observed, &end)?;
        let end_position = position_in(&end, &self.market_id);
        let end_desk = self.desk_holdings().await?;
        let end_value_usd = self
            .value(grader_f64(end.value_usd), end_position, done.end_slot)
            .await?
            + end_desk.map_or(0.0, |held| held.base_usd());
        let positions_outside_universe = self.positions_outside_universe.max(end.outside_universe);
        let liquidatable = self.meter.liquidatable().await?;
        let instrument = scenario.instrument();
        let end_mark_usd = self
            .phoenix
            .ticks_to_usd(&self.market, self.mark_ticks().await?);
        let observed = std::mem::take(&mut self.observed);
        Ok(SessionReport {
            scenario: scenario.name.clone(),
            start_mark_usd: self.start_mark_usd,
            end_mark_usd,
            start_value_usd: self.start_value_usd,
            end_value_usd,
            pnl_usd: end_value_usd - self.start_value_usd,
            end_position,
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
            valuation: scenario.valuation(),
            flow: done.actors.flow,
            competitor: done.actors.competitor,
            actors: done.actors.library.map(|mut library| {
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
            order: None,
            participation: None,
            net: None,
            lending: None,
            crank_refusals: done.crank_refusals,
            oracle_starts: std::mem::take(&mut self.oracle_starts),
            turns: done.turns,
            wall_ms: done.began.elapsed().as_millis() as u64,
            timeline: Timeline {
                start_slot: done.start_slot,
                end_slot: done.end_slot,
                slots_per_step: crate::session::SLOTS_PER_STEP,
                start_mark_usd: self.start_mark_usd,
                steps: done.steps,
                transactions: done.transactions,
                calls: done.calls,
                taker_orders: done.actors.taker_orders,
            },
        })
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
