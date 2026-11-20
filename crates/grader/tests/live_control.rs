//! Step 4 of `docs/rfd/0004`: a live runtime, loaded as the geyser plugin
//! into a surfnet whose clock runs in real time, runs a strategy on its
//! own and is controlled while it runs: add, pause, a new mandate, resume,
//! a restart of the plugin that brings the same strategies back, the kill
//! switch, and remove.
//!
//! `MTX_TEST_GEYSER_PLUGIN` is the built plugin (`cargo build -p
//! mtx-geyser`), and the flash-crash reference is built (`just geyser-parity`
//! does both).

use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use mtx_core::{
    Mandate,
    control::{self, AddStrategy, Command, OnPause, Reply, StrategyState, StrategyStatus},
};
use mtx_grader::{
    chain::{self, Override},
    phoenix,
    snapshot::{self, Access},
};
use serde_json::{Value, json};
use solana_keypair::Keypair;
use solana_rpc_client::rpc_client::RpcClient;
use solana_rpc_client_api::request::RpcRequest;
use solana_signer::Signer;

/// How long the market is kept moving: longer than the test.
const LIVE_SLOTS: u64 = 3_000;
/// Slots between two moves: inside the oracle's 50-slot staleness.
const MOVE_EVERY_SLOTS: u64 = 5;
/// How long to wait for the runtime to show a change.
const WITHIN: Duration = Duration::from_secs(60);
/// How long a paused strategy is watched for steps it shouldn't take.
const PAUSED_FOR: Duration = Duration::from_secs(4);

const SYMBOL: &str = "SPCX";

fn ask(socket: &Path, command: Command) -> Vec<StrategyStatus> {
    match control::request(socket, &command).expect("the runtime answers") {
        Reply::Ok { strategies } => strategies,
        Reply::Error { error } => panic!("{command:?} refused: {error}"),
    }
}

fn one<'a>(strategies: &'a [StrategyStatus], name: &str) -> &'a StrategyStatus {
    strategies
        .iter()
        .find(|s| s.name == name)
        .unwrap_or_else(|| panic!("no {name} in {strategies:?}"))
}

/// Ask for the status until `done` holds of it.
async fn until(socket: &Path, what: &str, done: impl Fn(&[StrategyStatus]) -> bool) {
    let deadline = Instant::now() + WITHIN;
    loop {
        let strategies = ask(socket, Command::Status);
        if done(&strategies) {
            return;
        }
        assert!(Instant::now() < deadline, "{what}: {strategies:?}");
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

fn load(rpc: &RpcClient, config: &Path) -> String {
    let loaded: Value = rpc
        .send(
            RpcRequest::Custom {
                method: "loadPlugin",
            },
            json!([config]),
        )
        .expect("the surfnet loads the plugin");
    loaded.as_str().expect("the plugin's name").to_string()
}

fn unload(rpc: &RpcClient, name: &str) {
    let _: Value = rpc
        .send(
            RpcRequest::Custom {
                method: "unloadPlugin",
            },
            json!([name]),
        )
        .expect("the surfnet unloads the plugin");
}

fn mandate(cap: f64) -> Mandate {
    Mandate::perp("phoenix", SYMBOL, cap, 1_000.0)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the built plugin and components"]
async fn a_live_runtime_is_controlled_while_it_runs() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let plugin = PathBuf::from(
        std::env::var("MTX_TEST_GEYSER_PLUGIN").expect("MTX_TEST_GEYSER_PLUGIN: the built plugin"),
    );
    let component =
        mtx_grader::wasm::component_in(&root.join("bench/tasks/flash-crash-spcx/reference"))
            .expect("the built reference");
    let dir = std::env::temp_dir().join(format!("mtx-live-control-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    // A surfnet whose clock runs, and a market kept moving on it.
    let world = snapshot::start_offline(
        &root.join("snapshots/phoenix-spcx-2026-10-29"),
        Access::Open,
    )
    .await
    .unwrap();
    let rpc = chain::rpc_client(&world.surfnet);
    let kit = mtx_kit::Phoenix::new(world.exchange.clone(), world.surfnet.rpc_url());
    let key = Keypair::new();
    phoenix::onboard_trader(&world.surfnet, &kit, &key, 10_000.0)
        .await
        .unwrap();
    let mark = kit
        .hawkeye(key.pubkey())
        .view_bbo(SYMBOL)
        .await
        .unwrap()
        .value
        .mark_price_ticks;
    let asset_map: solana_pubkey::Pubkey = world.exchange.keys.perp_asset_map.parse().unwrap();
    let moves: Vec<Override> = (1..LIVE_SLOTS / MOVE_EVERY_SLOTS)
        .flat_map(|n| {
            // A tick either way and back: the price stays, the reports
            // stay fresh.
            let ticks = mark + n % 3 - 1;
            let (mut overrides, _) =
                phoenix::market_move(asset_map, SYMBOL, mark, ticks, n * MOVE_EVERY_SLOTS);
            overrides.truncate(2);
            overrides
        })
        .collect();
    chain::register_scenario(&rpc, "keep SPCX moving", moves).unwrap();

    // The plugin, with a control socket and no strategy yet.
    let keypair = dir.join("strategy.json");
    std::fs::write(
        &keypair,
        serde_json::to_string(&key.to_bytes().to_vec()).unwrap(),
    )
    .unwrap();
    let socket = dir.join("control.sock");
    let config = dir.join("mtx-geyser.json");
    std::fs::write(
        &config,
        json!({
            "libpath": plugin,
            "mtx": {
                "journal": dir.join("journal.jsonl"),
                "markets": [],
                "control": {
                    "socket": socket,
                    "state": dir.join("strategies.json"),
                    "rpc_url": world.surfnet.rpc_url(),
                    "logs": dir.join("logs"),
                },
            },
        })
        .to_string(),
    )
    .unwrap();
    let name = load(&rpc, &config);
    let add = |name: &str, run: bool| {
        Command::Add(AddStrategy {
            name: name.into(),
            component: component.clone(),
            mandate: mandate(5.0),
            keypair: keypair.clone(),
            exchange: world.exchange_path.clone(),
            symbol: SYMBOL.into(),
            slots_per_step: 2,
            run,
        })
    };

    // Added running, it steps on the slots.
    ask(&socket, add("a", true));
    until(&socket, "a steps", |s| one(s, "a").steps >= 3).await;

    // Paused, it doesn't.
    let paused = ask(
        &socket,
        Command::Pause {
            name: Some("a".into()),
            orders: OnPause::Cancel,
        },
    );
    let steps = one(&paused, "a").steps;
    tokio::time::sleep(PAUSED_FOR).await;
    let still = ask(&socket, Command::Status);
    assert_eq!(
        one(&still, "a").steps,
        steps,
        "a paused strategy took a step"
    );
    assert!(matches!(
        one(&still, "a").state,
        StrategyState::Paused { .. }
    ));

    // A new mandate restarts it under the new cap, paused as it was.
    let changed = ask(
        &socket,
        Command::Mandate {
            name: "a".into(),
            mandate: mandate(0.5),
        },
    );
    assert_eq!(one(&changed, "a").cap, 0.5);
    assert!(matches!(
        one(&changed, "a").state,
        StrategyState::Paused { .. }
    ));

    // Resumed, it steps again.
    ask(
        &socket,
        Command::Resume {
            name: Some("a".into()),
        },
    );
    until(&socket, "a steps again", |s| one(s, "a").steps > steps).await;

    // Added paused, it waits.
    let added = ask(&socket, add("b", false));
    assert!(matches!(
        one(&added, "b").state,
        StrategyState::Paused { .. }
    ));

    // A restart of the plugin brings both back as they were.
    unload(&rpc, &name);
    let name = load(&rpc, &config);
    until(&socket, "the same strategies back", |s| {
        s.len() == 2
            && one(s, "a").state == StrategyState::Running
            && one(s, "a").cap == 0.5
            && matches!(one(s, "b").state, StrategyState::Paused { .. })
    })
    .await;
    until(&socket, "a steps after the restart", |s| {
        one(s, "a").steps >= 1
    })
    .await;

    // The kill switch pauses everything.
    let killed = ask(&socket, Command::Kill);
    for strategy in &killed {
        assert_eq!(
            strategy.state,
            StrategyState::Paused {
                why: "the kill switch".into()
            }
        );
    }

    // Removed, it's gone, and stays gone after a restart.
    let removed = ask(&socket, Command::Remove { name: "b".into() });
    assert_eq!(removed.len(), 1);
    unload(&rpc, &name);
    let name = load(&rpc, &config);
    until(&socket, "only a back", |s| s.len() == 1 && s[0].name == "a").await;
    unload(&rpc, &name);
    std::fs::remove_dir_all(&dir).unwrap();
}
