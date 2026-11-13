//! The searchers in whole sessions (`bench/checks/searchers`), with the
//! probes built (`bots/searcher-probe`, `bots/kamino-probe`, `just build`)
//! as `MTX_TEST_SEARCHER_PROBE_WASM` and `MTX_TEST_KAMINO_PROBE_WASM`: each
//! acts on chain through the pool and the builder, paying fees and tips its
//! wallet shows (its ledger explains every lamport), a sandwich or a backrun
//! costs a careless sale and a careful one escapes it, and the same session
//! builds the same blocks twice.

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::{
    keys::SessionKeys,
    scenario::Scenario,
    session::{SessionOptions, SessionReport, run},
    txlayer::report::TxLayerReport,
};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn scenario(name: &str) -> Scenario {
    Scenario::load(&root().join("bench/checks/searchers").join(name)).unwrap()
}

/// `scenario` with `searchers` (JSON) on its layer.
fn with(mut scenario: Scenario, searchers: &str) -> Scenario {
    scenario.tx_layer.as_mut().unwrap().searchers = serde_json::from_str(searchers).unwrap();
    scenario
}

/// Grade `bot` (the wasm a variable names) in `scenario` on `snapshot`,
/// with `regime`: the report and the bot's log.
async fn grade(
    scenario: &Scenario,
    snapshot: &str,
    bot: &str,
    regime: &str,
    keys: &SessionKeys,
) -> (SessionReport, String) {
    let wasm = PathBuf::from(std::env::var(bot).unwrap());
    let dir = std::env::temp_dir().join(format!(
        "mtx-searchers-{}-{}-{regime}-{}",
        std::process::id(),
        scenario.name,
        rand_suffix()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::copy(&wasm, dir.join("bot.wasm")).unwrap();
    let log = dir.join("bot.log");
    let report = run(SessionOptions {
        snapshot_dir: &root().join("snapshots").join(snapshot),
        scenario,
        bot_dir: &dir,
        bot_log: &log,
        slot: None,
        regime: Some(regime),
        cranks: None,
        trusted: false,
        keys: keys.role("bot").session(0, &scenario.name),
        observer: None,
    })
    .await
    .unwrap();
    let log = std::fs::read_to_string(&log).unwrap_or_default();
    std::fs::remove_dir_all(&dir).unwrap();
    (report, log)
}

fn rand_suffix() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    N.fetch_add(1, Ordering::Relaxed)
}

const POOLS: &str = "spcx-pools-2026-11-13";
const PROBE: &str = "MTX_TEST_SEARCHER_PROBE_WASM";

/// Every ledger in the layer's report explains its wallet to the lamport.
fn reconciled(tx: &TxLayerReport) {
    assert_eq!(tx.fees.unexplained, Some(0), "the bot's: {:?}", tx.fees);
    for actor in &tx.actors {
        assert_eq!(
            actor.fees.unexplained,
            Some(0),
            "{}'s: {:?}",
            actor.sender,
            actor.fees
        );
    }
}

fn actor<'a>(tx: &'a TxLayerReport, sender: &str) -> &'a crate::txlayer::report::ActorFees {
    tx.actors
        .iter()
        .find(|a| a.sender == sender)
        .unwrap_or_else(|| panic!("no {sender} in {:?}", tx.actors))
}

fn count(tx: &TxLayerReport, sender: &str, what: &str) -> u64 {
    actor(tx, sender).counts.get(what).copied().unwrap_or(0)
}

/// What the bot's sale brought in, in USDC.
fn proceeds(report: &SessionReport) -> f64 {
    let spot = report.spot.as_ref().unwrap();
    spot.end.quote - spot.start.quote
}

/// The blocks where `sender` landed a bundle holding a transaction of the
/// bot's.
fn around_the_bot(tx: &TxLayerReport, sender: &str) -> usize {
    tx.blocks
        .iter()
        .filter(|b| {
            let bundled = |who: &str| {
                b.landed
                    .iter()
                    .filter(|l| l.sender == who && l.ok)
                    .filter_map(|l| l.bundle)
                    .collect::<Vec<_>>()
            };
            let theirs = bundled(sender);
            bundled("bot").iter().any(|i| theirs.contains(i))
        })
        .count()
}

fn strip(report: &SessionReport) -> Value {
    let mut v = serde_json::to_value(report).unwrap();
    v["wall_ms"] = Value::Null;
    v["turns"]["wall_ms"] = Value::Null;
    for step in v["timeline"]["steps"].as_array_mut().into_iter().flatten() {
        step["turn_ms"] = Value::Null;
    }
    v
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a built component"]
async fn a_sandwich_costs_a_public_sale_and_a_private_or_tight_one_escapes() {
    let keys = SessionKeys::random();
    let plain = scenario("pools.json");
    let sandwiched = with(
        plain.clone(),
        r#"{ "sandwichers": [{ "venue": "raydium", "max_base": 8000.0, "base": 10000.0, "quote": 1500000.0 }] }"#,
    );
    let (control, _) = grade(&plain, POOLS, PROBE, "public", &keys).await;
    let (public, log) = grade(&sandwiched, POOLS, PROBE, "public", &keys).await;
    let (again, _) = grade(&sandwiched, POOLS, PROBE, "public", &keys).await;
    let (private, _) = grade(&sandwiched, POOLS, PROBE, "private", &keys).await;
    let (tight, _) = grade(&sandwiched, POOLS, PROBE, "tight", &keys).await;
    let (tight_alone, _) = grade(&plain, POOLS, PROBE, "tight", &keys).await;
    let sender = "actor:sandwicher/0";
    let tx = public.tx_layer.as_ref().unwrap();
    println!(
        "TABLE sandwich: control {:.4}, public {:.4}, private {:.4}, tight {:.4} (alone {:.4}); sandwicher {:?} {:?}",
        proceeds(&control),
        proceeds(&public),
        proceeds(&private),
        proceeds(&tight),
        proceeds(&tight_alone),
        actor(tx, sender).counts,
        actor(tx, sender).values,
    );
    // The public sale was sandwiched: a bundle of the sandwicher's around
    // it landed, the sale still filled, for less.
    assert_eq!(around_the_bot(tx, sender), 1, "{:#?}\n{log}", tx.blocks);
    assert!(log.contains("Landed"), "{log}");
    assert!(
        proceeds(&public) < proceeds(&control) - 1.0,
        "{} vs {}",
        proceeds(&public),
        proceeds(&control)
    );
    // The sandwicher made money after its fees and tip.
    let made = actor(tx, sender).values["value_change_quote"];
    let paid = actor(tx, sender).fees.total() as f64 * 150.0 / 1e9;
    assert!(made - paid > 0.0, "made {made}, paid {paid} USD");
    reconciled(tx);
    // Sent privately, the sale escapes.
    let tx = private.tx_layer.as_ref().unwrap();
    assert_eq!(around_the_bot(tx, sender), 0, "{:#?}", tx.blocks);
    assert_eq!(count(tx, sender, "sandwiches_sent"), 0);
    reconciled(tx);
    // With a limit just past its own impact, what a sandwich can take is
    // what the limit leaves: a small part of the public sale's loss.
    reconciled(tight.tx_layer.as_ref().unwrap());
    let lost = |r: &SessionReport, alone: &SessionReport| proceeds(alone) - proceeds(r);
    assert!(
        lost(&tight, &tight_alone) < 0.1 * lost(&public, &control),
        "{} vs {}",
        lost(&tight, &tight_alone),
        lost(&public, &control)
    );
    assert!(proceeds(&private) > proceeds(&public) + 1.0);
    // The same session builds the same blocks.
    assert_eq!(strip(&public), strip(&again));
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a built component"]
async fn jit_liquidity_takes_a_public_swaps_fee_and_never_sees_a_private_one() {
    let keys = SessionKeys::random();
    let jit = with(
        scenario("pools.json"),
        r#"{ "jit": [{ "venue": "raydium", "min_base": 100.0, "deposit_base": 4000.0, "deposit_quote": 600000.0, "base": 5000.0, "quote": 800000.0 }] }"#,
    );
    let (public, log) = grade(&jit, POOLS, PROBE, "public", &keys).await;
    let (private, _) = grade(&jit, POOLS, PROBE, "private", &keys).await;
    let sender = "actor:jit/0";
    let tx = public.tx_layer.as_ref().unwrap();
    println!(
        "TABLE jit: public {:.4}, private {:.4}; jit {:?} {:?} {:?}",
        proceeds(&public),
        proceeds(&private),
        actor(tx, sender).counts,
        actor(tx, sender).values,
        actor(tx, sender).fees,
    );
    assert_eq!(around_the_bot(tx, sender), 1, "{:#?}\n{log}", tx.blocks);
    assert!(count(tx, sender, "withdrawals_sent") >= 1);
    // It took liquidity out with the swap's fee in it: more value than it
    // put in, before its fees.
    assert!(actor(tx, sender).values["value_change_quote"] > 0.0);
    reconciled(tx);
    let tx = private.tx_layer.as_ref().unwrap();
    assert_eq!(count(tx, sender, "jit_sent"), 0);
    reconciled(tx);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a built component"]
async fn a_backrun_between_two_halves_costs_the_second_and_one_bundle_leaves_no_gap() {
    let keys = SessionKeys::random();
    let plain = scenario("pools.json");
    let backrun = with(
        plain.clone(),
        r#"{ "backrunners": [{ "venue": "raydium", "other": "meteora", "edge_bps": 10.0, "max_base": 2000.0, "base": 5000.0, "quote": 800000.0 }] }"#,
    );
    let (split_alone, _) = grade(&plain, POOLS, PROBE, "split", &keys).await;
    let (split, log) = grade(&backrun, POOLS, PROBE, "split", &keys).await;
    let (together_alone, _) = grade(&plain, POOLS, PROBE, "together", &keys).await;
    let (together, _) = grade(&backrun, POOLS, PROBE, "together", &keys).await;
    let sender = "actor:backrunner/0";
    let tx = split.tx_layer.as_ref().unwrap();
    println!(
        "TABLE backrun: split {:.4} (alone {:.4}), together {:.4} (alone {:.4}); backrunner {:?} {:?}",
        proceeds(&split),
        proceeds(&split_alone),
        proceeds(&together),
        proceeds(&together_alone),
        actor(tx, sender).counts,
        actor(tx, sender).values,
    );
    for b in tx.blocks.iter().filter(|b| !b.landed.is_empty()) {
        println!(
            "BLOCK {} {:?}",
            b.at,
            b.landed
                .iter()
                .map(|l| (&l.sender, l.ok, l.bundle))
                .collect::<Vec<_>>()
        );
    }
    println!("TOGETHER {:#?}", together.tx_layer.as_ref().unwrap().sends);
    assert!(count(tx, sender, "backruns_sent") >= 1, "{log}");
    let landed = tx
        .blocks
        .iter()
        .flat_map(|b| &b.landed)
        .filter(|l| l.sender == sender && l.ok)
        .count();
    assert!(landed >= 2, "{:#?}", tx.blocks);
    assert!(
        proceeds(&split) < proceeds(&split_alone) - 1.0,
        "{} vs {}",
        proceeds(&split),
        proceeds(&split_alone)
    );
    reconciled(tx);
    let tx = together.tx_layer.as_ref().unwrap();
    reconciled(tx);
    assert!(
        proceeds(&together) >= proceeds(&together_alone) - 1e-6,
        "{} vs {}",
        proceeds(&together),
        proceeds(&together_alone)
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a built component"]
async fn fee_bidders_land_ahead_of_a_cheaper_send_and_pay_for_it() {
    let keys = SessionKeys::random();
    let bidders = with(
        scenario("pools.json"),
        r#"{ "fee_bidders": [{ "venue": "raydium", "per_slot": 0.5, "size": 2.0 }, { "venue": "meteora", "per_slot": 0.5, "size": 2.0 }] }"#,
    );
    let (report, log) = grade(&bidders, POOLS, PROBE, "public", &keys).await;
    let tx = report.tx_layer.as_ref().unwrap();
    println!(
        "TABLE fee bidders: {:?}",
        tx.actors
            .iter()
            .map(|a| (&a.sender, &a.counts, a.fees.priority))
            .collect::<Vec<_>>()
    );
    for sender in ["actor:fee-bidder/0", "actor:fee-bidder/1"] {
        assert!(count(tx, sender, "swaps_sent") >= 3, "{sender}: {log}");
        assert!(actor(tx, sender).fees.priority > 0, "{sender}");
    }
    // In a block with both, the bidders' swaps went first.
    for block in tx
        .blocks
        .iter()
        .filter(|b| b.landed.iter().any(|l| l.sender == "bot"))
    {
        let bot = block.landed.iter().position(|l| l.sender == "bot").unwrap();
        assert!(
            block.landed[bot..]
                .iter()
                .all(|l| !l.sender.starts_with("actor:fee-bidder")),
            "{block:#?}"
        );
    }
    reconciled(tx);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a built component"]
async fn a_pool_reading_front_runner_lands_ahead_of_public_sales_and_not_private_ones() {
    let keys = SessionKeys::random();
    let front = scenario("front-run.json");
    let (public, log) = grade(
        &front,
        "phoenix-spcx-2026-10-29",
        PROBE,
        "perp-public",
        &keys,
    )
    .await;
    let (again, _) = grade(
        &front,
        "phoenix-spcx-2026-10-29",
        PROBE,
        "perp-public",
        &keys,
    )
    .await;
    let (private, _) = grade(
        &front,
        "phoenix-spcx-2026-10-29",
        PROBE,
        "perp-private",
        &keys,
    )
    .await;
    let sender = "actor:front-runner/0";
    let tx = public.tx_layer.as_ref().unwrap();
    println!(
        "TABLE front-run: public {:.4}, private {:.4}; runner {:?}; library {:?}",
        public.pnl_usd,
        private.pnl_usd,
        actor(tx, sender).fees,
        public.actors.as_ref().map(|l| &l.actors),
    );
    // It landed ahead of the bot in the bot's own block.
    let ahead = tx
        .blocks
        .iter()
        .filter(|b| {
            let first = |who: &str| b.landed.iter().position(|l| l.sender == who);
            matches!((first(sender), first("bot")), (Some(f), Some(bot)) if f < bot)
        })
        .count();
    assert!(ahead >= 1, "{:#?}\n{log}", tx.blocks);
    assert!(actor(tx, sender).fees.priority > 0);
    reconciled(tx);
    let tx = private.tx_layer.as_ref().unwrap();
    assert_eq!(actor(tx, sender).fees.transactions, 0, "{:#?}", tx.blocks);
    reconciled(tx);
    assert_eq!(strip(&public), strip(&again));
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a built component"]
async fn liquidators_race_through_the_pool_and_the_losers_pay_unless_bundled() {
    let keys = SessionKeys::random();
    let race = scenario("liquidation-race.json");
    let mut bundled = race.clone();
    bundled
        .tx_layer
        .as_mut()
        .unwrap()
        .searchers
        .liquidators
        .as_mut()
        .unwrap()
        .bundles = true;
    const KAMINO: &str = "kamino-2026-11-13";
    const LIQ: &str = "MTX_TEST_KAMINO_PROBE_WASM";
    let (naive, log) = grade(&race, KAMINO, LIQ, "naive", &keys).await;
    let (bidding, bid_log) = grade(&race, KAMINO, LIQ, "bid", &keys).await;
    let (again, _) = grade(&race, KAMINO, LIQ, "bid", &keys).await;
    let (quiet, _) = grade(&bundled, KAMINO, LIQ, "naive", &keys).await;
    let liquidations = |r: &SessionReport| r.lending.as_ref().unwrap().liquidations;
    let landed = |tx: &TxLayerReport, ok: bool| {
        tx.blocks
            .iter()
            .flat_map(|b| &b.landed)
            .filter(|l| l.sender.starts_with("actor:kamino-liquidator") && l.ok == ok)
            .count()
    };
    let tx = naive.tx_layer.as_ref().unwrap();
    println!(
        "TABLE race: bot liquidations naive {} bidding {}; liquidators landed {} failed {}; bundled failed {}; {:?}",
        liquidations(&naive),
        liquidations(&bidding),
        landed(tx, true),
        landed(tx, false),
        landed(quiet.tx_layer.as_ref().unwrap(), false),
        tx.actors
            .iter()
            .map(|a| (&a.sender, a.fees.failed_tx_fees, a.fees.priority))
            .collect::<Vec<_>>(),
    );
    // The race: liquidations landed, and later ones in the same race failed
    // and paid their fees.
    assert!(landed(tx, true) >= 1, "{:#?}\n{log}", tx.blocks);
    assert!(landed(tx, false) >= 1, "{:#?}", tx.blocks);
    assert!(tx.actors.iter().any(|a| a.fees.failed_tx_fees > 0));
    reconciled(tx);
    // A bot that bids lands more than one that doesn't.
    assert!(
        liquidations(&bidding) > liquidations(&naive),
        "{} vs {}\n{bid_log}",
        liquidations(&bidding),
        liquidations(&naive)
    );
    reconciled(bidding.tx_layer.as_ref().unwrap());
    // As bundles, the losers pay nothing.
    let tx = quiet.tx_layer.as_ref().unwrap();
    assert!(landed(tx, true) >= 1, "{:#?}", tx.blocks);
    assert!(
        tx.actors.iter().all(|a| a.fees.failed_tx_fees == 0),
        "{:?}",
        tx.actors
    );
    reconciled(tx);
    assert_eq!(strip(&bidding), strip(&again));
}
