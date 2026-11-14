//! Phoenix's liquidation path on the fork, as the keeper meets it: who may
//! call it, what a call closes and what it costs, on the 96-market snapshot.
//!
//! One trader goes long at high leverage, an oracle-and-makers move takes
//! it past maintenance margin, and a keeper's real transactions
//! (`LiquidateViaMarketOrder`, built by [`mtx_grader::keeper`]) liquidate it.
//! What a test measures is printed as a `TABLE|item|PASS or FAIL|numbers`
//! line (run with `--nocapture`), and asserted after printing. It is slow
//! (it starts a world), so it's ignored by default.
//!
//! ```sh
//! cargo test -p mtx-grader --test phoenix_liquidation -- --ignored --nocapture
//! ```

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use mtx_core::AccountMeter;
use mtx_grader::{
    chain::{self, Override},
    guard::Guard,
    keeper::{Keys, force_cancel_risk_increasing, liquidate_via_market_order, liquidation_transfer},
    phoenix::{self, Phoenix},
    snapshot,
    venue::{Market, perps::PerpMarket},
};
use mtx_kit::{
    Side,
    chain::RpcChainClient,
    phoenix_rise::ix::hawkeye::{ViewAssetReturn, ViewMarginReturn},
    protocols::phoenix::eternal::{PhoenixEternalMeter, PhoenixExchange},
};
use serde_json::json;
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_rpc_client::rpc_client::RpcClient;
use solana_signer::Signer;

const SNAPSHOT: &str = "snapshots/phoenix-perps-2026-11-13";
const SYMBOL: &str = "NVDA";
/// What a session gives the bot, and the keeper.
const DEPOSIT_USDC: f64 = 10_000.0;
const KEEPER_DEPOSIT_USDC: f64 = 100_000.0;
/// The position the victim opens, in shares: about 15x its collateral.
const SHARES: f64 = 900.0;
/// The most one liquidation may close in this test, in base lots.
const MAX_LIQUIDATION_LOTS: u64 = 300_000;

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn key(i: u8) -> Keypair {
    let mut seed = [9u8; 32];
    seed[0] = i;
    seed[1] = 0x41;
    Keypair::new_from_array(seed)
}

fn table(item: &str, status: &str, details: String) {
    println!("TABLE|{item}|{status}|{details}");
}

fn status(pass: bool) -> &'static str {
    if pass { "PASS" } else { "FAIL" }
}

struct Rig {
    world: snapshot::OfflineWorld,
    _guard: Guard,
    rpc: RpcClient,
    url: String,
    phoenix: Phoenix,
    perp: PerpMarket,
    victim: Keypair,
    keeper: Keypair,
    outsider: Keypair,
    victim_trader: Pubkey,
    keeper_trader: Pubkey,
    keys: Keys,
}

impl Rig {
    async fn new() -> Rig {
        let world = snapshot::start_offline_stepped(&root().join(SNAPSHOT))
            .await
            .unwrap();
        let guard = Guard::internal(world.surfnet.rpc_url()).await.unwrap();
        let url = guard.url().to_string();
        let rpc = mtx_kit::rpc_client_with_timeout(&url, Duration::from_secs(600));
        let phoenix = Phoenix::new(world.exchange.clone(), &url);
        let (victim, keeper, outsider) = (key(0), key(1), key(2));
        let victim_trader =
            phoenix::onboard_traders_stepped(&world.surfnet, &phoenix, &rpc, &[&victim], DEPOSIT_USDC)
                .await
                .unwrap()[0];
        let keeper_trader = phoenix::onboard_traders_stepped(
            &world.surfnet,
            &phoenix,
            &rpc,
            &[&keeper],
            KEEPER_DEPOSIT_USDC,
        )
        .await
        .unwrap()[0];
        // A wallet with SOL for fees and nothing else: no trader account.
        world
            .surfnet
            .cheatcodes()
            .fund_sol(&outsider.pubkey(), 1_000_000_000)
            .unwrap();
        chain::register_scenario(
            &rpc,
            "readings",
            vec![phoenix::enable_trader(victim_trader, 1)],
        )
        .unwrap();
        chain::next_slot(&world.surfnet, &rpc).await.unwrap();
        chain::next_slot(&world.surfnet, &rpc).await.unwrap();
        let perp = PerpMarket::new(
            world.exchange.clone(),
            world.exchange_path.clone(),
            SYMBOL,
            &url,
            victim.pubkey(),
        )
        .unwrap();
        let keys = Keys::of(
            phoenix.exchange(),
            phoenix.market(SYMBOL).unwrap(),
        )
        .unwrap();
        Rig {
            world,
            _guard: guard,
            rpc,
            url,
            phoenix,
            perp,
            victim,
            keeper,
            outsider,
            victim_trader,
            keeper_trader,
            keys,
        }
    }

    async fn step(&self, slots: u64) {
        for _ in 0..slots {
            chain::next_slot(&self.world.surfnet, &self.rpc)
                .await
                .unwrap();
        }
    }

    fn market(&self) -> mtx_kit::phoenix_rise::types::exchange::ExchangeMarketConfig {
        self.phoenix.market(SYMBOL).unwrap().clone()
    }

    async fn margin(&self) -> ViewMarginReturn {
        self.phoenix
            .hawkeye(self.keeper.pubkey())
            .view_margin_for_trader(self.victim_trader)
            .await
            .unwrap()
            .value
    }

    async fn asset(&self) -> ViewAssetReturn {
        self.phoenix
            .hawkeye(self.keeper.pubkey())
            .view_margin_for_asset_for_trader(self.victim_trader, self.market().asset_id)
            .await
            .unwrap()
            .value
    }

    async fn mark_ticks(&self) -> u64 {
        self.phoenix
            .hawkeye(self.keeper.pubkey())
            .view_bbo(SYMBOL)
            .await
            .unwrap()
            .value
            .mark_price_ticks
    }

    async fn value(&self) -> f64 {
        let meter = PhoenixEternalMeter::new(
            Arc::new(PhoenixExchange::new(self.world.exchange.clone())),
            &self.url,
            Arc::new(RpcChainClient::with_timeout(
                &self.url,
                Duration::from_secs(600),
            )),
            self.victim.pubkey(),
            SYMBOL,
        )
        .unwrap();
        let id = self.perp.instrument().id();
        meter
            .measure(std::slice::from_ref(&id))
            .await
            .unwrap()
            .value_usd
            .to_string()
            .parse()
            .unwrap()
    }

    /// A liquidation of the victim by `by`, at most `lots` base lots, as the
    /// keeper sends it.
    async fn liquidate(&self, by: &Keypair, lots: u64, long: bool) -> Result<(), String> {
        let mark = self.mark_ticks().await;
        let through = mark / 10;
        let limit = if long { mark - through } else { mark + through };
        let ix = liquidate_via_market_order(&self.keys, by.pubkey(), self.victim_trader, lots, limit);
        mtx_kit::send(&self.rpc, by, vec![ix], "liquidate")
            .map(|_| ())
            .map_err(|error| error.to_string())
    }
}

fn short(reason: &str) -> String {
    reason
        .lines()
        .rev()
        .find(|l| l.contains("Error") || l.contains("failed") || l.contains("custom"))
        .unwrap_or(reason)
        .chars()
        .take(160)
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "slow: starts a world; run with --ignored"]
async fn liquidation_rules() {
    let rig = Rig::new().await;
    let market = rig.market();
    let lot = rig.phoenix.lots_to_base_units(&market, 1);
    let start_ticks = rig.mark_ticks().await;
    let start_usd = rig.phoenix.ticks_to_usd(&market, start_ticks);

    // The most one call may close: the market's own, then a small one so a
    // position takes several calls.
    chain::register_scenario(
        &rig.rpc,
        "max liquidation size",
        vec![Override {
            template_id: "phoenix-market-max-liquidation-size",
            account: rig.phoenix.perp_asset_map().unwrap(),
            values: json!({ "symbol": SYMBOL, "maxLiquidationSize": MAX_LIQUIDATION_LOTS.to_string() }),
            slot: 1,
            label: "max liquidation size".into(),
        }],
    )
    .unwrap();
    rig.step(2).await;

    rig.perp
        .trader(rig.victim.insecure_clone(), &rig.url)
        .unwrap()
        .place_market(Side::Bid, SHARES, start_usd * 1.03)
        .await
        .unwrap();
    let opened = rig.asset().await;
    let healthy = rig.margin().await;
    let notional = opened.base_lots as f64 * lot * start_usd;
    table(
        "setup",
        status(healthy.is_liquidatable == 0 && opened.base_lots > 0),
        format!(
            "long {} lots (${notional:.0}, {:.1}x its ${DEPOSIT_USDC} of collateral) at mark {start_usd:.2}: tier {}, liquidatable {}, maintenance margin {:.0} of {:.0} effective",
            opened.base_lots,
            notional / DEPOSIT_USDC,
            healthy.risk_tier,
            healthy.is_liquidatable,
            healthy.maintenance_margin_quote_lots as f64 / 1e6,
            healthy.effective_collateral_quote_lots as f64 / 1e6,
        ),
    );
    assert!(healthy.is_liquidatable == 0 && opened.base_lots > 0);

    // A healthy account is refused.
    let refused = rig.liquidate(&rig.keeper, MAX_LIQUIDATION_LOTS, true).await;
    table(
        "healthy account",
        status(refused.is_err()),
        format!("refused: {}", refused.as_ref().err().map_or("NOT REFUSED".into(), |e| short(e))),
    );
    assert!(refused.is_err(), "a healthy account was liquidated");

    // The move: down until the account reads liquidatable.
    let map = rig.phoenix.perp_asset_map().unwrap();
    let mut from = start_ticks;
    let mut dropped = 0.0;
    let mut margin = healthy;
    for pct in [3.0, 4.0, 5.0, 5.5, 6.0, 6.5] {
        let to = (start_ticks as f64 * (1.0 - pct / 100.0)).round() as u64;
        let (moves, last) = phoenix::market_move(map, SYMBOL, from, to, 1);
        chain::register_scenario(&rig.rpc, "move", moves).unwrap();
        rig.step(last + 4).await;
        from = to;
        dropped = pct;
        margin = rig.margin().await;
        if margin.is_liquidatable != 0 {
            break;
        }
    }
    let before = rig.asset().await;
    let value_before = rig.value().await;
    let mark_before = rig.phoenix.ticks_to_usd(&market, rig.mark_ticks().await);
    table(
        "past maintenance",
        status(margin.is_liquidatable != 0),
        format!(
            "after -{dropped}%: mark {mark_before:.2}, tier {}, effective collateral {:.0} against maintenance {:.0}, backstop {:.0}, high risk {:.0}",
            margin.risk_tier,
            margin.effective_collateral_quote_lots as f64 / 1e6,
            margin.maintenance_margin_quote_lots as f64 / 1e6,
            margin.backstop_margin_quote_lots as f64 / 1e6,
            margin.high_risk_margin_quote_lots as f64 / 1e6,
        ),
    );
    assert!(margin.is_liquidatable != 0, "the move left the account healthy");

    // Who may call: a wallet with SOL and no trader account of its own.
    let too_big = rig.liquidate(&rig.keeper, before.base_lots.unsigned_abs(), true).await;
    table(
        "over the max size",
        status(too_big.is_err()),
        format!(
            "asked {} lots against a max of {MAX_LIQUIDATION_LOTS}: {}",
            before.base_lots,
            too_big.as_ref().err().map_or("NOT REFUSED".into(), |e| short(e))
        ),
    );
    let outsider = rig.liquidate(&rig.outsider, MAX_LIQUIDATION_LOTS, true).await;
    table(
        "permission",
        "NOTE",
        format!(
            "a funded wallet with no trader account: {}",
            outsider.as_ref().map_or_else(|e| format!("refused: {}", short(e)), |_| "liquidated".into())
        ),
    );
    let keeper = if outsider.is_ok() {
        Ok(())
    } else {
        rig.liquidate(&rig.keeper, MAX_LIQUIDATION_LOTS, true).await
    };
    table(
        "permission",
        status(keeper.is_ok()),
        format!(
            "an onboarded keeper: {}",
            keeper.as_ref().map_or_else(|e| format!("refused: {}", short(e)), |_| "liquidated".into())
        ),
    );
    assert!(keeper.is_ok(), "no wallet could liquidate");

    // What each call closes, and what the first cost.
    let after_first = rig.asset().await;
    let value_first = rig.value().await;
    let closed_lots = before.base_lots - after_first.base_lots;
    let closed_usd = closed_lots as f64 * lot * mark_before;
    let cost = value_before - value_first;
    let taker_fee_usd = market.taker_fee * closed_usd;
    table(
        "one call",
        status(closed_lots > 0 && closed_lots as u64 <= MAX_LIQUIDATION_LOTS),
        format!(
            "closed {closed_lots} lots (${closed_usd:.0}) of {} with a cap of {MAX_LIQUIDATION_LOTS}; the account's value fell ${cost:.2} ({:.1} bp of what closed; the taker fee alone is {:.1} bp)",
            before.base_lots,
            cost / closed_usd * 1e4,
            taker_fee_usd / closed_usd * 1e4,
        ),
    );
    assert!(closed_lots > 0 && closed_lots as u64 <= MAX_LIQUIDATION_LOTS);

    // Liquidate on until healthy: how many calls, and the total cost.
    let mut calls = 1;
    let mut held = after_first.base_lots;
    let mut m = rig.margin().await;
    while m.is_liquidatable != 0 && held != 0 && calls < 10 {
        rig.liquidate(&rig.keeper, MAX_LIQUIDATION_LOTS, true).await
            .unwrap_or_else(|e| panic!("call {}: {}", calls + 1, short(&e)));
        calls += 1;
        held = rig.asset().await.base_lots;
        m = rig.margin().await;
    }
    let value_end = rig.value().await;
    table(
        "to healthy",
        status(m.is_liquidatable == 0 || held == 0),
        format!(
            "{calls} calls left {held} lots, tier {}, liquidatable {}; the account lost ${:.2} to liquidation in all",
            m.risk_tier,
            m.is_liquidatable,
            value_before - value_end,
        ),
    );
}

/// Past the backstop margin, which instructions Phoenix lets a keeper use:
/// the market order, and the transfer of the position to the keeper's own
/// account. Each runs on a world of its own.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "slow: starts two worlds; run with --ignored"]
async fn backstop_liquidation() {
    for transfer in [false, true] {
        let rig = Rig::new().await;
        let market = rig.market();
        let lot = rig.phoenix.lots_to_base_units(&market, 1);
        let start_ticks = rig.mark_ticks().await;
        let start_usd = rig.phoenix.ticks_to_usd(&market, start_ticks);
        rig.perp
            .trader(rig.victim.insecure_clone(), &rig.url)
            .unwrap()
            .place_market(Side::Bid, SHARES, start_usd * 1.03)
            .await
            .unwrap();
        let to = (start_ticks as f64 * (1.0 - 6.0 / 100.0)).round() as u64;
        let (moves, last) =
            phoenix::market_move(rig.phoenix.perp_asset_map().unwrap(), SYMBOL, start_ticks, to, 1);
        chain::register_scenario(&rig.rpc, "move", moves).unwrap();
        rig.step(last + 4).await;
        let margin = rig.margin().await;
        let before = rig.asset().await;
        let value_before = rig.value().await;
        let mark = rig.phoenix.ticks_to_usd(&market, rig.mark_ticks().await);
        let name = if transfer { "backstop transfer" } else { "backstop market order" };
        table(
            name,
            "NOTE",
            format!(
                "after -6%: tier {}, effective collateral {:.0} against maintenance {:.0}, backstop {:.0}",
                margin.risk_tier,
                margin.effective_collateral_quote_lots as f64 / 1e6,
                margin.maintenance_margin_quote_lots as f64 / 1e6,
                margin.backstop_margin_quote_lots as f64 / 1e6,
            ),
        );
        assert!(margin.is_liquidatable != 0, "the move left the account healthy");
        let sent = if transfer {
            let ix = liquidation_transfer(
                &rig.keys,
                rig.keeper.pubkey(),
                rig.keeper_trader,
                rig.victim_trader,
                u64::from(market.asset_id),
                before.base_lots.unsigned_abs(),
            );
            mtx_kit::send(&rig.rpc, &rig.keeper, vec![ix], "liquidation transfer")
                .map(|_| ())
                .map_err(|e| e.to_string())
        } else {
            rig.liquidate(&rig.keeper, before.base_lots.unsigned_abs(), true).await
        };
        let after = rig.asset().await;
        let value_after = rig.value().await;
        let moved = before.base_lots - after.base_lots;
        let keeper_position = rig
            .phoenix
            .hawkeye(rig.keeper.pubkey())
            .view_margin_for_asset_for_trader(rig.keeper_trader, market.asset_id)
            .await
            .unwrap()
            .value
            .base_lots;
        table(
            name,
            status(sent.is_ok()),
            format!(
                "{}: the victim's position {} -> {} lots ({:.0} shares at {mark:.2}), its value fell ${:.2}; the keeper's own position is {keeper_position} lots",
                sent.as_ref().map_or_else(|e| format!("refused ({})", short(e)), |_| "landed".into()),
                before.base_lots,
                after.base_lots,
                moved as f64 * lot,
                value_before - value_after,
            ),
        );
    }
}

/// A trader with a risk-increasing order resting can't be liquidated until
/// the keeper force-cancels it: otherwise resting an order would make an
/// account unliquidatable.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "slow: starts a world; run with --ignored"]
async fn resting_orders_are_force_cancelled_first() {
    let rig = Rig::new().await;
    let market = rig.market();
    let start_ticks = rig.mark_ticks().await;
    let start_usd = rig.phoenix.ticks_to_usd(&market, start_ticks);
    rig.perp
        .trader(rig.victim.insecure_clone(), &rig.url)
        .unwrap()
        .place_market(Side::Bid, SHARES, start_usd * 1.03)
        .await
        .unwrap();
    let seat = rig
        .perp
        .seat(rig.victim.insecure_clone(), &rig.url)
        .await
        .unwrap();
    seat.rest(Side::Bid, start_usd * 0.85, 20.0).await.unwrap();
    let map = rig.phoenix.perp_asset_map().unwrap();
    let mut from = start_ticks;
    let mut dropped = 0.0;
    let mut margin = rig.margin().await;
    for pct in [3.0, 4.0, 5.0, 5.5, 6.0] {
        let to = (start_ticks as f64 * (1.0 - pct / 100.0)).round() as u64;
        let (moves, last) = phoenix::market_move(map, SYMBOL, from, to, 1);
        chain::register_scenario(&rig.rpc, "move", moves).unwrap();
        rig.step(last + 4).await;
        from = to;
        dropped = pct;
        margin = rig.margin().await;
        if margin.is_liquidatable != 0 {
            break;
        }
    }
    assert!(margin.is_liquidatable != 0, "no move left the account liquidatable");
    let held = rig.asset().await.base_lots;
    let blocked = rig.liquidate(&rig.keeper, held.unsigned_abs(), true).await;
    table(
        "resting order",
        status(blocked.is_err()),
        format!(
            "after -{dropped}% (tier {}): a liquidation with a bid resting is {}",
            margin.risk_tier,
            blocked.as_ref().err().map_or("NOT REFUSED".into(), |e| format!("refused: {}", short(e)))
        ),
    );
    let cancel = force_cancel_risk_increasing(&rig.keys, rig.keeper.pubkey(), rig.victim_trader);
    let cancelled = mtx_kit::send(&rig.rpc, &rig.keeper, vec![cancel], "force cancel")
        .map(|_| ())
        .map_err(|e| e.to_string());
    let after_cancel = rig.margin().await;
    let again = rig.liquidate(&rig.keeper, held.unsigned_abs(), true).await;
    table(
        "resting order",
        status(cancelled.is_ok() && again.is_ok()),
        format!(
            "the keeper's force cancel: {}; liquidatable after it: {}; then the liquidation: {}",
            cancelled.as_ref().map_or_else(|e| format!("refused ({})", short(e)), |_| "landed".into()),
            after_cancel.is_liquidatable,
            again.as_ref().map_or_else(|e| format!("refused ({})", short(e)), |_| "landed".into()),
        ),
    );
    assert!(blocked.is_err() && cancelled.is_ok() && again.is_ok());
}
