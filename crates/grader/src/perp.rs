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
    phoenix_rise::types::exchange::ExchangeMarketConfig,
    protocols::phoenix::eternal::{PhoenixEternalMeter, PhoenixExchange},
};
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use surfpool_sdk::{CheatcodeFilter, SimnetCommand};

use crate::{
    Error, Result,
    actors::{Cast, Competitor, Moment, Pricing, Takers},
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
    venue::{Seat, Venue, phoenix_perp::PhoenixPerp},
    wasm::{Desk, Reader},
};

/// Each taker's collateral, and the grader's maker's: far more than their
/// orders need, so none fails for margin.
const ACTOR_DEPOSIT_USDC: f64 = 100_000.0;

/// Set a perp session up, up to the bot's start.
pub(crate) async fn prepare(opts: &SessionOptions<'_>) -> Result<Prepared> {
    let scenario = opts.scenario;
    let world = snapshot::start_offline_stepped(opts.snapshot_dir).await?;
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
    let start_mark_ticks = patiently("the book", async || {
        hawkeye.view_bbo(&scenario.symbol).await
    })
    .await?
    .value
    .mark_price_ticks;
    let start_mark_usd = phoenix.ticks_to_usd(&market, start_mark_ticks);
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
    let start_value_usd = grader_f64(
        patiently("the meter", async || meter.measure(&universe).await)
            .await?
            .value_usd,
    );
    if let Some(mark) = scenario.funding.as_ref().and_then(|f| f.mark.as_ref()) {
        phoenix::set_mark(surfnet, &phoenix, &scenario.symbol, mark)?;
    }
    let venue = PhoenixPerp::new(
        world.exchange.clone(),
        world.exchange_path.clone(),
        &scenario.symbol,
        internal.url(),
        bot.pubkey(),
    )?;
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
            Pricing::Level,
        )),
        (None, Some(off)) => Some((widen(off.half_spread_bps)?, &off.takers, Pricing::Level)),
        (None, None) if !scenario.tape.is_empty() => Some((0, &scenario.tape, Pricing::Book)),
        (None, None) => None,
    };
    if let Some((widened_ticks, orders, pricing)) = takers {
        let keypairs: Vec<Keypair> = (0..TAKER_ACCOUNTS)
            .map(|i| opts.keys.keypair("taker", i as u32))
            .collect();
        let seats = onboard(&world, &phoenix, &venue, &rpc, internal.url(), keypairs).await?;
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
        let seat = onboard(
            &world,
            &phoenix,
            &venue,
            &rpc,
            internal.url(),
            vec![keypair],
        )
        .await?
        .remove(0);
        cast.add(Box::new(Competitor::new(seat, making)));
    }

    let guard = Guard::start(surfnet.rpc_url()).await?;
    let workdir = SessionDir::create(&bot, BOT_USERS[opts.slot])?;
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
    let base_slot = rpc.get_slot()?;
    surfnet.cheatcodes().lock(CheatcodeFilter::all())?;
    let end_slot = base_slot + scenario.duration_slots;
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
    let account = PerpAccount {
        scenario: scenario.clone(),
        phoenix,
        venue,
        market,
        bot: bot.pubkey(),
        meter,
        universe,
        market_id: scenario.instrument().id(),
        start_mark_usd,
        start_value_usd,
        base_slot,
        observed: Observed::default(),
        peak_value_usd: start_value_usd,
        max_drawdown_usd: 0.0,
        positions_outside_universe: 0,
    };
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
    venue: &PhoenixPerp,
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
        seats.push(venue.seat(keypair, rpc_url).await?);
    }
    Ok(seats)
}

/// A perp session's account: the bot's Phoenix trader account, metered.
struct PerpAccount {
    scenario: Scenario,
    phoenix: Phoenix,
    venue: PhoenixPerp,
    market: ExchangeMarketConfig,
    bot: Pubkey,
    meter: PhoenixEternalMeter,
    universe: Vec<InstrumentId>,
    market_id: InstrumentId,
    start_mark_usd: f64,
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
}

impl PerpAccount {
    async fn measure(&self) -> Result<Measurement> {
        Ok(patiently("the meter", || self.meter.measure(&self.universe)).await?)
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
        }
    }

    async fn step(&mut self, k: u64, slot: u64, at: u64) -> Result<StepRecord> {
        let scenario = &self.scenario;
        let now = self.measure().await?;
        record(&mut self.observed, &now)?;
        let position = position_in(&now, &self.market_id);
        self.positions_outside_universe = self.positions_outside_universe.max(now.outside_universe);
        let value_usd = self
            .value(grader_f64(now.value_usd), position, slot)
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
        let trader = Arc::new(self.venue.trader(bot.insecure_clone(), host_url)?);
        let venue = std::fs::read_to_string(&env.exchange_path)
            .map_err(crate::error::io_error(&env.exchange_path))?;
        Ok(Desk {
            venues: mtx_kit::VenueInfo::of_perp_session(
                mandate,
                &env.symbol,
                mtx_kit::phoenix_market_address(&venue, &env.symbol),
            ),
            seats: vec![Box::new(crate::venue::phoenix_perp::PerpSeat::new(
                self.venue.instrument().to_string(),
                trader.clone(),
            ))],
            symbol: env.symbol.clone(),
            venue,
            authority: bot.pubkey().to_string(),
            reader: Reader::Perp(trader),
        })
    }

    fn decode(&self, ix: &serde_json::Value, keys: &[serde_json::Value]) -> IxRecord {
        decode_instruction(ix, keys, &self.phoenix, &self.market)
    }

    async fn finish(mut self: Box<Self>, done: Done) -> Result<SessionReport> {
        let scenario = &self.scenario;
        let end = self.measure().await?;
        record(&mut self.observed, &end)?;
        let end_position = position_in(&end, &self.market_id);
        let end_value_usd = self
            .value(grader_f64(end.value_usd), end_position, done.end_slot)
            .await?;
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
            spot: None,
            faults: done.faults,
            order: None,
            participation: None,
            net: None,
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
    phoenix: &Phoenix,
    market: &ExchangeMarketConfig,
) -> IxRecord {
    use mtx_kit::phoenix_rise::ix::{PhoenixInstruction, order_packet::OrderPacket};
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
    let found = data
        .get(..8)
        .and_then(|tag| PhoenixInstruction::from_discriminant(tag.try_into().ok()?));
    let Some(instruction) = found else {
        return IxRecord {
            program,
            name: None,
            order: None,
        };
    };
    let order = matches!(
        instruction,
        PhoenixInstruction::PlaceLimitOrder | PhoenixInstruction::PlaceMarketOrder
    )
    .then(|| {
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
                .map(|ticks| phoenix.ticks_to_usd(market, ticks)),
            size: phoenix.lots_to_base_units(market, fields["num_base_lots"].as_i64()?),
        })
    })
    .flatten();
    IxRecord {
        program: "phoenix".into(),
        name: Some(format!("{instruction:?}")),
        order,
    }
}
