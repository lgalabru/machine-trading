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

/// A transaction from `payer` of `ixs`, as written.
fn custom(payer: &Keypair, ixs: &[Instruction], blockhash: Hash) -> (Vec<u8>, String) {
    let message = Message::new_with_blockhash(ixs, Some(&payer.pubkey()), &blockhash);
    let tx = VersionedTransaction::try_new(VersionedMessage::Legacy(message), &[payer]).unwrap();
    (
        wincode::serialize(&tx).unwrap(),
        tx.signatures[0].to_string(),
    )
}

fn cu_limit(units: u32) -> Instruction {
    let mut data = vec![2];
    data.extend(units.to_le_bytes());
    Instruction::new_with_bytes(COMPUTE_BUDGET, &data, vec![])
}

fn cu_price(price: u64) -> Instruction {
    let mut data = vec![3];
    data.extend(price.to_le_bytes());
    Instruction::new_with_bytes(COMPUTE_BUDGET, &data, vec![])
}

fn pay(from: &Pubkey, to: Pubkey, lamports: u64) -> Instruction {
    let mut data = 2u32.to_le_bytes().to_vec();
    data.extend(lamports.to_le_bytes());
    Instruction::new_with_bytes(
        SYSTEM,
        &data,
        vec![AccountMeta::new(*from, true), AccountMeta::new(to, false)],
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
    let states: Vec<&str> = report.sends.iter().map(|s| s.state.as_str()).collect();
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
    assert_eq!((resent.state.as_str(), resent.resigned), ("landed", 1));
    assert_eq!(report.sends.len(), 3);
    // Signed again before expiry, both ran.
    assert!(w.landed(&first_sig) && w.landed(&second_sig));
    assert_eq!(w.balance(&twice), 2 * SOL);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bundle_pays_the_congestion_floor_as_a_transaction_does() {
    // A floor of 200,000 micro-lamports per compute unit for 150 steps.
    let mut w = world(layer(false), Some((0, 150, 200_000))).await;
    w.slots(2).await;
    let start = w.balance(&w.bot.pubkey());
    let h = w.blockhash();
    // 200,000 units each: a transaction at half the floor, a bundle of one
    // with the least tip and no priority fee, and one whose tip is worth
    // half again the floor.
    let (plain, plain_sig) = transfer(
        &w.bot,
        &[(Pubkey::new_unique(), SOL)],
        Some(200_000),
        Some(100_000),
        None,
        h,
    );
    let (cheap, cheap_sig) = transfer(
        &w.bot,
        &[(Pubkey::new_unique(), SOL)],
        Some(200_000),
        None,
        Some(jito::MIN_TIP),
        h,
    );
    let (paid, paid_sig) = transfer(
        &w.bot,
        &[(Pubkey::new_unique(), SOL)],
        Some(200_000),
        None,
        Some(60_000),
        h,
    );
    send(w.guard.url(), &plain, json!({ "encoding": "base64" })).await;
    for wire in [&cheap, &paid] {
        let sent = jito(
            &w,
            "sendBundle",
            json!([[b64(wire)], { "encoding": "base64" }]),
        )
        .await;
        assert!(sent["result"].is_string(), "{sent}");
    }
    w.slots(12).await;
    assert!(w.landed(&paid_sig));
    assert!(!w.landed(&cheap_sig) && !w.landed(&plain_sig));
    // It paid for the one that landed: its fee and tip, and the transfer.
    assert_eq!(w.balance(&w.bot.pubkey()), start - SOL - 5_000 - 60_000);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_transaction_sent_alone_and_in_a_bundle_lands_once() {
    let mut w = world(layer(false), None).await;
    w.slots(2).await;
    let h = w.blockhash();
    let (x_to, t_to) = (Pubkey::new_unique(), Pubkey::new_unique());
    let (x, x_sig) = transfer(&w.bot, &[(x_to, SOL)], Some(10_000), Some(10), None, h);
    let (t, _) = transfer(&w.bot, &[(t_to, SOL)], Some(10_000), None, Some(5_000), h);
    let start = w.balance(&w.bot.pubkey());
    send(w.guard.url(), &x, json!({ "encoding": "base64" })).await;
    let sent = jito(
        &w,
        "sendBundle",
        json!([[b64(&t), b64(&x)], { "encoding": "base64" }]),
    )
    .await;
    assert!(sent["result"].is_string(), "{sent}");
    w.slots(10).await;
    assert!(w.landed(&x_sig));
    assert_eq!(w.balance(&x_to), SOL, "it ran once");
    // Each landed transaction is charged once, and the ledger explains the
    // wallet.
    let moved = w.balance(&x_to) + w.balance(&t_to);
    let change = w.balance(&w.bot.pubkey()) as i64 - start as i64 + moved as i64;
    let report = w.engine.report(Some((change, 0)));
    let rows = report.blocks.iter().flat_map(|b| &b.landed).count() as u64;
    assert_eq!(report.fees.transactions, rows);
    assert!(rows <= 2, "{:?}", report.blocks);
    assert_eq!(report.fees.unexplained, Some(0), "{:?}", report.fees);
    // Sent again once it landed, in a new bundle: refused.
    let (u, _) = transfer(
        &w.bot,
        &[(Pubkey::new_unique(), SOL)],
        Some(10_000),
        None,
        Some(5_000),
        h,
    );
    let again = jito(
        &w,
        "sendBundle",
        json!([[b64(&u), b64(&x)], { "encoding": "base64" }]),
    )
    .await;
    assert!(
        again["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("already been processed")),
        "{again}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn flooding_the_pool_is_refused_past_its_limit() {
    let mut w = world(layer(false), None).await;
    w.slots(2).await;
    let h = w.blockhash();
    let mut refused = 0;
    for i in 0..70u64 {
        let (wire, _) = transfer(
            &w.bot,
            &[(Pubkey::new_from_array([i as u8 + 3; 32]), 1_000_000 + i)],
            Some(1_000),
            None,
            None,
            h,
        );
        let sent = send(w.guard.url(), &wire, json!({ "encoding": "base64" })).await;
        if sent["error"]["code"] == super::engine::TOO_MANY_REQUESTS {
            refused += 1;
        }
    }
    assert_eq!(refused, 70 - 64);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_tip_paid_elsewhere_buys_nothing() {
    let mut w = world(layer(false), None).await;
    w.slots(2).await;
    let start = w.balance(&w.bot.pubkey());
    // A bundle whose "tip" goes to an account that isn't Jito's.
    let elsewhere = Pubkey::new_unique();
    let (wire, signature) = transfer(
        &w.bot,
        &[(elsewhere, 50_000_000)],
        Some(10_000),
        None,
        None,
        w.blockhash(),
    );
    let id = jito(
        &w,
        "sendBundle",
        json!([[b64(&wire)], { "encoding": "base64" }]),
    )
    .await;
    w.slots(20).await;
    assert!(!w.landed(&signature));
    assert_eq!(w.balance(&elsewhere), 0);
    assert_eq!(w.balance(&w.bot.pubkey()), start);
    let inflight = jito(
        &w,
        "getInflightBundleStatuses",
        json!([[id["result"].clone()]]),
    )
    .await;
    assert_ne!(inflight["result"]["value"][0]["status"], "Landed");
}

#[tokio::test(flavor = "multi_thread")]
async fn sends_outside_the_turn_are_refused_and_pending_ones_cant_be_read() {
    // Under the floor, a cheap send stays pending.
    let mut w = world(layer(false), Some((0, 150, 200_000))).await;
    w.slots(2).await;
    let to = Pubkey::new_unique();
    let (pending, pending_sig) = transfer(
        &w.bot,
        &[(to, SOL)],
        Some(10_000),
        None,
        None,
        w.blockhash(),
    );
    send(w.guard.url(), &pending, json!({ "encoding": "base64" })).await;
    w.slots(4).await;
    assert_eq!(w.engine.fate(&pending_sig), Some(Fate::Pending));
    // Nothing the bot can ask shows it before it lands.
    let ask = |method: &str, params: Value| {
        post(
            w.guard.url(),
            json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }),
        )
    };
    let status = ask("getSignatureStatuses", json!([[pending_sig]])).await;
    assert!(status["result"]["value"][0].is_null(), "{status}");
    let found = ask(
        "getTransaction",
        json!([pending_sig, { "encoding": "json", "maxSupportedTransactionVersion": 0 }]),
    )
    .await;
    assert!(found["result"].is_null(), "{found}");
    assert_eq!(w.balance(&to), 0);
    // A bundle's simulation commits nothing.
    let y = Pubkey::new_unique();
    let (simulated, simulated_sig) =
        transfer(&w.bot, &[(y, SOL)], None, None, Some(5_000), w.blockhash());
    jito(
        &w,
        "simulateBundle",
        json!([{ "encodedTransactions": [b64(&simulated)] }, { "encoding": "base64" }]),
    )
    .await;
    w.slots(4).await;
    assert_eq!(w.balance(&y), 0);
    assert!(!w.landed(&simulated_sig));

    // Outside the turn, every way in is closed, and nothing is pooled.
    let sends = w.engine.report(None).sends.len();
    w.guard.set_turn(false);
    let (late, _) = transfer(
        &w.bot,
        &[(Pubkey::new_unique(), SOL)],
        None,
        None,
        Some(5_000),
        w.blockhash(),
    );
    let closed = [
        send(w.guard.url(), &late, json!({ "encoding": "base64" })).await,
        jito(&w, "sendBundle", json!([[b64(&late)], { "encoding": "base64" }])).await,
        post(
            &format!("{}/api/v1/transactions?bundleOnly=true", w.guard.url()),
            json!({ "jsonrpc": "2.0", "id": 1, "method": "sendTransaction", "params": [b64(&late), { "encoding": "base64" }] }),
        )
        .await,
    ];
    for reply in &closed {
        assert_eq!(reply["error"]["code"], crate::guard::TURN_CLOSED, "{reply}");
    }
    w.guard.set_turn(true);
    assert_eq!(w.engine.report(None).sends.len(), sends);
}

/// The layer's report with its ledger checked against the bot's wallet
/// since `start`, counting what it moved to `moved` as its own.
fn reconciled(w: &World, start: u64, moved: &[Pubkey]) -> crate::txlayer::report::TxLayerReport {
    let moved: u64 = moved.iter().map(|k| w.balance(k)).sum();
    let change = w.balance(&w.bot.pubkey()) as i64 - start as i64 + moved as i64;
    let report = w.engine.report(Some((change, 0)));
    let rows = report.blocks.iter().flat_map(|b| &b.landed).count() as u64;
    assert_eq!(report.fees.transactions, rows, "{:?}", report.blocks);
    assert_eq!(report.fees.unexplained, Some(0), "{:?}", report.fees);
    report
}

#[tokio::test(flavor = "multi_thread")]
async fn a_transaction_sent_many_ways_runs_and_is_charged_once() {
    let mut w = world(layer(false), None).await;
    w.slots(2).await;
    let start = w.balance(&w.bot.pubkey());
    let h = w.blockhash();
    let (x_to, t1_to, t2_to, y_to, z_to, f_to, u_to) = (
        Pubkey::new_unique(),
        Pubkey::new_unique(),
        Pubkey::new_unique(),
        Pubkey::new_unique(),
        Pubkey::new_unique(),
        Pubkey::new_unique(),
        Pubkey::new_unique(),
    );
    // One transaction in two bundles.
    let (x, x_sig) = transfer(&w.bot, &[(x_to, SOL)], Some(10_000), None, None, h);
    let (t1, _) = transfer(&w.bot, &[(t1_to, SOL)], Some(10_000), None, Some(5_000), h);
    let (t2, _) = transfer(&w.bot, &[(t2_to, SOL)], Some(10_000), None, Some(6_000), h);
    for t in [&t1, &t2] {
        let sent = jito(
            &w,
            "sendBundle",
            json!([[b64(t), b64(&x)], { "encoding": "base64" }]),
        )
        .await;
        assert!(sent["result"].is_string(), "{sent}");
    }
    // One transaction twice in a bundle: refused.
    let (y, _) = transfer(&w.bot, &[(y_to, SOL)], Some(10_000), None, Some(5_000), h);
    let twice = jito(
        &w,
        "sendBundle",
        json!([[b64(&y), b64(&y)], { "encoding": "base64" }]),
    )
    .await;
    assert!(
        twice["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("each transaction once")),
        "{twice}"
    );
    // A bundle of one, and the same transaction alone.
    let (z, z_sig) = transfer(
        &w.bot,
        &[(z_to, SOL)],
        Some(10_000),
        Some(10),
        Some(5_000),
        h,
    );
    jito(
        &w,
        "sendBundle",
        json!([[b64(&z)], { "encoding": "base64" }]),
    )
    .await;
    send(w.guard.url(), &z, json!({ "encoding": "base64" })).await;
    // A transfer it can't afford, in a bundle and alone; then again, alone
    // and in a new bundle, after it failed.
    let (f, f_sig) = transfer(
        &w.bot,
        &[(Pubkey::new_unique(), 1_000 * SOL)],
        Some(10_000),
        Some(10),
        None,
        h,
    );
    let (tf, _) = transfer(&w.bot, &[(f_to, SOL)], Some(10_000), None, Some(5_000), h);
    jito(
        &w,
        "sendBundle",
        json!([[b64(&tf), b64(&f)], { "encoding": "base64" }]),
    )
    .await;
    send(w.guard.url(), &f, json!({ "encoding": "base64" })).await;
    w.slots(10).await;
    assert!(w.landed(&f_sig), "{:?}", w.engine.fate(&f_sig));
    // The same bytes again are a rebroadcast of what landed: no new send.
    // In a new bundle, they are refused as processed.
    let sends = w.engine.report(None).sends.len();
    let again = send(w.guard.url(), &f, json!({ "encoding": "base64" })).await;
    assert_eq!(again["result"], f_sig, "{again}");
    assert_eq!(w.engine.report(None).sends.len(), sends);
    let (u, _) = transfer(&w.bot, &[(u_to, SOL)], Some(10_000), None, Some(5_000), h);
    let again = jito(
        &w,
        "sendBundle",
        json!([[b64(&u), b64(&f)], { "encoding": "base64" }]),
    )
    .await;
    assert!(
        again["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("already been processed")),
        "{again}"
    );
    w.slots(10).await;
    assert!(w.landed(&x_sig) && w.landed(&z_sig));
    assert_eq!(
        (w.balance(&x_to), w.balance(&z_to)),
        (SOL, SOL),
        "each ran once"
    );
    assert_eq!(w.balance(&y_to), 0);
    reconciled(&w, start, &[x_to, t1_to, t2_to, y_to, z_to, f_to, u_to]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_transaction_that_already_ran_is_never_sent_or_charged_again() {
    let mut w = world(layer(false), None).await;
    w.slots(2).await;
    let start = w.balance(&w.bot.pubkey());
    let h = w.blockhash();
    let (x_to, t_to) = (Pubkey::new_unique(), Pubkey::new_unique());
    let (x, _) = transfer(&w.bot, &[(x_to, SOL)], Some(10_000), Some(10), None, h);
    let (t, _) = transfer(&w.bot, &[(t_to, SOL)], Some(10_000), None, Some(5_000), h);
    send(w.guard.url(), &x, json!({ "encoding": "base64" })).await;
    jito(
        &w,
        "sendBundle",
        json!([[b64(&t), b64(&x)], { "encoding": "base64" }]),
    )
    .await;
    // Both picked for one block, whatever the builder would say: the
    // bundle runs x, then x alone comes after it.
    let next = w.slot + 1;
    w.engine.plan(next);
    let picks = vec![(1, true, vec![t, x.clone()]), (0, false, vec![x])];
    for signature in w.engine.commit(next, picks).await {
        w.guard.landed(&signature, 0);
    }
    w.slots(4).await;
    assert_eq!((w.balance(&x_to), w.balance(&t_to)), (SOL, SOL));
    let report = reconciled(&w, start, &[x_to, t_to]);
    let states: Vec<&str> = report.sends.iter().map(|s| s.state.as_str()).collect();
    assert_eq!(states, ["failed", "landed"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_send_signed_again_as_the_bots_own_copy_runs_once() {
    // A floor long enough that the first blockhash expires.
    let mut w = world(layer(false), Some((0, 100, 200_000))).await;
    w.slots(2).await;
    let start = w.balance(&w.bot.pubkey());
    let q = Pubkey::new_unique();
    let (x0, x0_sig) = transfer(
        &w.bot,
        &[(q, SOL / 100)],
        Some(10_000),
        None,
        None,
        w.blockhash(),
    );
    let retry = json!({ "encoding": "base64", "mtx": { "route": "rpc", "retryUntilSlot": w.slot + 400, "resign": true } });
    let sent = send(&w.guard.host_url(), &x0, retry).await;
    assert!(sent["result"].is_string(), "{sent}");
    // Near its expiry, the bot signs the same transfer itself on the latest
    // blockhash every slot: one of its copies is what the grader signs.
    let mut copies = Vec::new();
    for i in 0..230u64 {
        if i >= 135 && ["pending", "lost"].contains(&w.engine.report(None).sends[0].state.as_str())
        {
            let (copy, copy_sig) = transfer(
                &w.bot,
                &[(q, SOL / 100)],
                Some(10_000),
                None,
                None,
                w.blockhash(),
            );
            let sent = send(w.guard.url(), &copy, json!({ "encoding": "base64" })).await;
            assert!(sent["result"].is_string(), "{sent}");
            copies.push(copy_sig);
        }
        w.slot().await;
    }
    w.slots(10).await;
    let report = reconciled(&w, start, &[q]);
    assert_eq!(report.sends[0].state, "merged", "{:?}", report.sends[0]);
    let Some(Fate::Landed {
        signature: landed_as,
        ok: true,
        ..
    }) = w.engine.fate(&x0_sig)
    else {
        panic!("{:?}", w.engine.fate(&x0_sig));
    };
    assert!(copies.contains(&landed_as), "it landed as the bot's copy");
    assert!(!w.landed(&x0_sig));
    let ran = copies.iter().filter(|s| w.landed(s)).count() as u64;
    assert_eq!(w.balance(&q), ran * (SOL / 100), "no transfer ran twice");
}

#[tokio::test(flavor = "multi_thread")]
async fn what_a_congested_block_takes_is_charged_as_it_landed() {
    let mut w = world(layer(false), Some((0, 150, 200_000))).await;
    w.slots(2).await;
    let start = w.balance(&w.bot.pubkey());
    let h = w.blockhash();
    let mut moved = Vec::new();
    let mut to = || {
        let key = Pubkey::new_unique();
        moved.push(key);
        key
    };
    // Bundles of one with the least tip: 1,000 units, and fewer units than
    // the transfer uses.
    let (tiny, _) = transfer(
        &w.bot,
        &[(to(), SOL / 100)],
        Some(1_000),
        None,
        Some(jito::MIN_TIP),
        h,
    );
    let (under, _) = transfer(
        &w.bot,
        &[(to(), SOL / 100)],
        Some(100),
        None,
        Some(jito::MIN_TIP),
        h,
    );
    // Five 200,000-unit bundles with the least tip.
    let split: Vec<Vec<u8>> = (0..5)
        .map(|_| {
            transfer(
                &w.bot,
                &[(to(), SOL / 100)],
                Some(200_000),
                None,
                Some(jito::MIN_TIP),
                h,
            )
            .0
        })
        .collect();
    // A priority fee only on a transaction that writes nothing but its payer.
    let (a, _) = transfer(&w.bot, &[(to(), SOL / 100)], Some(200_000), None, None, h);
    let (rich, _) = transfer(
        &w.bot,
        &[],
        Some(1_000),
        Some(60_000_000),
        Some(jito::MIN_TIP),
        h,
    );
    let (a2, _) = transfer(&w.bot, &[(to(), SOL / 100)], Some(200_000), None, None, h);
    let (poor, _) = transfer(
        &w.bot,
        &[],
        Some(1_000),
        Some(20_000_000),
        Some(jito::MIN_TIP),
        h,
    );
    let (plain, _) = transfer(
        &w.bot,
        &[(to(), SOL / 100)],
        Some(1_000),
        Some(250_000),
        None,
        h,
    );
    let bundles = [vec![tiny], vec![under], vec![a, rich], vec![a2, poor]]
        .into_iter()
        .chain(split.into_iter().map(|wire| vec![wire]));
    for wires in bundles {
        let encoded: Vec<String> = wires.iter().map(|w| b64(w)).collect();
        jito(&w, "sendBundle", json!([encoded, { "encoding": "base64" }])).await;
    }
    send(w.guard.url(), &plain, json!({ "encoding": "base64" })).await;
    w.slots(20).await;
    reconciled(&w, start, &moved);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_send_the_chain_never_ran_is_neither_landed_nor_paid() {
    let mut w = world(layer(false), None).await;
    w.slots(2).await;
    let me = w.bot.pubkey();
    let (q, drain) = (Pubkey::new_unique(), Pubkey::new_unique());
    // Two compute-unit prices: the node takes it without preflight, and
    // the runtime refuses it before it runs.
    let (twice, twice_sig) = custom(
        &w.bot,
        &[
            cu_limit(10_000),
            cu_price(1),
            cu_price(10_000_000),
            pay(&me, q, SOL / 100),
        ],
        w.blockhash(),
    );
    send(w.guard.url(), &twice, json!({ "encoding": "base64" })).await;
    w.slots(4).await;
    assert!(!w.landed(&twice_sig));
    assert_eq!(
        w.engine.fate(&twice_sig),
        Some(Fate::Failed("the chain didn't process it".into()))
    );
    // A fee the wallet can't pay: it keeps 1,000,000 lamports (above its
    // rent), and the send costs 2,005,000.
    let start = w.balance(&me);
    let left = 1_000_000;
    let (emptied, _) = custom(
        &w.bot,
        &[pay(&me, drain, start - 5_000 - left)],
        w.blockhash(),
    );
    send(w.guard.url(), &emptied, json!({ "encoding": "base64" })).await;
    w.slots(4).await;
    assert_eq!(w.balance(&me), left);
    let (dear, dear_sig) = custom(
        &w.bot,
        &[cu_limit(10_000), cu_price(200_000_000), pay(&me, q, 1_000)],
        w.blockhash(),
    );
    send(w.guard.url(), &dear, json!({ "encoding": "base64" })).await;
    w.slots(4).await;
    assert!(!w.landed(&dear_sig));
    assert!(!matches!(
        w.engine.fate(&dear_sig),
        Some(Fate::Landed { .. })
    ));
    assert_eq!(w.balance(&me), left);
    let report = w.engine.report(Some((
        w.balance(&me) as i64 - start as i64 + w.balance(&drain) as i64,
        0,
    )));
    assert_eq!(report.fees.failed_tx_fees, 0, "{:?}", report.fees);
    assert_eq!(report.fees.unexplained, Some(0), "{:?}", report.fees);
    let states: Vec<&str> = report.sends.iter().map(|s| s.state.as_str()).collect();
    assert_eq!(states[0], "failed");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bundle_that_failed_without_running_may_go_again() {
    let mut w = world(layer(false), None).await;
    w.slots(2).await;
    let start = w.balance(&w.bot.pubkey());
    let me = w.bot.pubkey();
    let q = Pubkey::new_unique();
    let tip_to: Pubkey = jito::TIP_ACCOUNTS[0].parse().unwrap();
    let h = w.blockhash();
    // One lamport to a new account is below its rent: the bundle fails.
    let (bundle, bundle_sig) = custom(&w.bot, &[pay(&me, q, 1), pay(&me, tip_to, 10_000)], h);
    let sent = jito(
        &w,
        "sendBundle",
        json!([[b64(&bundle)], { "encoding": "base64" }]),
    )
    .await;
    let id = sent["result"].clone();
    w.slots(6).await;
    let status = jito(&w, "getInflightBundleStatuses", json!([[id.clone()]])).await;
    assert_eq!(status["result"]["value"][0]["status"], "Failed", "{status}");
    // Once q holds its rent, the same bytes land.
    let (fund, _) = custom(&w.bot, &[pay(&me, q, SOL / 100)], h);
    send(w.guard.url(), &fund, json!({ "encoding": "base64" })).await;
    w.slots(4).await;
    let again = jito(
        &w,
        "sendBundle",
        json!([[b64(&bundle)], { "encoding": "base64" }]),
    )
    .await;
    assert_eq!(again["result"], id, "{again}");
    w.slots(6).await;
    assert!(w.landed(&bundle_sig));
    assert_eq!(w.balance(&q), SOL / 100 + 1);
    let status = jito(&w, "getInflightBundleStatuses", json!([[id]])).await;
    assert_eq!(status["result"]["value"][0]["status"], "Landed", "{status}");
    // Once it ran, never again.
    let late = jito(
        &w,
        "sendBundle",
        json!([[b64(&bundle)], { "encoding": "base64" }]),
    )
    .await;
    assert_eq!(late["result"], status["result"]["value"][0]["bundle_id"]);
    w.slots(4).await;
    assert_eq!(w.balance(&q), SOL / 100 + 1);
    reconciled(&w, start, &[q]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_transaction_alone_and_as_a_bundle_of_one_race() {
    let mut w = world(layer(false), None).await;
    w.slots(2).await;
    let start = w.balance(&w.bot.pubkey());
    let me = w.bot.pubkey();
    let (q, r) = (Pubkey::new_unique(), Pubkey::new_unique());
    let tip_to: Pubkey = jito::TIP_ACCOUNTS[0].parse().unwrap();
    let h = w.blockhash();
    let (x, x_sig) = custom(
        &w.bot,
        &[pay(&me, q, SOL / 100), pay(&me, tip_to, 5_000)],
        h,
    );
    let (y, y_sig) = custom(
        &w.bot,
        &[pay(&me, r, SOL / 100), pay(&me, tip_to, 5_000)],
        h,
    );
    send(w.guard.url(), &x, json!({ "encoding": "base64" })).await;
    let x_bundle = jito(
        &w,
        "sendBundle",
        json!([[b64(&x)], { "encoding": "base64" }]),
    )
    .await;
    let y_bundle = jito(
        &w,
        "sendBundle",
        json!([[b64(&y)], { "encoding": "base64" }]),
    )
    .await;
    send(w.guard.url(), &y, json!({ "encoding": "base64" })).await;
    w.slots(10).await;
    assert!(w.landed(&x_sig) && w.landed(&y_sig));
    assert_eq!((w.balance(&q), w.balance(&r)), (SOL / 100, SOL / 100));
    for id in [&x_bundle["result"], &y_bundle["result"]] {
        let status = jito(&w, "getInflightBundleStatuses", json!([[id]])).await;
        let status = &status["result"]["value"][0]["status"];
        assert!(status == "Landed" || status == "Failed", "{status}");
    }
    let report = w.engine.report(None);
    assert_eq!(report.sends.len(), 4);
    reconciled(&w, start, &[q, r]);
}

#[tokio::test(flavor = "multi_thread")]
async fn fees_another_key_paid_stay_out_of_the_wallets_ledger() {
    let mut w = world(layer(false), None).await;
    w.slots(2).await;
    let start = w.balance(&w.bot.pubkey());
    let me = w.bot.pubkey();
    let other = Keypair::new();
    let (fund, _) = custom(&w.bot, &[pay(&me, other.pubkey(), SOL / 10)], w.blockhash());
    send(w.guard.url(), &fund, json!({ "encoding": "base64" })).await;
    w.slots(4).await;
    let q = Pubkey::new_unique();
    let (paid, paid_sig) = custom(
        &other,
        &[
            cu_limit(10_000),
            cu_price(1_000_000),
            pay(&other.pubkey(), q, SOL / 100),
        ],
        w.blockhash(),
    );
    send(w.guard.url(), &paid, json!({ "encoding": "base64" })).await;
    w.slots(6).await;
    assert!(w.landed(&paid_sig));
    // The wallet sent the other key SOL / 10 and paid one base fee.
    let change = w.balance(&me) as i64 - start as i64 + (SOL / 10) as i64;
    let report = w.engine.report(Some((change, 0)));
    assert_eq!(
        report.fees.paid_by_others,
        5_000 + 10_000,
        "{:?}",
        report.fees
    );
    assert_eq!(report.fees.unexplained, Some(0), "{:?}", report.fees);
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
            slot: None,
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
    // A line and a raw send in one bundle went in the step they were sent,
    // and landed in one slot.
    let landed_at = |source: &str| {
        let landed = format!("step 8 {source}: landed, slot ");
        log.lines().find_map(|l| {
            l.split_once(&landed)
                .map(|(_, rest)| rest.split(',').next().unwrap_or_default().to_string())
        })
    };
    assert!(
        landed_at("line 0").is_some() && landed_at("line 0") == landed_at("raw 0"),
        "{log}"
    );
    let tx = report.tx_layer.as_ref().unwrap();
    // The last step's bundled raw send went into the pool too.
    let last = scenario.duration_slots.div_ceil(2) - 1;
    assert!(
        tx.sends.iter().any(|s| s.step == last && s.bundle),
        "{:?}",
        tx.sends
    );
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
