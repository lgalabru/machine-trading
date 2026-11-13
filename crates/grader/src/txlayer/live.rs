//! The transaction layer against a real surfnet: what lands, what doesn't,
//! what it costs, and that the same sends build the same blocks.

use std::sync::Arc;

use base64::Engine as _;
use serde_json::{Value, json};
use solana_keypair::Keypair;
use solana_message::{Message, VersionedMessage};
use solana_pubkey::Pubkey;
use solana_rpc_client::rpc_client::RpcClient;
use solana_signer::Signer;
use solana_transaction::{AccountMeta, Hash, Instruction, versioned::VersionedTransaction};
use surfpool_sdk::SimnetCommand;

use super::{
    HotAccount, RegimeWindow, Routes, TxEngine, TxLayer,
    engine::Fate,
    jito,
    pool::{COMPUTE_BUDGET, SYSTEM},
};
use crate::{guard::Guard, keys::SessionKeys};

const SOL: u64 = 1_000_000_000;

fn hot() -> Pubkey {
    Pubkey::new_from_array([42; 32])
}

fn layer(spike: bool) -> TxLayer {
    TxLayer {
        seed: 3,
        calibration: "fixture-fees.20261113.00000000".into(),
        hot_accounts: vec![HotAccount {
            address: hot().to_string(),
            class: "amm_pool".into(),
        }],
        regimes: if spike {
            vec![RegimeWindow {
                from_step: 0,
                to_step: 300,
                regime: "spike".into(),
            }]
        } else {
            Vec::new()
        },
        drawn_regimes: false,
        routes: Routes::default(),
        validators: 24,
        max_pending: 64,
        fees_in_pnl: false,
        native_sol: Some(10.0),
        sol_usd: None,
    }
}

struct World {
    world: crate::snapshot::OfflineWorld,
    _internal: Guard,
    rpc: RpcClient,
    guard: Guard,
    engine: Arc<TxEngine>,
    bot: Keypair,
    base: u64,
    slot: u64,
}

/// A stepped world with the layer on: the bot holds 10 SOL, the tip
/// accounts exist. `floor`: a congestion floor on the block, by step.
async fn world(layer: TxLayer, floor: Option<(u64, u64, u64)>) -> World {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../snapshots/phoenix-spcx-2026-10-29");
    let world = crate::snapshot::start_offline_stepped(&dir).await.unwrap();
    let internal = Guard::internal(world.surfnet.rpc_url()).await.unwrap();
    let rpc = mtx_kit::rpc_client(internal.url());
    let bot = SessionKeys::from_eval_seed(b"a batch secret of at least 32 chars")
        .session("bot/0/tx")
        .keypair("bot", 0);
    super::setup(&world.surfnet, &layer, &bot.pubkey()).unwrap();
    let slot = rpc.get_slot().unwrap();
    let engine = TxEngine::start(
        &layer,
        internal.url(),
        slot,
        slot + 600,
        2,
        floor,
        bot.insecure_clone(),
    )
    .await
    .unwrap();
    let guard = Guard::start(world.surfnet.rpc_url()).await.unwrap();
    guard.set_tx_layer(engine.clone());
    guard.set_step(0);
    engine.set_step(0);
    World {
        world,
        _internal: internal,
        rpc,
        guard,
        engine,
        bot,
        base: slot,
        slot,
    }
}

impl World {
    /// Build, commit and produce the next slot.
    async fn slot(&mut self) {
        let next = self.slot + 1;
        let picks = self.engine.plan(next);
        for signature in self.engine.commit(next, picks).await {
            self.guard.landed(&signature, 0);
        }
        self.world
            .surfnet
            .send_command(SimnetCommand::SlotForward(None))
            .unwrap();
        self.slot = crate::session::wait_for_slot(&self.rpc, next)
            .await
            .unwrap();
        self.engine.produced(self.slot).await.unwrap();
        // A turn every two slots.
        let step = (self.slot - self.base) / 2;
        self.guard.set_step(step);
        self.engine.set_step(step);
    }

    async fn slots(&mut self, n: u64) {
        for _ in 0..n {
            self.slot().await;
        }
    }

    fn blockhash(&self) -> Hash {
        self.rpc.get_latest_blockhash().unwrap()
    }

    fn balance(&self, key: &Pubkey) -> u64 {
        self.rpc.get_balance(key).unwrap()
    }

    fn landed(&self, signature: &str) -> bool {
        crate::chain::call(
            &self.rpc,
            "getTransaction",
            json!([signature, { "encoding": "json", "commitment": "confirmed", "maxSupportedTransactionVersion": 0 }]),
        )
        .unwrap()
        .is_object()
    }
}

/// A transfer from `payer` to each `(to, lamports)`, with a limit and price
/// if given, and a tip if given.
fn transfer(
    payer: &Keypair,
    to: &[(Pubkey, u64)],
    limit: Option<u32>,
    price: Option<u64>,
    tip: Option<u64>,
    blockhash: Hash,
) -> (Vec<u8>, String) {
    let mut ixs = Vec::new();
    if let Some(limit) = limit {
        let mut data = vec![2];
        data.extend(limit.to_le_bytes());
        ixs.push(Instruction::new_with_bytes(COMPUTE_BUDGET, &data, vec![]));
    }
    if let Some(price) = price {
        let mut data = vec![3];
        data.extend(price.to_le_bytes());
        ixs.push(Instruction::new_with_bytes(COMPUTE_BUDGET, &data, vec![]));
    }
    let pay = |to: Pubkey, lamports: u64| {
        let mut data = 2u32.to_le_bytes().to_vec();
        data.extend(lamports.to_le_bytes());
        Instruction::new_with_bytes(
            SYSTEM,
            &data,
            vec![
                AccountMeta::new(payer.pubkey(), true),
                AccountMeta::new(to, false),
            ],
        )
    };
    for (to, lamports) in to {
        ixs.push(pay(*to, *lamports));
    }
    if let Some(tip) = tip {
        ixs.push(pay(jito::TIP_ACCOUNTS[0].parse().unwrap(), tip));
    }
    let message = Message::new_with_blockhash(&ixs, Some(&payer.pubkey()), &blockhash);
    let tx = VersionedTransaction::try_new(VersionedMessage::Legacy(message), &[payer]).unwrap();
    (
        wincode::serialize(&tx).unwrap(),
        tx.signatures[0].to_string(),
    )
}

fn b64(wire: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(wire)
}

async fn post(url: &str, body: Value) -> Value {
    reqwest::Client::new()
        .post(url)
        .json(&body)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

async fn send(url: &str, wire: &[u8], config: Value) -> Value {
    post(
        url,
        json!({ "jsonrpc": "2.0", "id": 1, "method": "sendTransaction", "params": [b64(wire), config] }),
    )
    .await
}

async fn jito(world: &World, method: &str, params: Value) -> Value {
    post(
        &format!("{}/api/v1/bundles", world.guard.url()),
        json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }),
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn a_fee_spike_lands_who_pays_and_the_cheap_send_never_lands() {
    // A floor of 200,000 micro-lamports per compute unit for 150 steps.
    let mut w = world(layer(true), Some((0, 150, 200_000))).await;
    let start = w.balance(&w.bot.pubkey());
    let (paid, paid_sig) = transfer(
        &w.bot,
        &[(hot(), SOL)],
        Some(10_000),
        Some(20_000_000),
        None,
        w.blockhash(),
    );
    let (cheap, cheap_sig) = transfer(
        &w.bot,
        &[(Pubkey::new_unique(), SOL)],
        Some(10_000),
        None,
        None,
        w.blockhash(),
    );
    let config = json!({ "encoding": "base64" });
    assert_eq!(
        send(w.guard.url(), &paid, config.clone()).await["result"],
        paid_sig
    );
    assert_eq!(
        send(w.guard.url(), &cheap, config).await["result"],
        cheap_sig
    );
    w.slots(6).await;
    assert!(
        matches!(
            w.engine.fate(&paid_sig),
            Some(Fate::Landed { ok: true, .. })
        ),
        "{:?}",
        w.engine.fate(&paid_sig)
    );
    assert!(w.landed(&paid_sig));
    // The cheap one waits out its blockhash, or is lost on the way.
    w.slots(160).await;
    assert!(
        matches!(
            w.engine.fate(&cheap_sig),
            Some(Fate::Expired | Fate::Pending)
        ),
        "{:?}",
        w.engine.fate(&cheap_sig)
    );
    assert!(!w.landed(&cheap_sig));
    // The bot paid the transfer and its fee: the base fee and 20M
    // micro-lamports on its 10,000 requested units.
    let fee = 5_000 + 200_000;
    assert_eq!(w.balance(&w.bot.pubkey()), start - SOL - fee);
    let report = w.engine.report(None);
    assert_eq!((report.fees.base, report.fees.priority), (5_000, 200_000));
    let states: Vec<&str> = report.sends.iter().map(|s| s.state).collect();
    assert_eq!(states[0], "landed");
    assert!(matches!(states[1], "expired" | "lost"), "{states:?}");
    // What the guard says of fees shows the spike.
    let fees = post(
        w.guard.url(),
        json!({ "jsonrpc": "2.0", "id": 1, "method": "getRecentPrioritizationFees", "params": [[hot().to_string()]] }),
    )
    .await;
    let fees = fees["result"].as_array().unwrap();
    assert_eq!(fees.len(), 150);
    assert!(
        fees.iter()
            .all(|f| f["prioritizationFee"].as_u64().unwrap() >= 200_000)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn bundles_land_whole_or_not_at_all_and_a_failed_send_pays_its_fee() {
    let mut w = world(layer(false), None).await;
    let start = w.balance(&w.bot.pubkey());
    let tip_account: Pubkey = jito::TIP_ACCOUNTS[0].parse().unwrap();
    let tips_before = w.balance(&tip_account);
    let (y, z, y2) = (
        Pubkey::new_unique(),
        Pubkey::new_unique(),
        Pubkey::new_unique(),
    );
    let h = w.blockhash();
    let good = [
        transfer(&w.bot, &[(y, SOL)], None, None, None, h),
        transfer(&w.bot, &[(z, SOL)], None, None, Some(10_000), h),
    ];
    // Its second transaction leaves a new account below rent: it fails.
    let bad = [
        transfer(&w.bot, &[(y2, SOL)], None, None, None, h),
        transfer(
            &w.bot,
            &[(Pubkey::new_unique(), 1)],
            None,
            None,
            Some(10_000),
            h,
        ),
    ];
    let bundle = |txs: &[(Vec<u8>, String)]| json!([txs.iter().map(|(w, _)| b64(w)).collect::<Vec<_>>(), { "encoding": "base64" }]);
    let good_id = jito(&w, "sendBundle", bundle(&good)).await["result"]
        .as_str()
        .unwrap()
        .to_string();
    let bad_id = jito(&w, "sendBundle", bundle(&bad)).await["result"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        good_id,
        jito::bundle_id(&[good[0].1.clone(), good[1].1.clone()])
    );
    // A transaction that lands and fails still pays its fee.
    let (fails, fails_sig) = transfer(&w.bot, &[(Pubkey::new_unique(), 1)], None, None, None, h);
    send(w.guard.url(), &fails, json!({ "encoding": "base64" })).await;
    w.slots(40).await;

    let statuses = jito(&w, "getBundleStatuses", json!([[good_id, bad_id]])).await;
    let value = &statuses["result"]["value"];
    assert!(value[0].is_object(), "{statuses}");
    assert!(
        value[1].is_null(),
        "the bad bundle never landed: {statuses}"
    );
    let inflight = jito(&w, "getInflightBundleStatuses", json!([[bad_id]])).await;
    assert_eq!(inflight["result"]["value"][0]["status"], "Failed");
    // Both of the good bundle's transactions ran, in one slot.
    assert_eq!(w.balance(&y), SOL);
    assert_eq!(w.balance(&z), SOL);
    assert_eq!(w.balance(&y2), 0, "nothing of the bad bundle committed");
    assert_eq!(w.balance(&tip_account), tips_before + 10_000);
    let slots: Vec<u64> = good
        .iter()
        .map(|(_, sig)| match w.engine.fate(sig) {
            Some(Fate::Landed { slot, .. }) => slot,
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(slots[0], slots[1]);
    assert!(matches!(
        w.engine.fate(&fails_sig),
        Some(Fate::Landed { ok: false, .. })
    ));
    // The good bundle's two fees and tip, the failed send's fee, and the
    // two SOL: the bad bundle cost nothing.
    assert_eq!(
        w.balance(&w.bot.pubkey()),
        start - 2 * SOL - 10_000 - 3 * 5_000
    );
    let ledger = w.engine.report(None).fees;
    assert_eq!(
        (ledger.base, ledger.tips, ledger.failed_tx_fees),
        (10_000, 10_000, 5_000)
    );
    // The tip accounts are Jito's, and the tip floor is served.
    let tips = jito(&w, "getTipAccounts", json!([])).await;
    assert_eq!(tips["result"], json!(jito::TIP_ACCOUNTS));
    let floor: Value = reqwest::get(format!("{}/api/v1/bundles/tip_floor", w.guard.url()))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(floor[0]["landed_tips_50th_percentile"].as_f64().unwrap() > 0.0);
}

#[tokio::test(flavor = "multi_thread")]
async fn retries_rebroadcast_and_sign_again_only_after_expiry() {
    // The floor holds for 100 steps (200 slots): longer than a blockhash.
    let mut w = world(layer(false), Some((0, 100, 200_000))).await;
    let (q, twice) = (Pubkey::new_unique(), Pubkey::new_unique());
    let (wire, signature) = transfer(&w.bot, &[(q, SOL)], None, None, None, w.blockhash());
    let retry = json!({ "encoding": "base64", "mtx": { "route": "rpc", "retryUntilSlot": w.slot + 300, "resign": true } });
    send(&w.guard.host_url(), &wire, retry.clone()).await;
    // Sent again, the same bytes: one entry, which runs once.
    send(&w.guard.host_url(), &wire, retry).await;
    // A bot that signs again before expiry sends a second transaction.
    let (first, first_sig) = transfer(
        &w.bot,
        &[(twice, SOL)],
        Some(10_000),
        Some(400_000),
        None,
        w.blockhash(),
    );
    send(w.guard.url(), &first, json!({ "encoding": "base64" })).await;
    w.slots(2).await;
    let (second, second_sig) = transfer(
        &w.bot,
        &[(twice, SOL)],
        Some(10_000),
        Some(400_000),
        None,
        w.blockhash(),
    );
    send(w.guard.url(), &second, json!({ "encoding": "base64" })).await;
    w.slots(240).await;

    let Some(Fate::Landed {
        signature: landed_as,
        ok: true,
        ..
    }) = w.engine.fate(&signature)
    else {
        panic!("{:?}", w.engine.fate(&signature));
    };
    assert_ne!(landed_as, signature, "signed again on a new blockhash");
    assert!(!w.landed(&signature), "the first signature never ran");
    assert_eq!(w.balance(&q), SOL, "it ran once");
    let report = w.engine.report(None);
    let resent = &report.sends[0];
    assert_eq!((resent.state, resent.resigned), ("landed", 1));
    assert_eq!(report.sends.len(), 3);
    // Signed again before expiry, both ran.
    assert!(w.landed(&first_sig) && w.landed(&second_sig));
    assert_eq!(w.balance(&twice), 2 * SOL);
}

/// Every slot's block for a scripted session: the same sends in a fresh
/// world build the same blocks, with or without congestion.
async fn scripted(spike: bool) -> crate::txlayer::report::TxLayerReport {
    let floor = spike.then_some((0, 30, 200_000));
    let mut w = world(layer(spike), floor).await;
    for round in 0..6u64 {
        let h = w.blockhash();
        for i in 0..5u64 {
            let price = (i % 3) * 150_000 + round * 10_000;
            let to = Pubkey::new_from_array([(round * 5 + i) as u8 + 1; 32]);
            let (wire, _) = transfer(
                &w.bot,
                &[(to, SOL / 10), (hot(), 1_000_000)],
                Some(20_000),
                Some(price),
                None,
                h,
            );
            send(w.guard.url(), &wire, json!({ "encoding": "base64" })).await;
        }
        w.slots(4).await;
    }
    w.slots(30).await;
    w.engine.report(None)
}

#[tokio::test(flavor = "multi_thread")]
async fn the_same_sends_build_the_same_blocks() {
    for spike in [false, true] {
        let first = scripted(spike).await;
        let second = scripted(spike).await;
        assert!(!first.blocks.is_empty());
        assert_eq!(first, second, "spike {spike}");
    }
}

/// The probe (`bots/tx-probe`, `just build`) as `MTX_TEST_TX_PROBE_WASM`,
/// graded in its scenario: its options take effect, what it paid counts in
/// its PnL and matches its wallet, and a second grade is the first.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a built component"]
async fn the_tx_probe_pays_for_what_it_lands_and_replays() {
    let wasm = std::path::PathBuf::from(std::env::var("MTX_TEST_TX_PROBE_WASM").unwrap());
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let scenario =
        crate::scenario::Scenario::load(&root.join("bots/tx-probe/scenario.json")).unwrap();
    let mut unpriced = scenario.clone();
    unpriced.tx_layer.as_mut().unwrap().fees_in_pnl = false;
    let keys = SessionKeys::random();
    let mut runs = Vec::new();
    for (run, scenario) in [&scenario, &scenario, &unpriced].into_iter().enumerate() {
        let dir = std::env::temp_dir().join(format!("mtx-tx-probe-{}-{run}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::copy(&wasm, dir.join("bot.wasm")).unwrap();
        let log = dir.join("bot.log");
        let report = crate::session::run(crate::session::SessionOptions {
            snapshot_dir: &root.join("snapshots/phoenix-spcx-2026-10-29"),
            scenario,
            bot_dir: &dir,
            bot_log: &log,
            slot: 0,
            regime: None,
            cranks: None,
            trusted: false,
            keys: keys.role("bot").session(0, &scenario.name),
            observer: None,
        })
        .await
        .unwrap();
        let log = std::fs::read_to_string(&log).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        runs.push((report, log));
    }
    let (report, log) = &runs[0];
    assert!(
        log.contains("applies tx options true, fee reports true"),
        "{log}"
    );
    let has = |line: &str| log.lines().any(|l| l.contains(line));
    assert!(has("step 2 line 0: landed"), "{log}");
    assert!(
        has("step 3 line 0: landed") && has("step 3 line 1: landed"),
        "{log}"
    );
    assert!(
        has("step 5 line 0: failed") && has("step 5 line 1: failed"),
        "{log}"
    );
    assert!(has("step 6 line 0: failed"), "{log}");
    assert!(has("step 22 line 1: landed"), "{log}");
    assert!(
        has("step 22 line 0: expired") || has("step 22 line 0: delayed"),
        "{log}"
    );
    assert!(has("step 24 line 0: landed"), "{log}");
    let tx = report.tx_layer.as_ref().unwrap();
    assert_eq!(tx.fees.unexplained, Some(0), "{:?}", tx.fees);
    assert!(
        tx.fees.tips >= 20_000 && tx.fees.failed_tx_fees >= 5_000,
        "{:?}",
        tx.fees
    );
    let paid = (tx.fees.base + tx.fees.priority + tx.fees.tips + tx.fees.failed_tx_fees) as f64;
    let unpriced_pnl = runs[2].0.pnl_usd;
    assert!(
        (report.pnl_usd - (unpriced_pnl - paid * 150.0 / 1e9)).abs() < 1e-6,
        "{} vs {unpriced_pnl} less {paid} lamports",
        report.pnl_usd
    );
    // The second grade is the first, block for block.
    let strip = |r: &crate::session::SessionReport| {
        let mut v = serde_json::to_value(r).unwrap();
        v["wall_ms"] = Value::Null;
        v["turns"]["wall_ms"] = Value::Null;
        v
    };
    assert_eq!(strip(&runs[0].0), strip(&runs[1].0));
}
