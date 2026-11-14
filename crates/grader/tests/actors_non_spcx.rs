//! The grader's actors and family machinery on the other equity perps of
//! the 96-market snapshot (NVDA, TSLA, SPY, QQQ), with SPCX, the market they
//! were built on, as the control.
//!
//! Each test sets up one fresh offline world per market, as a session does,
//! and drives the real code: `widen_makers`, the takers, the competitor, the
//! scenario's path moves, funding, mark and oracle overrides, and Phoenix's
//! own meter. What a test measures is printed as a `TABLE|item|market|PASS or
//! FAIL|numbers` line (run with `--nocapture`), and asserted after printing.
//! They take about 8 minutes, so they're ignored by default: run them with
//! `just actors-markets`.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use mtx_core::AccountMeter;
use mtx_grader::{
    actors::{
        Actor, ActorsReport, Competitor, Library, Moment, Pricing, Takers,
        library::{Setting, WalletSeat},
    },
    chain,
    grade::MAX_FAILED_TAKER_SHARE,
    guard::Guard,
    making::{Competition, Making, Reactive, TAKER_ACCOUNTS, TakerOrder, TakerSide},
    oracles::{self, Failure, Feed, Mode, Oracles, PhoenixOracle, Source, Window},
    phoenix::{self, MarkConfig, Phoenix},
    scenario::{self, Funding, Scenario},
    scenario_v2,
    snapshot::{self, OfflineWorld},
    venue::{Market, Seat, perps::PerpMarket},
};
use mtx_kit::{
    Side, Trader,
    chain::RpcChainClient,
    phoenix_rise::accounts::spline_collection::SplineCollection,
    protocols::phoenix::eternal::{PhoenixEternalMeter, PhoenixExchange},
};
use serde_json::json;
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_rpc_client::rpc_client::RpcClient;
use solana_signer::Signer;
use tokio::sync::Semaphore;

const SNAPSHOT: &str = "snapshots/phoenix-perps-2026-11-13";
/// What every account here starts with: far more than any order needs.
const DEPOSIT_USDC: f64 = 100_000.0;
/// Worlds started at once in one test process: each is a surfnet with 900
/// accounts and Phoenix's programs.
static WORLDS: Semaphore = Semaphore::const_new(2);

/// A real market's own funding cap per interval, from the snapshot's
/// exchange metadata: the carry test runs at it.
fn real_max_rate(symbol: &str) -> i64 {
    match symbol {
        "SPCX" => 315,
        "NVDA" => 388,
        "TSLA" => 887,
        "SPY" => 1750,
        "QQQ" => 1491,
        other => panic!("no funding cap listed for {other}"),
    }
}

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// One line of the results table: `PASS`, `FAIL`, or `NOTE` for a result
/// that holds the contract but not the tighter bound the item was written
/// with.
fn table(item: &str, market: &str, status: &str, details: String) {
    println!("TABLE|{item}|{market}|{status}|{details}");
}

fn status(pass: bool) -> &'static str {
    if pass { "PASS" } else { "FAIL" }
}

/// A seeded generator (SplitMix64), so every run draws the same orders.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }

    fn normal(&mut self) -> f64 {
        let (a, b) = (self.unit().max(1e-12), self.unit());
        (-2.0 * a.ln()).sqrt() * (std::f64::consts::TAU * b).cos()
    }
}

/// The best quotes and Phoenix's two prices, in ticks.
#[derive(Debug, Clone, Copy)]
struct Book {
    bid: Option<u64>,
    ask: Option<u64>,
    mark: u64,
    index: u64,
}

/// One market's offline world, set up as a session sets one up.
struct Rig {
    world: OfflineWorld,
    _guard: Guard,
    rpc: RpcClient,
    url: String,
    phoenix: Phoenix,
    perp: PerpMarket,
    symbol: String,
    start_ticks: u64,
    keys: Vec<Keypair>,
    traders: Vec<Pubkey>,
    _permit: tokio::sync::SemaphorePermit<'static>,
}

fn key(i: usize) -> Keypair {
    let mut seed = [7u8; 32];
    seed[0] = i as u8;
    seed[1] = 0x30;
    Keypair::new_from_array(seed)
}

impl Rig {
    /// A world on `symbol` with `accounts` funded and enabled traders.
    async fn new(symbol: &str, accounts: usize) -> Rig {
        let permit = WORLDS.acquire().await.unwrap();
        // A run can point at another snapshot (the one the shipped tasks
        // use, say) to compare.
        let dir = std::env::var("MTX_TEST_SNAPSHOT")
            .ok()
            .filter(|dir| !dir.is_empty())
            .map_or_else(|| root().join(SNAPSHOT), PathBuf::from);
        let world = snapshot::start_offline_stepped(&dir).await.unwrap();
        assert!(world.manifest.holds(symbol), "the snapshot holds {symbol}");
        let guard = Guard::internal(world.surfnet.rpc_url()).await.unwrap();
        let url = guard.url().to_string();
        let rpc = mtx_kit::rpc_client_with_timeout(&url, Duration::from_secs(600));
        let phoenix = Phoenix::new(world.exchange.clone(), &url);
        let keys: Vec<Keypair> = (0..accounts).map(key).collect();
        let refs: Vec<&Keypair> = keys.iter().collect();
        let traders =
            phoenix::onboard_traders_stepped(&world.surfnet, &phoenix, &rpc, &refs, DEPOSIT_USDC)
                .await
                .unwrap();
        // One more Phoenix override restamps the oracle readings to now, as
        // a session's setup does before its first slot.
        chain::register_scenario(
            &rpc,
            "readings",
            vec![phoenix::enable_trader(traders[0], 1)],
        )
        .unwrap();
        chain::next_slot(&world.surfnet, &rpc).await.unwrap();
        chain::next_slot(&world.surfnet, &rpc).await.unwrap();
        let perp = PerpMarket::new(
            world.exchange.clone(),
            world.exchange_path.clone(),
            symbol,
            &url,
            keys[0].pubkey(),
        )
        .unwrap();
        let mut rig = Rig {
            world,
            _guard: guard,
            rpc,
            url,
            phoenix,
            perp,
            symbol: symbol.to_string(),
            start_ticks: 0,
            keys,
            traders,
            _permit: permit,
        };
        rig.start_ticks = rig.book().await.mark;
        rig
    }

    fn usd(&self, ticks: u64) -> f64 {
        let market = self.phoenix.market(&self.symbol).unwrap();
        self.phoenix.ticks_to_usd(market, ticks)
    }

    fn tick_usd(&self) -> f64 {
        self.usd(1)
    }

    fn start_usd(&self) -> f64 {
        self.usd(self.start_ticks)
    }

    async fn book(&self) -> Book {
        let hawkeye = self.phoenix.hawkeye(self.keys[0].pubkey());
        let mut last = None;
        for _ in 0..5 {
            match hawkeye.view_bbo(&self.symbol).await {
                Ok(view) => {
                    let bbo = view.value;
                    return Book {
                        bid: bbo.best_bid_ticks(),
                        ask: bbo.best_ask_ticks(),
                        mark: bbo.mark_price_ticks,
                        index: bbo.index_price_ticks,
                    };
                }
                Err(error) => last = Some(error.to_string()),
            }
        }
        panic!("no book: {}", last.unwrap());
    }

    async fn step(&self, slots: u64) {
        for _ in 0..slots {
            chain::next_slot(&self.world.surfnet, &self.rpc)
                .await
                .unwrap();
        }
    }

    async fn seat(&self, account: usize) -> Box<dyn Seat> {
        self.perp
            .seat(self.keys[account].insecure_clone(), &self.url)
            .await
            .unwrap()
    }

    fn trader(&self, account: usize) -> Trader {
        self.perp
            .trader(self.keys[account].insecure_clone(), &self.url)
            .unwrap()
    }

    fn meter(&self, account: usize, unsettled_funding: bool) -> PhoenixEternalMeter {
        PhoenixEternalMeter::new(
            Arc::new(PhoenixExchange::new(self.world.exchange.clone())),
            &self.url,
            Arc::new(RpcChainClient::with_timeout(
                &self.url,
                Duration::from_secs(600),
            )),
            self.keys[account].pubkey(),
            &self.symbol,
        )
        .unwrap()
        .with_unsettled_funding(unsettled_funding)
    }

    async fn value(&self, account: usize, unsettled_funding: bool) -> f64 {
        let id = self.perp.instrument().id();
        let measured = self
            .meter(account, unsettled_funding)
            .measure(std::slice::from_ref(&id))
            .await
            .unwrap();
        measured.value_usd.to_string().parse().unwrap()
    }

    /// Push the makers out to `bps` of the starting mark, as a session's
    /// setup does. Returns the ticks asked for.
    fn widen(&self, bps: f64) -> u64 {
        let ticks = (self.start_ticks as f64 * bps / 10_000.0).round() as u64;
        phoenix::widen_makers(&self.world.surfnet, &self.phoenix, &self.symbol, ticks).unwrap();
        ticks
    }

    /// Phoenix's view of an account's funding: (accumulated, unsettled)
    /// in quote lots, and the current rate in micro basis points.
    async fn funding_view(&self, account: usize) -> (i64, i64, i64) {
        let market = self.phoenix.market(&self.symbol).unwrap();
        let view = self
            .phoenix
            .hawkeye(self.keys[0].pubkey())
            .view_funding_for_trader(self.traders[account], market.asset_id)
            .await
            .unwrap()
            .value;
        (
            view.total_accumulated_funding_quote_lots().unwrap_or(0),
            view.total_unsettled_funding_quote_lots().unwrap_or(0),
            view.current_funding_rate_micro_bps,
        )
    }

    fn perp_asset_map(&self) -> Pubkey {
        self.phoenix.perp_asset_map().unwrap()
    }

    /// The scenario's market, from the spline account: each enabled
    /// spline's active ask regions as (start offset in ticks, total lots,
    /// unfilled lots, density).
    fn ask_regions(&self) -> Vec<(u64, u64, u64, u64)> {
        let market = self.phoenix.market(&self.symbol).unwrap();
        let address: Pubkey = market.spline_pubkey.parse().unwrap();
        let account = self.rpc.get_account(&address).unwrap();
        let collection = SplineCollection::try_from_account_bytes(&account.data).unwrap();
        let mut regions = Vec::new();
        for spline in collection.splines().filter_map(|s| s.ok()) {
            if !spline.is_enabled() {
                continue;
            }
            for region in spline.active_ask_regions() {
                regions.push((
                    region.start_offset().as_inner(),
                    region.total_size().as_inner(),
                    region.unfilled_size().as_inner(),
                    region.density().as_inner(),
                ));
            }
        }
        regions.sort();
        regions
    }
}

impl Rig {
    /// Each enabled spline as (mid in ticks, bid offset, ask offset, innermost
    /// bid region start, innermost ask region start).
    fn splines(&self) -> Vec<(u64, u64, u64, u64, u64)> {
        let market = self.phoenix.market(&self.symbol).unwrap();
        let address: Pubkey = market.spline_pubkey.parse().unwrap();
        let account = self.rpc.get_account(&address).unwrap();
        let collection = SplineCollection::try_from_account_bytes(&account.data).unwrap();
        collection
            .splines()
            .filter_map(|s| s.ok())
            .filter(|s| s.is_enabled())
            .map(|s| {
                let inner =
                    |regions: &[_]| {
                        regions
                        .iter()
                        .map(|r: &mtx_kit::phoenix_rise::accounts::spline_collection::TickRegion| {
                            r.start_offset().as_inner()
                        })
                        .min()
                        .unwrap_or(0)
                    };
                (
                    s.mid_price().as_inner(),
                    s.bid_offset(),
                    s.ask_offset(),
                    inner(s.active_bid_regions()),
                    inner(s.active_ask_regions()),
                )
            })
            .collect()
    }
}

fn scenario_of(value: serde_json::Value) -> Scenario {
    scenario_v2::parse(&value.to_string()).unwrap()
}

fn mandate(symbol: &str, max_position: f64) -> serde_json::Value {
    json!({
        "version": "v0",
        "universe": [{"instrument": format!("phoenix:perp:{symbol}"), "max_position": max_position}],
        "limits": {"max_drawdown_usd": 100000.0}
    })
}

macro_rules! per_market {
    ($name:ident, $run:ident) => {
        mod $name {
            use super::*;
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            #[ignore = "slow (about 8 minutes): run with `just actors-markets`"]
            async fn spcx() {
                $run("SPCX").await
            }
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            #[ignore = "slow (about 8 minutes): run with `just actors-markets`"]
            async fn nvda() {
                $run("NVDA").await
            }
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            #[ignore = "slow (about 8 minutes): run with `just actors-markets`"]
            async fn tsla() {
                $run("TSLA").await
            }
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            #[ignore = "slow (about 8 minutes): run with `just actors-markets`"]
            async fn spy() {
                $run("SPY").await
            }
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            #[ignore = "slow (about 8 minutes): run with `just actors-markets`"]
            async fn qqq() {
                $run("QQQ").await
            }
        }
    };
}

// 1. widen_makers ---------------------------------------------------------

/// After widening by h bp, the best bid and ask are h bp (within a tick) from
/// where the makers centre, at every price.
async fn widen_run(symbol: &str) {
    let rig = Rig::new(symbol, 1).await;
    let natural = rig.book().await;
    let natural_half = (natural.ask.unwrap() - natural.bid.unwrap()) as f64 / 2.0;
    let mut pass = true;
    let mut tight = true;
    let update_slots: Vec<u64> = {
        let market = rig.phoenix.market(symbol).unwrap();
        let address: Pubkey = market.spline_pubkey.parse().unwrap();
        let account = rig.rpc.get_account(&address).unwrap();
        let collection = SplineCollection::try_from_account_bytes(&account.data).unwrap();
        collection
            .splines()
            .filter_map(|s| s.ok())
            .filter(|s| s.is_enabled())
            .map(|s| s.user_update_slot())
            .collect()
    };
    let now = rig.rpc.get_slot().unwrap();
    let mut details = vec![format!(
        "spline update slots {update_slots:?} at slot {now}; tick {:.2} bp, natural half-spread {natural_half} ticks, mark {} index {}, splines (mid, bid offset, ask offset, inner bid, inner ask) {:?}",
        rig.tick_usd() / rig.start_usd() * 1e4,
        natural.mark,
        natural.index,
        rig.splines()
    )];
    for bps in [5.0, 15.0, 40.0] {
        let asked = rig.widen(bps);
        let book = rig.book().await;
        let (bid, ask) = (book.bid.unwrap(), book.ask.unwrap());
        let half = (ask - bid) as f64 / 2.0;
        let expected = (asked as f64).max(natural_half);
        let bid_off = book.mark as i64 - bid as i64;
        let ask_off = ask as i64 - book.mark as i64;
        // The contract: no closer than asked, and no further than what the
        // market quotes anyway. The item's tighter bound: within a tick.
        let ok = half >= asked as f64 - 1.0 && half <= expected + natural_half;
        pass &= ok;
        tight &= (half - expected).abs() <= 1.0;
        details.push(format!(
            "h={bps}bp: asked {asked}, half {half} ({:+} ticks), vs mark {bid_off}/{ask_off}{}",
            half - expected,
            if ok { "" } else { " WRONG" }
        ));
    }
    // Does the spread move as the splines age?
    let mut ageing = Vec::new();
    for slots in [100u64, 200] {
        rig.step(slots).await;
        let book = rig.book().await;
        ageing.push(format!(
            "+{} slots: half {}",
            if ageing.is_empty() { 100 } else { 300 },
            (book.ask.unwrap() - book.bid.unwrap()) as f64 / 2.0
        ));
    }
    details.push(format!("ageing: {}", ageing.join(", ")));
    let verdict = if !pass {
        "FAIL"
    } else if tight {
        "PASS"
    } else {
        "NOTE"
    };
    table("1 widen_makers", symbol, verdict, details.join("; "));
    assert!(pass, "{details:?}");
}

per_market!(widen_makers, widen_run);

// 2. takers ---------------------------------------------------------------

/// 200 drawn taker orders, sent by the real takers from eight accounts on a
/// book widened to 15 bp: every order that fails is counted, every fill is
/// inside its limit, and an order that doesn't fill is one the book couldn't
/// meet.
async fn takers_run(symbol: &str) {
    let rig = Rig::new(symbol, TAKER_ACCOUNTS as usize).await;
    let widened = rig.widen(15.0);
    let level = rig.start_usd();
    let tick = rig.tick_usd();
    let market = rig.phoenix.market(symbol).unwrap().clone();
    let lot_units = rig.phoenix.lots_to_base_units(&market, 1);

    let mut rng = Rng(0x51CA_7E12);
    let mut orders = Vec::new();
    for i in 0..200u64 {
        let buy = rng.unit() < 0.5;
        // Buys come from accounts 0-3 and sells from 4-7, so each account's
        // position only grows and its entry price reads off each fill.
        let account = (if buy { 0 } else { 4 } + (rng.next() % 4)) as u8;
        let size = ((0.5 * (0.7 * rng.normal()).exp()).clamp(0.05, 4.0) * 1e3).round() / 1e3;
        orders.push(TakerOrder {
            slot: i + 1,
            side: if buy { TakerSide::Buy } else { TakerSide::Sell },
            size,
            // Between 10 and 40 bp from the level.
            limit_bps: 10.0 + (rng.unit() * 3000.0).round() / 100.0,
            account,
        });
    }
    let mut seats = Vec::new();
    for account in 0..TAKER_ACCOUNTS as usize {
        seats.push(rig.seat(account).await);
    }
    let mut takers = Takers::new(seats, orders.clone(), Pricing::Level, widened);
    let readers: Vec<Trader> = (0..TAKER_ACCOUNTS as usize)
        .map(|a| rig.trader(a))
        .collect();

    let mut seen_accounts = [false; TAKER_ACCOUNTS as usize];
    let mut price_checks = 0;
    let mut filled = 0;
    let mut partial = 0;
    let mut none = 0;
    let mut spurious = 0;
    let mut outside = 0;
    let mut rounding_loss: f64 = 0.0;
    let mut volume = 0.0;
    let mut filled_volume = 0.0;
    let mut worst_fill_bps: f64 = 0.0;
    for order in &orders {
        let before_book = rig.book().await;
        let account = order.account as usize;
        let before = readers[account].position().await.unwrap();
        let moment = Moment {
            at: order.slot,
            level_usd: level,
            book_usd: level,
            fair_usd: None,
        };
        takers.before_blocks(&moment).await;
        let after = readers[account].position().await.unwrap();
        rig.step(1).await;

        let sign = if order.side == TakerSide::Buy {
            1.0
        } else {
            -1.0
        };
        let got = (after.size - before.size) * sign;
        let reach = order.limit_bps / 1e4;
        let limit = match order.side {
            TakerSide::Buy => level * (1.0 + reach),
            TakerSide::Sell => level * (1.0 - reach),
        };
        let limit = (limit * 100.0).round() / 100.0;
        // The size as Phoenix rounds it, to lots.
        let lots = rig.phoenix.base_units_to_lots(&market, order.size);
        rounding_loss = rounding_loss.max((order.size - lots as f64 * lot_units).abs() / lot_units);
        volume += order.size;
        filled_volume += got.max(0.0);
        let wanted = lots as f64 * lot_units;
        if got > wanted - lot_units / 2.0 {
            filled += 1;
        } else if got > lot_units / 2.0 {
            partial += 1;
        } else {
            none += 1;
            // An order that got nothing must have met no quote inside its
            // limit.
            let best = match order.side {
                TakerSide::Buy => before_book.ask.map(|t| rig.usd(t)),
                TakerSide::Sell => before_book.bid.map(|t| rig.usd(t)),
            };
            let reachable = match (order.side, best) {
                (TakerSide::Buy, Some(p)) => p <= limit + tick / 2.0,
                (TakerSide::Sell, Some(p)) => p >= limit - tick / 2.0,
                _ => false,
            };
            if reachable {
                spurious += 1;
            }
        }
        if got > lot_units / 2.0 {
            // What it filled must have been quoted inside its limit.
            let best = match order.side {
                TakerSide::Buy => before_book.ask.map(|t| rig.usd(t)),
                TakerSide::Sell => before_book.bid.map(|t| rig.usd(t)),
            };
            let reachable = match (order.side, best) {
                (TakerSide::Buy, Some(p)) => p <= limit + tick / 2.0,
                (TakerSide::Sell, Some(p)) => p >= limit - tick / 2.0,
                _ => false,
            };
            if !reachable {
                outside += 1;
            }
            // An account's first fill is its whole position: its entry price
            // is the fill's average price.
            if !seen_accounts[account] && before.size == 0.0 {
                let fill = after.entry_usd;
                let worse = match order.side {
                    TakerSide::Buy => fill - limit,
                    TakerSide::Sell => limit - fill,
                };
                price_checks += 1;
                if worse > tick * 1.5 {
                    outside += 1;
                }
                worst_fill_bps = worst_fill_bps.max((fill - level).abs() / level * 1e4);
            }
        }
        seen_accounts[account] = true;
    }
    let mut report = ActorsReport::default();
    takers.report(&mut report).await;
    // The limits as the takers worked them out, on the cent.
    let mut limit_error: f64 = 0.0;
    for (record, order) in report.taker_orders.iter().zip(&orders) {
        let reach = order.limit_bps / 1e4;
        let exact = match order.side {
            TakerSide::Buy => level * (1.0 + reach),
            TakerSide::Sell => level * (1.0 - reach),
        };
        limit_error = limit_error.max((record.limit_usd - exact).abs() / tick);
    }
    let flow = report.flow.unwrap();
    let failed_share = flow.failed as f64 / flow.sent.max(1) as f64;
    let pass = flow.sent == 200
        && failed_share <= MAX_FAILED_TAKER_SHARE
        && outside == 0
        && spurious == 0
        && limit_error <= 0.5 + 1e-6
        && rounding_loss < 1.0;
    let details = format!(
        "sent {} failed {} ({:.1}%); fully filled {filled}, partial {partial}, unfilled {none} (reachable but unfilled {spurious}); \
         fills outside their limit {outside} ({price_checks} fill prices read); limits {limit_error:.2} ticks from exact; filled {:.1} of {:.1} units; worst fill {worst_fill_bps:.1} bp off the level; \
         size rounding loss {rounding_loss:.2} lots",
        flow.sent,
        flow.failed,
        failed_share * 100.0,
        filled_volume,
        volume,
    );
    table("2 takers", symbol, status(pass), details.clone());
    assert!(pass, "{details}");
}

per_market!(takers, takers_run);

// 3. competitor -----------------------------------------------------------

/// The cents of a USD price.
fn cents(usd: f64) -> i64 {
    (usd * 100.0).round() as i64
}

/// The competitor steps one tick inside a rival, stops at its floor, rounds
/// its quotes to the tick, and leans against its inventory. It works around
/// a level it is told: here the price the splines are centred on (the
/// index), which is where a session's level is once its first move has
/// re-centred the makers.
async fn competitor_run(symbol: &str) {
    let rig = Rig::new(symbol, 3).await;
    rig.widen(15.0);
    let tick = rig.tick_usd();
    let level = rig.usd(rig.book().await.index);
    let tick_bps = tick / level * 1e4;
    let making = Making {
        competitor_bps: 15.0,
        competition: Competition {
            floor_bps: 4.0,
            react_slots: 2,
            size: 5.0,
            reactive: None,
        },
        takers: Vec::new(),
    };
    let mut maker = Competitor::new(rig.seat(0).await, &making, [0; 32]);
    let bot = rig.seat(1).await;
    let taker = rig.seat(2).await;
    let mut at = 0u64;
    let mut notes = Vec::new();
    let mut pass = true;
    let mut tie = false;
    macro_rules! look {
        () => {{
            at += 2;
            rig.step(2).await;
            maker
                .after_blocks(&Moment {
                    at,
                    level_usd: level,
                    book_usd: level,
                    fair_usd: None,
                })
                .await;
        }};
    }
    let on_grid = |usd: f64, up: bool| {
        let ticks = usd / tick;
        (if up { ticks.ceil() } else { ticks.floor() }) * tick
    };

    // It quotes at its half-spread first: the makers are there too.
    look!();
    let first = rig.book().await;
    let (b, a) = (rig.usd(first.bid.unwrap()), rig.usd(first.ask.unwrap()));
    let first_bps = ((level - b) / level * 1e4, (a - level) / level * 1e4);
    let ok = (first_bps.0 - 15.0).abs() <= tick_bps * 2.0
        && (first_bps.1 - 15.0).abs() <= tick_bps * 2.0;
    pass &= ok;
    notes.push(format!(
        "opens {:.2}/{:.2} bp{}",
        first_bps.0,
        first_bps.1,
        if ok { "" } else { " WRONG" }
    ));

    // A rival at 9 bp: on its next look it steps one tick inside, both sides.
    let rival_bid = on_grid(level * (1.0 - 9.0 / 1e4), false);
    let rival_ask = on_grid(level * (1.0 + 9.0 / 1e4), true);
    bot.rest(Side::Bid, rival_bid, 1.0).await.unwrap();
    bot.rest(Side::Ask, rival_ask, 1.0).await.unwrap();
    look!();
    let inside = rig.book().await;
    let (b, a) = (rig.usd(inside.bid.unwrap()), rig.usd(inside.ask.unwrap()));
    let gap = (cents(b) - cents(rival_bid), cents(rival_ask) - cents(a));
    // A tick inside on a side is where it steps; a side it rounds back onto
    // the rival's own price (a tie, which the rival's time priority wins)
    // is a rounding the step can't avoid on that side.
    let ok = (0..=1).contains(&gap.0) && (0..=1).contains(&gap.1) && gap.0 + gap.1 >= 1;
    pass &= ok;
    tie |= gap != (1, 1);
    notes.push(format!(
        "rival at 9 bp: {} / {} cents inside it{}",
        gap.0,
        gap.1,
        if !ok {
            " WRONG"
        } else if gap != (1, 1) {
            " (a tie on one side)"
        } else {
            ""
        }
    ));

    // A rival inside its floor: it stops at the floor.
    bot.cancel_all().await.unwrap();
    let tight_bid = on_grid(level * (1.0 - 2.0 / 1e4), false);
    let tight_ask = on_grid(level * (1.0 + 2.0 / 1e4), true);
    bot.rest(Side::Bid, tight_bid, 1.0).await.unwrap();
    bot.rest(Side::Ask, tight_ask, 1.0).await.unwrap();
    let mut lowest: f64 = f64::MAX;
    for _ in 0..6 {
        look!();
        let book = rig.book().await;
        // Its own quotes sit behind the rival's: read them where the rival
        // isn't, in the bid it would lift: the tightest bps it ever showed.
        let _ = book;
    }
    bot.cancel_all().await.unwrap();
    rig.step(1).await;
    let floor = rig.book().await;
    let (b, a) = (rig.usd(floor.bid.unwrap()), rig.usd(floor.ask.unwrap()));
    let (bid_bps, ask_bps) = ((level - b) / level * 1e4, (a - level) / level * 1e4);
    lowest = lowest.min(bid_bps).min(ask_bps);
    // Never inside the floor (less the tick it rounds out by), and within a
    // tick of it or the easing that follows a tick out from it.
    let ok =
        lowest >= 4.0 - 1e-9 && bid_bps <= 4.0 + tick_bps * 3.0 && ask_bps <= 4.0 + tick_bps * 3.0;
    pass &= ok;
    notes.push(format!(
        "rival past the floor: rests at {bid_bps:.2}/{ask_bps:.2} bp (floor 4, tick {tick_bps:.2}){}",
        if ok { "" } else { " WRONG" }
    ));

    // Inventory: a taker sells into its bid, and it leans its quotes.
    taker.take(Side::Ask, 5.0, level * 0.99).await.unwrap();
    for _ in 0..3 {
        look!();
    }
    let held = rig.trader(0).position().await.unwrap().size;
    let leaned = rig.book().await;
    let (b, a) = (rig.usd(leaned.bid.unwrap()), rig.usd(leaned.ask.unwrap()));
    let (bid_bps, ask_bps) = ((level - b) / level * 1e4, (a - level) / level * 1e4);
    // Skew 1 bp per unit held: bid and ask move together, against it.
    let measured = (bid_bps - ask_bps) / 2.0;
    let ok = held > 4.0 && (measured - held).abs() <= tick_bps * 2.0 + 0.5;
    pass &= ok;
    notes.push(format!(
        "holding {held:.3} units ({:.0} USD of a 25-unit cap): bid/ask {bid_bps:.2}/{ask_bps:.2} bp, lean {measured:.2} bp for 1 bp/unit{}",
        held * level,
        if ok { "" } else { " WRONG" }
    ));
    let mut report = ActorsReport::default();
    maker.report(&mut report).await;
    let report = report.competitor.unwrap();
    let ok = report.tightest_bps >= 4.0 - tick_bps && report.undercuts > 0;
    pass &= ok;
    notes.push(format!(
        "tightest {:.2} bp, {} undercuts, {} requotes{}",
        report.tightest_bps,
        report.undercuts,
        report.requotes,
        if ok { "" } else { " WRONG" }
    ));
    table(
        "3 competitor",
        symbol,
        if pass && tie { "NOTE" } else { status(pass) },
        notes.join("; "),
    );
    assert!(pass, "{notes:?}");
}

per_market!(competitor, competitor_run);

/// Its quotes land on the tick it works out, across a sweep of levels around
/// the market's price: the bug class is a float that sits a hair under its
/// tick and rounds down a tick more.
async fn competitor_rounding_run(symbol: &str) {
    let rig = Rig::new(symbol, 1).await;
    // Splines far enough out that the sweep never reaches them.
    rig.widen(80.0);
    let base = rig.usd(rig.book().await.index);
    let making = Making {
        competitor_bps: 3.0,
        competition: Competition {
            floor_bps: 1.0,
            react_slots: 1,
            size: 1.0,
            reactive: None,
        },
        takers: Vec::new(),
    };
    let mut maker = Competitor::new(rig.seat(0).await, &making, [0; 32]);
    let (mut off_by_one, mut worse, mut n) = (0, 0, 0);
    let mut samples = Vec::new();
    for i in 0..50u64 {
        // Levels a cent apart through the fifty cents above the price.
        let level_cents = cents(base) as i128 + i as i128;
        let level = level_cents as f64 / 100.0;
        maker
            .after_blocks(&Moment {
                at: i + 1,
                level_usd: level,
                book_usd: level,
                fair_usd: None,
            })
            .await;
        rig.step(1).await;
        let book = rig.book().await;
        // Exact arithmetic on the level in cents and the spread in 0.01 bp.
        let bid_exact = level_cents * (1_000_000 - 300) / 1_000_000;
        let ask_exact = (level_cents * (1_000_000 + 300) + 999_999) / 1_000_000;
        let bid = cents(rig.usd(book.bid.unwrap())) as i128;
        let ask = cents(rig.usd(book.ask.unwrap())) as i128;
        for (side, diff) in [("bid", bid - bid_exact), ("ask", ask - ask_exact)] {
            n += 1;
            if diff != 0 {
                off_by_one += 1;
                if samples.len() < 6 {
                    samples.push(format!("{level:.2} {side} {diff:+}c"));
                }
            }
            if diff.abs() > 1 {
                worse += 1;
            }
        }
    }
    let pass = worse == 0;
    let details = format!(
        "{n} quotes over 50 levels from ${base:.2}: {off_by_one} not on the exact tick ({:.0}%), {worse} by more than one; e.g. {samples:?}",
        off_by_one as f64 / n as f64 * 100.0
    );
    let verdict = if !pass {
        "FAIL"
    } else if off_by_one > 0 {
        "NOTE"
    } else {
        "PASS"
    };
    table(
        "3b competitor tick rounding",
        symbol,
        verdict,
        details.clone(),
    );
    assert!(pass, "{details}");
}

per_market!(competitor_rounding, competitor_rounding_run);

// 4. path moves -----------------------------------------------------------

/// A +6% and a -6% waypoint land in the steps `market_move` plans, and the
/// book and the oracle follow together.
async fn path_run(symbol: &str) {
    let rig = Rig::new(symbol, 1).await;
    let start = rig.start_usd();
    let sc = scenario_of(json!({
        "name": "path", "description": "path", "symbol": symbol, "duration_slots": 60,
        "mandate": mandate(symbol, 1.0),
        "path": [{"slot": 4, "level_pct": 6.0}, {"slot": 24, "level_pct": -6.0}],
    }));
    chain::register_scenario(
        &rig.rpc,
        "path",
        sc.overrides(rig.perp_asset_map(), rig.start_ticks),
    )
    .unwrap();
    let base = rig.rpc.get_slot().unwrap();
    let base_index = rig.book().await.index;
    let mut seen: Vec<(u64, Book)> = Vec::new();
    for _ in 0..45 {
        rig.step(1).await;
        let slot = rig.rpc.get_slot().unwrap() - base;
        seen.push((slot, rig.book().await));
    }
    // Distinct index values after the start, with the slot each arrived at.
    let mut changes: Vec<(u64, u64)> = Vec::new();
    let mut last_index = base_index;
    for (slot, book) in &seen {
        if book.index != last_index {
            changes.push((*slot, book.index));
            last_index = book.index;
        }
    }
    let target = |pct: f64| (rig.start_ticks as f64 * (1.0 + pct / 100.0)).round() as u64;
    let plateau = |to: u64| {
        seen.iter()
            .find(|(slot, _)| *slot >= to)
            .map_or(seen.last().unwrap().1, |(_, book)| *book)
    };
    let up = plateau(16);
    let down = plateau(40);
    let mid = |book: Book| (book.bid.unwrap() + book.ask.unwrap()) as f64 / 2.0;
    let off_bp = |ticks: f64, wanted: u64| (ticks - wanted as f64) / wanted as f64 * 1e4;
    let (index_up, mid_up) = (
        off_bp(up.index as f64, target(6.0)),
        off_bp(mid(up), target(6.0)),
    );
    let (index_down, mid_down) = (
        off_bp(down.index as f64, target(-6.0)),
        off_bp(mid(down), target(-6.0)),
    );
    // Before the first waypoint (slot 4) the index may settle onto the
    // session's starting mark, which the makers' mid is not always at.
    let snap = changes
        .iter()
        .find(|(slot, _)| *slot < 3)
        .map(|(_, t)| *t as i64 - base_index as i64);
    let steps_up = changes
        .iter()
        .filter(|(slot, _)| (3..20).contains(slot))
        .count();
    let steps_down = changes.iter().filter(|(slot, _)| *slot >= 20).count();
    let pass = steps_up == 2
        && steps_down == 3
        && index_up.abs() <= 2.0
        && index_down.abs() <= 2.0
        && mid_up.abs() <= 25.0
        && mid_down.abs() <= 25.0;
    let details = format!(
        "$ {start:.2}: +6% in {steps_up} steps (want 2), then -6% in {steps_down} (want 3), index changed at slots {:?}; \
         index settled {snap:?} ticks onto the starting mark before the first move; \
         index off its target {index_up:.2}/{index_down:.2} bp, book mid {mid_up:.2}/{mid_down:.2} bp",
        changes.iter().map(|(s, _)| *s).collect::<Vec<_>>()
    );
    table("4 path moves", symbol, status(pass), details.clone());
    assert!(pass, "{details}");
}

per_market!(path_moves, path_run);

// 5. carry ----------------------------------------------------------------

/// A premium episode with the funding parameters set as a carry scenario
/// sets them, at the market's own cap, and a tape of taker orders keeping
/// Phoenix's book price (and with it the mark) fresh: funding accrues to a
/// bot held against the premium, and `funding_bound_usd` says what it can.
async fn carry_run(symbol: &str) {
    carry(symbol, 1.5, "5 carry").await
}

/// The same episode at the largest premium a scenario may carry, past what
/// the market's own cap lets one interval pay: funding runs at the cap.
async fn carry_capped_run(symbol: &str) {
    carry(symbol, 3.0, "5b carry at the cap").await
}

async fn carry(symbol: &str, premium_pct: f64, item: &str) {
    let rig = Rig::new(symbol, 3).await;
    let start = rig.start_usd();
    let funding = Funding {
        interval_seconds: 6,
        period_seconds: 48,
        max_rate: real_max_rate(symbol),
        mark: Some(MarkConfig {
            spot_weight: 0,
            perp_weight: 0,
            book_weight: 100,
            ema_ticks: 0,
        }),
    };
    let held = 10.0;
    let tape: Vec<serde_json::Value> = (0..60u64)
        .map(|i| {
            json!({
                "slot": 3 + i * 3,
                "side": if i % 2 == 0 { "buy" } else { "sell" },
                "size": 0.3,
                "limit_bps": 60.0,
                "account": i % 2,
            })
        })
        .collect();
    // A premium that holds from slot 10 to slot 150, restated every eight
    // slots as a carry scenario's episodes are: Phoenix's funding accrues
    // when its oracle updates, and each waypoint is one.
    let mut path: Vec<serde_json::Value> = (0..18u64)
        .map(|i| json!({"slot": 10 + i * 8, "level_pct": 0.0, "premium_pct": premium_pct}))
        .collect();
    path.push(json!({"slot": 150, "level_pct": 0.0, "premium_pct": 0.0}));
    let sc = scenario_of(json!({
        "name": "carry", "description": "carry", "symbol": symbol, "duration_slots": 400,
        "mandate": mandate(symbol, held),
        "path": path,
        "funding": funding,
        "tape": tape,
    }));
    let map = rig.perp_asset_map();
    phoenix::set_mark(
        &rig.world.surfnet,
        &rig.phoenix,
        symbol,
        funding.mark.as_ref().unwrap(),
    )
    .unwrap();
    chain::register_scenario(
        &rig.rpc,
        "funding",
        vec![scenario::funding_override(map, symbol, &funding, 1)],
    )
    .unwrap();
    rig.step(2).await;
    phoenix::check_funding(&rig.rpc, &rig.phoenix, symbol, &funding).unwrap();
    chain::register_scenario(&rig.rpc, "carry", sc.overrides(map, rig.start_ticks)).unwrap();
    let base = rig.rpc.get_slot().unwrap();
    // The tape, from the takers' own accounts (1 and 2), priced off the book.
    let seats = vec![rig.seat(1).await, rig.seat(2).await];
    let mut takers = Takers::new(seats, sc.tape.clone(), Pricing::Book, 0);

    // Short before the premium opens, held to the end of the episode.
    rig.trader(0)
        .place_market(Side::Ask, held, start * 0.98)
        .await
        .unwrap();
    let opened = rig.trader(0).position().await.unwrap();
    let mark_open = rig.usd(rig.book().await.mark);
    let value_open = rig.value(0, true).await;
    let mut series = Vec::new();
    let mut views = Vec::new();
    let mut premium_seen: f64 = 0.0;
    loop {
        let at = rig.rpc.get_slot().unwrap() - base;
        if at >= 200 {
            break;
        }
        rig.step(1).await;
        let at = rig.rpc.get_slot().unwrap() - base;
        takers
            .before_blocks(&Moment {
                at,
                level_usd: start,
                book_usd: start * (1.0 + mtx_grader::making::book_at(&sc.path, at) / 100.0),
                fair_usd: None,
            })
            .await;
        if at % 25 == 0 {
            let book = rig.book().await;
            let premium = (rig.usd(book.mark) - rig.usd(book.index)) / rig.usd(book.index) * 100.0;
            premium_seen = premium_seen.max(premium);
            // Funding is what the account gained beyond the position's move.
            let funding_usd =
                rig.value(0, true).await - value_open + held * (rig.usd(book.mark) - mark_open);
            let (accumulated, unsettled, rate) = rig.funding_view(0).await;
            views.push(format!("{at}:{accumulated}/{unsettled}/{rate}"));
            series.push((at, premium, funding_usd));
        }
    }
    let accrued = series.last().unwrap().2;
    let bound = sc.funding_bound_usd(start) * held / sc.max_position();
    let mut report = ActorsReport::default();
    takers.report(&mut report).await;
    let flow = report.flow.unwrap();
    let ratio = accrued / bound;
    let pass = opened.size < -held * 0.999
        && flow.failed == 0
        && accrued > 0.0
        && (0.8..=1.05).contains(&ratio);
    let details = format!(
        "short {:.3}; tape sent {} failed {}; peak premium {premium_seen:.2}% (set {premium_pct}%); funding {accrued:.2} USD against bound {bound:.2} ({:.0}%); series (slot, premium %, funding USD) {:?}; Phoenix's view (slot:accumulated/unsettled lots/rate micro-bp) {views:?}",
        -opened.size,
        flow.sent,
        flow.failed,
        ratio * 100.0,
        series
            .iter()
            .map(|(s, p, f)| format!("{s}:{p:.2}:{f:.1}"))
            .collect::<Vec<_>>()
    );
    table(item, symbol, status(pass), details.clone());
    assert!(pass, "{details}");
}

per_market!(carry, carry_run);
per_market!(carry_capped, carry_capped_run);

// 6. meter and valuation --------------------------------------------------

/// A blind-long bot holds its position through a +6% move and a -6% move:
/// the meter reads the position and its value at each price, and the PnL is
/// position x move less fees.
async fn meter_run(symbol: &str) {
    let rig = Rig::new(symbol, 1).await;
    let start = rig.start_usd();
    let market = rig.phoenix.market(symbol).unwrap().clone();
    let lot_units = rig.phoenix.lots_to_base_units(&market, 1);
    let tick = rig.tick_usd();
    let units = 10.0;
    let value_start = rig.value(0, false).await;
    rig.trader(0)
        .place_market(Side::Bid, units, start * 1.02)
        .await
        .unwrap();
    let position = rig.trader(0).position().await.unwrap();
    let value_open = rig.value(0, false).await;
    let fee = market.taker_fee * position.size * position.entry_usd;
    let slippage = position.size * (position.entry_usd - start);

    let sc = scenario_of(json!({
        "name": "meter", "description": "meter", "symbol": symbol, "duration_slots": 60,
        "mandate": mandate(symbol, units),
        "path": [{"slot": 2, "level_pct": 6.0}, {"slot": 22, "level_pct": -6.0}],
    }));
    chain::register_scenario(
        &rig.rpc,
        "path",
        sc.overrides(rig.perp_asset_map(), rig.start_ticks),
    )
    .unwrap();
    let registered = rig.rpc.get_slot().unwrap();
    let mut notes = Vec::new();
    let mut worst: f64 = 0.0;
    let mut pass = (position.size - units).abs() <= lot_units / 2.0;
    for (until, label) in [(18u64, "+6%"), (44, "-6%")] {
        while rig.rpc.get_slot().unwrap() < registered + until {
            rig.step(1).await;
        }
        let book = rig.book().await;
        let value = rig.value(0, false).await;
        let held = rig.trader(0).position().await.unwrap().size;
        let mark = rig.usd(book.mark);
        // Value = start, less what opening cost, plus the position's move
        // from its entry to the mark.
        let expected = value_start - fee + held * (mark - position.entry_usd);
        let err = (value - expected).abs();
        worst = worst.max(err);
        notes.push(format!(
            "{label}: mark {mark:.2}, position {held:.3}, value {:+.4} vs {:+.4} ({err:.5} off)",
            value - value_start,
            expected - value_start
        ));
        pass &= (held - position.size).abs() < lot_units / 2.0 && err <= 0.01;
    }
    let details = format!(
        "bought {:.3} at {:.2} (slip {slippage:.4}, fee {fee:.4}, opening value change {:+.4}); {}; worst error {worst:.5} USD (1 lot x 1 tick = {:.6})",
        position.size,
        position.entry_usd,
        value_open - value_start,
        notes.join("; "),
        lot_units * tick
    );
    table("6 meter", symbol, status(pass), details.clone());
    assert!(pass, "{details}");
}

per_market!(meter, meter_run);

// 7. oracle writers -------------------------------------------------------

fn phoenix_off_price(symbol: &str, bias_pct: f64, from_slot: u64, slots: u64) -> Oracles {
    Oracles {
        seed: 1,
        feeds: vec![Feed {
            name: "phoenix".into(),
            source: Source::Phoenix(PhoenixOracle {
                symbol: symbol.into(),
            }),
            anchor: None,
            cadence_slots: 4,
            path: Vec::new(),
            conf_bps: 2.0,
            failures: vec![Failure {
                mode: Mode::OffPrice { bias_pct },
                window: Window {
                    from_slot,
                    from_slot_max: None,
                    slots: Some(slots),
                },
            }],
        }],
    }
}

/// A 2% `off_price` window moves the oracle and leaves the book.
async fn oracle_run(symbol: &str) {
    let rig = Rig::new(symbol, 1).await;
    let start = rig.book().await;
    let oracles = phoenix_off_price(symbol, 2.0, 16, 40);
    oracles::register(
        &rig.rpc,
        &oracles,
        100,
        &oracles::World {
            phoenix: Some((rig.perp_asset_map(), rig.start_ticks)),
            ..Default::default()
        },
    )
    .unwrap();
    let base = rig.rpc.get_slot().unwrap();
    let mut rows: Vec<(u64, Book)> = Vec::new();
    for _ in 0..80 {
        rig.step(1).await;
        rows.push((rig.rpc.get_slot().unwrap() - base, rig.book().await));
    }
    let at = |slot: u64| rows.iter().find(|(s, _)| *s >= slot).unwrap().1;
    let before = at(10);
    let inside = at(36);
    let after = at(75);
    let pct = |ticks: u64, wanted: u64| (ticks as f64 / wanted as f64 - 1.0) * 100.0;
    // The writer starts from the session's starting mark, so the oracle's
    // first publication settles the index onto it, and the window's bias
    // is on that.
    let basis = pct(rig.start_ticks, start.index);
    let index_inside = pct(inside.index, rig.start_ticks);
    let index_after = pct(after.index, rig.start_ticks);
    let bid_moved = inside.bid.unwrap() as i64 - start.bid.unwrap() as i64;
    let ask_moved = inside.ask.unwrap() as i64 - start.ask.unwrap() as i64;
    let mark_inside = pct(inside.mark, start.mark);
    let pass = pct(before.index, rig.start_ticks).abs() < 0.01
        && (index_inside - 2.0).abs() < 0.01
        && index_after.abs() < 0.01
        && bid_moved.abs() <= 1
        && ask_moved.abs() <= 1;
    let details = format!(
        "oracle {index_inside:+.3}% inside the window and {index_after:+.3}% after it (from the starting mark; the snapshot's index sat {basis:+.3}% from it); book moved {bid_moved}/{ask_moved} ticks; mark {mark_inside:+.3}% (Phoenix's blend)"
    );
    table("7 oracle off_price", symbol, status(pass), details.clone());
    assert!(pass, "{details}");
}

per_market!(oracle, oracle_run);

// 7b. Pricing::Fair smoke -------------------------------------------------

/// During an oracle `off_price` window, takers priced off the fair path
/// fill against a quoter centred on the oracle, and don't against one
/// centred on fair; takers priced off the oracle's level do the opposite.
async fn fair_run(symbol: &str) {
    let rig = Rig::new(symbol, 6).await;
    // Seats 0, 1 and 2 quote (one per arm), 3 and 4 take. The splines sit 3%
    // out, beyond both quoters.
    rig.widen(300.0);
    let start = rig.book().await;
    let fair = rig.usd(start.index);
    let oracles = phoenix_off_price(symbol, 2.0, 4, 60);
    oracles::register(
        &rig.rpc,
        &oracles,
        100,
        &oracles::World {
            phoenix: Some((rig.perp_asset_map(), rig.start_ticks)),
            ..Default::default()
        },
    )
    .unwrap();
    rig.step(12).await;
    let inside = rig.book().await;
    let oracle = rig.usd(inside.index);
    assert!(
        oracle > fair * 1.019,
        "the oracle is off: {oracle} vs {fair}"
    );

    let making = Making {
        competitor_bps: 15.0,
        competition: Competition {
            floor_bps: 5.0,
            react_slots: 1,
            size: 50.0,
            reactive: None,
        },
        takers: Vec::new(),
    };
    // 10 buys and 10 sells of 0.1 unit, 6 bp from the price they're
    // priced off.
    let orders = |from_slot: u64| -> Vec<TakerOrder> {
        (0..20)
            .map(|i| TakerOrder {
                slot: from_slot + i,
                side: if i % 2 == 0 {
                    TakerSide::Buy
                } else {
                    TakerSide::Sell
                },
                size: 0.1,
                limit_bps: 6.0,
                account: 0,
            })
            .collect()
    };
    let mut rows = Vec::new();
    let mut pass = true;
    // (arm, quoter centred on, takers priced off)
    for (arm, centre_oracle, pricing) in [
        ("fair takers vs oracle-centred quoter", true, Pricing::Fair),
        ("fair takers vs fair-centred quoter", false, Pricing::Fair),
        (
            "oracle-priced takers vs fair-centred quoter",
            false,
            Pricing::Level,
        ),
    ] {
        let quoter_account = rows.len();
        let quoter_seat = rig.seat(quoter_account).await;
        let cancel = rig.seat(quoter_account).await;
        let mut quoter = Competitor::new(quoter_seat, &making, [0; 32]);
        let level_for_quoter = if centre_oracle { oracle } else { fair };
        let moment = |at: u64| Moment {
            at,
            // The taker's `Level` is the oracle's; `Fair` reads the fair path.
            level_usd: oracle,
            book_usd: oracle,
            fair_usd: Some(fair),
        };
        quoter
            .after_blocks(&Moment {
                at: 1,
                level_usd: level_for_quoter,
                book_usd: level_for_quoter,
                fair_usd: None,
            })
            .await;
        rig.step(1).await;
        // Takers: accounts 2 and 3 (buys from 2, sells from 3).
        let seats = vec![rig.seat(3).await, rig.seat(4).await];
        let flow: Vec<TakerOrder> = orders(1)
            .into_iter()
            .map(|mut o| {
                o.account = if o.side == TakerSide::Buy { 0 } else { 1 };
                o
            })
            .collect();
        let before = (
            rig.trader(3).position().await.unwrap().size,
            rig.trader(4).position().await.unwrap().size,
        );
        let quote_book = rig.book().await;
        let mut takers = Takers::new(seats, flow, pricing, 0);
        takers.before_blocks(&moment(30)).await;
        let mut taker_report = ActorsReport::default();
        takers.report(&mut taker_report).await;
        let failed = taker_report.flow.as_ref().map_or(0, |f| f.failed);
        let first_error = taker_report
            .taker_orders
            .iter()
            .find_map(|o| o.error.clone())
            .unwrap_or_default();
        let after = (
            rig.trader(3).position().await.unwrap().size,
            rig.trader(4).position().await.unwrap().size,
        );
        let (bought, sold) = (after.0 - before.0, before.1 - after.1);
        cancel.cancel_all().await.unwrap();
        // Close what the takers took so the next arm starts flat.
        rig.step(1).await;
        let (want_buy, want_sell) = match (centre_oracle, pricing) {
            // The oracle-centred quoter's asks are 2% above the fair
            // takers' buy limits; its bids are above their sell limits.
            (true, Pricing::Fair) => (false, true),
            (false, Pricing::Fair) => (false, false),
            // Oracle-priced takers reach 2% above fair: the fair quoter's
            // asks are inside their buy limits, its bids above their sell
            // limits.
            (false, Pricing::Level) => (true, false),
            _ => unreachable!(),
        };
        let ok = (bought > 0.5) == want_buy && (sold > 0.5) == want_sell;
        pass &= ok;
        rows.push(format!(
            "{arm}: bought {bought:.2}, sold {sold:.2} (book {:.2}/{:.2} against fair {fair:.2}, oracle {oracle:.2}; {failed} failed {}){}",
            rig.usd(quote_book.bid.unwrap()),
            rig.usd(quote_book.ask.unwrap()),
            first_error.chars().take(80).collect::<String>(),
            if ok { "" } else { " WRONG" }
        ));
    }
    table(
        "7b Pricing::Fair smoke",
        symbol,
        status(pass),
        rows.join("; "),
    );
    assert!(pass, "{rows:?}");
}

per_market!(fair_smoke, fair_run);

// 8. refill ---------------------------------------------------------------

/// Take three times the top region's depth with an IOC, then read the best
/// ask and the splines' unfilled size every slot for 60 slots: does depth
/// come back, and how fast. Then a small market move, as a session's own
/// path makes.
async fn refill_run(symbol: &str) {
    let rig = Rig::new(symbol, 1).await;
    let before = rig.book().await;
    let regions = rig.ask_regions();
    let top = regions[0];
    let unfilled = |regions: &[(u64, u64, u64, u64)]| regions.iter().map(|r| r.2).sum::<u64>();
    let market = rig.phoenix.market(symbol).unwrap().clone();
    let lot_units = rig.phoenix.lots_to_base_units(&market, 1);
    // Three times the top region's total, in lots.
    let take_lots = top.1 * 3;
    let units = take_lots as f64 * lot_units;
    let limit = rig.usd(before.mark) * 1.05;
    let sent = rig
        .trader(0)
        .place_market(Side::Bid, units, limit)
        .await
        .map_err(|e| e.to_string());
    let filled = rig
        .trader(0)
        .position()
        .await
        .map(|p| p.size)
        .unwrap_or(0.0);
    let after_regions = rig.ask_regions();
    let after = rig.book().await;
    let taken = unfilled(&regions) as i64 - unfilled(&after_regions) as i64;
    let mut series = Vec::new();
    for slot in 1..=60u64 {
        rig.step(1).await;
        if slot % 10 == 0 || slot == 1 {
            let book = rig.book().await;
            series.push((slot, book.ask.unwrap(), unfilled(&rig.ask_regions())));
        }
    }
    let last = *series.last().unwrap();
    let back_lots = last.2 as i64 - unfilled(&after_regions) as i64;
    let ask_ticks_before = before.ask.unwrap();
    // A small market move, as a path waypoint makes it.
    let sc = scenario_of(json!({
        "name": "move", "description": "move", "symbol": symbol, "duration_slots": 30,
        "mandate": mandate(symbol, 1.0),
        "path": [{"slot": 2, "level_pct": 0.5}],
    }));
    chain::register_scenario(
        &rig.rpc,
        "move",
        sc.overrides(rig.perp_asset_map(), rig.start_ticks),
    )
    .unwrap();
    rig.step(8).await;
    let moved = rig.book().await;
    let moved_unfilled = unfilled(&rig.ask_regions());
    let refilled_in_time = back_lots * 2 >= taken && taken > 0;
    let restored_at_move = moved_unfilled as i64 - unfilled(&after_regions) as i64;
    let details = format!(
        "top region {} lots ({} per tick) at +{} ticks, asked {} lots: {} ({filled:.3} units filled), consumed {taken} lots; \
         best ask {} -> {} ticks (+{}); after 60 slots {} (unfilled back {back_lots} lots, {:.0}% of it); \
         series (slot, best ask, unfilled): {series:?}; after a +0.5% move best ask {} and unfilled {moved_unfilled} (was {}, {restored_at_move} lots back, {taken} taken)",
        top.1,
        top.3,
        top.0,
        take_lots,
        match &sent {
            Ok(_) => "sent".to_string(),
            Err(e) => format!("rejected ({})", e.chars().take(60).collect::<String>()),
        },
        ask_ticks_before,
        after.ask.unwrap(),
        after.ask.unwrap() as i64 - ask_ticks_before as i64,
        last.1,
        if taken > 0 {
            back_lots as f64 / taken as f64 * 100.0
        } else {
            0.0
        },
        moved.ask.unwrap(),
        unfilled(&after_regions),
    );
    // A measurement: it passes when the take went through and was measured.
    let pass = sent.is_ok() && taken > 0;
    table(
        &format!(
            "8 refill ({})",
            if refilled_in_time {
                "REFILLS"
            } else {
                "NO REFILL"
            }
        ),
        symbol,
        status(pass),
        details.clone(),
    );
    assert!(pass, "{details}");
}

per_market!(refill, refill_run);

// 8b. refill actor --------------------------------------------------------

/// Half-lives the curve is measured at, in slots; the run without the actor
/// is the control.
const HALF_LIVES: [f64; 3] = [4.0, 12.0, 40.0];

/// One world: take the book's first 25 ticks with an IOC, then watch the best
/// ask for 60 slots, with the refill actor at `half_life` (none: not built).
/// Returns the ticks the best ask sits above where it started at slots 0
/// (just after the take), 1, 2, 5, 10, 20, 40 and 60, and the actor's tally.
async fn refill_curve(symbol: &str, half_life: Option<f64>) -> (Vec<(u64, i64)>, i64, u64) {
    let rig = Rig::new(symbol, 2).await;
    let market = rig.phoenix.market(symbol).unwrap().clone();
    let lot_units = rig.phoenix.lots_to_base_units(&market, 1);
    let regions = rig.ask_regions();
    let top = regions[0];
    // The actor's tick is the spline's own density: `size_mult` 1 of a
    // "trade" that size.
    let scale = mtx_grader::actors::library::Scale {
        trades_per_minute: 1.0,
        trade_size: top.3 as f64 * lot_units,
        trade_size_sigma: 0.0,
    };
    let seat = rig
        .perp
        .unique_seat(rig.keys[1].insecure_clone(), &rig.url)
        .unwrap();
    let mut built = half_life.map(|h| {
        let library: Library = serde_json::from_value(json!({
            "scale": scale,
            "refill": { "half_life_slots": [h, h], "levels": 5, "size_mult": 1.0, "react_slots": 1 }
        }))
        .unwrap();
        library.validate().unwrap();
        assert_eq!(library.wallets(), [("refill", 0)]);
        let start = rig.start_usd();
        let setting = Setting {
            symbol: symbol.to_string(),
            base_slot: 0,
            path: Arc::new(move |at| moment(at, start)),
            settlement: None,
            send_reader: None,
            liquidations: None,
            rpc_url: None,
        };
        library.build(
            vec![WalletSeat {
                pubkey: rig.keys[1].pubkey(),
                seat,
            }],
            setting,
            &|_, _| [5; 32],
        )
    });
    let start = rig.start_usd();
    let mut slot = 0u64;
    // The first look notes the makers' touch, before anything is taken.
    if let Some(built) = built.as_mut() {
        for actor in built.actors.iter_mut() {
            actor.after_blocks(&moment(slot, start)).await;
        }
    }
    let before = rig.book().await;
    // Enough to clear the first 25 ticks of the book.
    let lots: u64 = regions
        .iter()
        .map(|r| r.2.min(r.3 * 25u64.saturating_sub(r.0)))
        .sum();
    let units = lots as f64 * lot_units;
    rig.trader(0)
        .place_market(Side::Bid, units, rig.usd(before.mark) * 1.05)
        .await
        .unwrap();
    let ask0 = before.ask.unwrap() as i64;
    let mut curve = vec![(0, rig.book().await.ask.unwrap() as i64 - ask0)];
    for _ in 1..=60u64 {
        rig.step(1).await;
        slot += 1;
        if let Some(built) = built.as_mut() {
            for actor in built.actors.iter_mut() {
                actor.after_blocks(&moment(slot, start)).await;
            }
        }
        if [1, 2, 5, 10, 20, 40, 60].contains(&slot) {
            curve.push((slot, rig.book().await.ask.unwrap() as i64 - ask0));
        }
    }
    let mut refilled = 0;
    if let Some(built) = built {
        let mut report = ActorsReport::default();
        for mut actor in built.actors {
            actor.report(&mut report).await;
        }
        refilled = report.library.map_or(0, |l| {
            l.actors
                .iter()
                .filter(|a| a.kind == "refill")
                .map(|a| a.counts.get("ticks_refilled").copied().unwrap_or(0))
                .sum()
        });
    }
    (curve, ask0, refilled)
}

fn moment(at: u64, usd: f64) -> Moment {
    Moment {
        at,
        level_usd: usd,
        book_usd: usd,
        fair_usd: None,
    }
}

/// Without the actor the gap a take leaves stays; with it, the gap halves in
/// the configured half-life, and the curve follows `0.5^(slot / half_life)`
/// within two ticks.
async fn refill_actor_run(symbol: &str) {
    let (control, _, _) = refill_curve(symbol, None).await;
    let gap0 = control[0].1;
    let stays = control.iter().all(|&(_, gap)| gap == gap0);
    let mut pass = stays && gap0 >= 20;
    let mut rows = vec![format!("off: gap {gap0} ticks at slot 0, {control:?}")];
    for h in HALF_LIVES {
        let (curve, _, refilled) = refill_curve(symbol, Some(h)).await;
        let start = curve[0].1;
        let mut worst = 0.0f64;
        for &(slot, gap) in &curve {
            let expected = start as f64 * 0.5f64.powf(slot as f64 / h);
            // The actor stops within `min_gap_ticks` of the makers' touch.
            worst = worst.max(gap as f64 - expected.max(1.0));
        }
        let half_at = curve
            .iter()
            .find(|&&(_, gap)| gap * 2 <= start)
            .map(|&(slot, _)| slot);
        let at_60 = curve.last().unwrap().1;
        let on_curve = worst <= 2.0 + start as f64 * 0.1;
        pass &= on_curve && at_60 < start && refilled > 0;
        rows.push(format!(
            "half-life {h}: gap {start} -> {at_60} at slot 60, halved by slot {half_at:?}, \
             {refilled} ticks refilled, worst miss {worst:.1} ticks; curve (slot, gap): {curve:?}"
        ));
    }
    table("8b refill actor", symbol, status(pass), rows.join(" | "));
    assert!(pass, "{rows:?}");
}

per_market!(refill_actor, refill_actor_run);

// 6. the reactive competitor ---------------------------------------------

fn reactive_making(bps: f64, floor: f64, react: u64, size: f64) -> Making {
    Making {
        competitor_bps: bps,
        competition: Competition {
            floor_bps: floor,
            react_slots: react,
            size,
            reactive: Some(Reactive {
                jitter: 0.0,
                step_ticks: (1, 1),
                ..Reactive::default()
            }),
        },
        takers: Vec::new(),
    }
}

/// With the reactive competitor: it steps exactly one tick inside a rival on
/// both sides (never a tie), ignores odd lots, and its quotes sit on the
/// exact tick at every level of a sweep.
async fn reactive_run(symbol: &str) {
    let rig = Rig::new(symbol, 3).await;
    rig.widen(15.0);
    let tick = rig.tick_usd();
    let level = rig.usd(rig.book().await.index);
    let making = reactive_making(15.0, 4.0, 2, 5.0);
    let mut maker = Competitor::new(rig.seat(0).await, &making, [5; 32]);
    let bot = rig.seat(1).await;
    let mut at = 0u64;
    macro_rules! look {
        () => {{
            at += 2;
            rig.step(2).await;
            maker
                .after_blocks(&Moment {
                    at,
                    level_usd: level,
                    book_usd: level,
                    fair_usd: None,
                })
                .await;
        }};
    }
    let on_grid = |usd: f64, up: bool| {
        let ticks = usd / tick;
        (if up { ticks.ceil() } else { ticks.floor() }) * tick
    };
    look!();
    let rival_bid = on_grid(level * (1.0 - 9.0 / 1e4), false);
    let rival_ask = on_grid(level * (1.0 + 9.0 / 1e4), true);
    bot.rest(Side::Bid, rival_bid, 1.0).await.unwrap();
    bot.rest(Side::Ask, rival_ask, 1.0).await.unwrap();
    look!();
    let book = rig.book().await;
    let (b, a) = (rig.usd(book.bid.unwrap()), rig.usd(book.ask.unwrap()));
    let gap = (cents(b) - cents(rival_bid), cents(rival_ask) - cents(a));
    let one_tick = cents(tick);
    table(
        "5 reactive step inside",
        symbol,
        status(gap == (one_tick, one_tick)),
        format!(
            "rival at 9 bp: {} / {} cents inside it (tick {one_tick}c)",
            gap.0, gap.1
        ),
    );
    assert_eq!(gap, (one_tick, one_tick), "one tick inside on both sides");

    // Odd lots 5 bp out, under the reaction size: it doesn't step in on them.
    bot.cancel_all().await.unwrap();
    let odd_bid = on_grid(level * (1.0 - 5.0 / 1e4), false);
    let odd_ask = on_grid(level * (1.0 + 5.0 / 1e4), true);
    bot.rest(Side::Bid, odd_bid, 0.4).await.unwrap();
    bot.rest(Side::Ask, odd_ask, 0.4).await.unwrap();
    let mut before = ActorsReport::default();
    maker.report(&mut before).await;
    for _ in 0..4 {
        look!();
    }
    let mut after = ActorsReport::default();
    maker.report(&mut after).await;
    let (u0, u1) = (
        before.competitor.unwrap().undercuts,
        after.competitor.unwrap().undercuts,
    );
    table(
        "5 reactive odd lots",
        symbol,
        status(u0 == u1),
        format!("quotes of 0.4 at 5 bp: undercuts {u0} -> {u1}"),
    );
    assert_eq!(u0, u1, "odd lots draw no undercut");
    bot.cancel_all().await.unwrap();
}

per_market!(reactive, reactive_run);

/// The reactive competitor's quotes land on the exact tick across a sweep of
/// levels: no quote one tick off (the legacy competitor's float drift).
async fn reactive_rounding_run(symbol: &str) {
    let rig = Rig::new(symbol, 1).await;
    rig.widen(80.0);
    let base = rig.usd(rig.book().await.index);
    let making = reactive_making(3.0, 1.0, 1, 1.0);
    let mut maker = Competitor::new(rig.seat(0).await, &making, [6; 32]);
    let (mut off, mut n) = (0, 0);
    let mut samples = Vec::new();
    for i in 0..50u64 {
        let level_cents = cents(base) as i128 + i as i128;
        let level = level_cents as f64 / 100.0;
        maker
            .after_blocks(&Moment {
                at: i + 1,
                level_usd: level,
                book_usd: level,
                fair_usd: None,
            })
            .await;
        rig.step(1).await;
        let book = rig.book().await;
        // Whole cents (the markets' tick is one cent): the half-spread is the
        // 3 bp of the level rounded up to a tick, from the level's cent.
        let width = (level_cents * 300 + 999_999) / 1_000_000;
        let bid = cents(rig.usd(book.bid.unwrap())) as i128;
        let ask = cents(rig.usd(book.ask.unwrap())) as i128;
        for (side, diff) in [
            ("bid", bid - (level_cents - width)),
            ("ask", ask - (level_cents + width)),
        ] {
            n += 1;
            if diff != 0 {
                off += 1;
                if samples.len() < 6 {
                    samples.push(format!("{level:.2} {side} {diff:+}c"));
                }
            }
        }
    }
    table(
        "5b reactive tick rounding",
        symbol,
        status(off == 0),
        format!(
            "{n} quotes over 50 levels from ${base:.2}: {off} not on the exact tick; {samples:?}"
        ),
    );
    assert_eq!(off, 0, "{samples:?}");
}

per_market!(reactive_rounding, reactive_rounding_run);
