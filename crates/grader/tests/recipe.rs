//! Snapshot recipes that make simulated markets, built offline from the
//! committed spot snapshot (it holds the Phoenix v1 and OpenBook v2 programs).

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use mtx_core::Decimal;
use mtx_grader::{
    recipe::{self, BuildOptions},
    snapshot::{self, Access, Manifest},
};
use mtx_kit::{
    chain::{ChainClient, RpcChainClient},
    protocols::{openbook::v2::OpenBookV2Trader, phoenix::v1::PhoenixV1Trader},
};

const RECIPE: &str = r#"
name = "sim-test"
kind = "spot"
symbol = "FAKE-USDC"

[fork]
from_snapshot = "SOURCE"

[[program]]
name = "phoenix-v1"
address = "PhoeNiXZ8ByJGLkxNfZRnkUfjvmuYqLR89jjFHGqdXY"
pin_from = "SOURCE"

[[account]]
name = "USDC"
kind = "mint"
address = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"

[[warmup]]
action = "create-mint"
name = "FAKE"
decimals = 9

[[warmup]]
action = "create-mint"
name = "FAKE22"
decimals = 6
program = "token-2022"

[[warmup]]
action = "create-mint"
name = "FAKE2"
decimals = 6

[[warmup]]
action = "phoenix-v1-market"
name = "FAKE/USDC"
base = "FAKE"
quote = "USDC"
base_lot_atoms = 1000000
quote_lot_atoms = 1
tick_quote_atoms_per_base_unit = 1000
price = { calibration = "calibration.toml", key = "fair_value.price" }
ladder = { levels = 4, half_spread_bps = 20, step_bps = 20, level_size = "50" }

[[warmup]]
action = "openbook-v2-market"
name = "FAKE2/USDC"
base = "FAKE2"
quote = "USDC"
base_lot_atoms = 1000
quote_lot_atoms = 1
price = { value = 3.25 }
ladder = { levels = 3, level_size = "100" }
"#;

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn setup(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "mtx-recipe-{tag}-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let source = root().join("snapshots/sol-usdc-spot-2026-11-11");
    std::fs::write(
        dir.join("recipe.toml"),
        RECIPE.replace("SOURCE", source.to_str().unwrap()),
    )
    .unwrap();
    std::fs::write(dir.join("calibration.toml"), "[fair_value]\nprice = 41.5\n").unwrap();
    dir
}

fn options(dir: &Path, out: &str) -> BuildOptions {
    BuildOptions {
        out_dir: Some(dir.join(out)),
        fork_rpc_url: String::new(),
        force: false,
        secret: Some("test-secret".into()),
    }
}

async fn build(dir: &Path, out: &str) -> Manifest {
    recipe::build(&dir.join("recipe.toml"), options(dir, out))
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn simulated_markets_are_made_seeded_and_flagged() {
    let dir = setup("made");
    let manifest = build(&dir, "a").await;

    assert!(manifest.simulated);
    assert_eq!(manifest.recipe_sha256.as_ref().map(String::len), Some(64));
    assert_eq!(manifest.sources.len(), 2);
    let program = &manifest.sources[0];
    assert_eq!(program.name, "phoenix-v1");
    assert_eq!(program.code_sha256.as_ref().map(String::len), Some(64));
    assert!(program.deploy_slot.is_some() && program.programdata.is_some());
    assert!(program.pinned_from.as_deref() == Some("sol-usdc-spot-2026-11-11"));
    let kinds: Vec<_> = manifest.created.iter().map(|c| c.kind.as_str()).collect();
    assert_eq!(
        kinds,
        [
            "mint",
            "mint",
            "mint",
            "phoenix-v1-market",
            "openbook-v2-market"
        ]
    );
    assert!(manifest.created.iter().all(|c| c.simulated));

    let (surfnet, _) = snapshot::start_snapshot(&dir.join("a"), Access::Open)
        .await
        .unwrap();
    let chain: Arc<dyn ChainClient> = Arc::new(RpcChainClient::new(surfnet.rpc_url()));
    let owner = mtx_kit::Pubkey::new_unique();

    let address = |name: &str| {
        manifest
            .created
            .iter()
            .find(|c| c.name == name)
            .unwrap()
            .address
            .parse()
            .unwrap()
    };
    let phoenix =
        PhoenixV1Trader::connect(chain.clone(), address("FAKE/USDC"), "FAKE", "USDC", owner)
            .await
            .unwrap();
    let book = phoenix.order_book().await.unwrap();
    assert_eq!((book.bids.len(), book.asks.len()), (4, 4));
    assert!(book.bids[0].price < Decimal::new(415, 1) && book.asks[0].price > Decimal::new(415, 1));
    assert!(
        book.asks[0].price < Decimal::new(42, 0),
        "within 1.2% of 41.5"
    );

    // A fresh bot gets a seat from the Seat Manager and rests an order, with
    // the kit's code and nothing special.
    let detail = &manifest
        .created
        .iter()
        .find(|c| c.name == "FAKE/USDC")
        .unwrap()
        .detail;
    let mint = |key: &str| -> mtx_kit::Pubkey { detail[key].as_str().unwrap().parse().unwrap() };
    let bot = solana_keypair::Keypair::new();
    let bot_key = solana_signer::Signer::pubkey(&bot);
    let cheats = surfnet.cheatcodes();
    cheats.fund_sol(&bot_key, 10_000_000_000).unwrap();
    cheats
        .fund_token(&bot_key, &mint("base_mint"), 1_000_000_000_000, None)
        .unwrap();
    cheats
        .fund_token(&bot_key, &mint("quote_mint"), 1_000_000_000, None)
        .unwrap();
    let chain2: Arc<dyn ChainClient> = Arc::new(RpcChainClient::new(surfnet.rpc_url()));
    let theirs = PhoenixV1Trader::connect(chain2, address("FAKE/USDC"), "FAKE", "USDC", bot_key)
        .await
        .unwrap();
    let seat = theirs.plan_claim_seat().unwrap();
    theirs
        .send(&seat, &bot)
        .await
        .expect("the seat manager seats a fresh bot");
    let tick = theirs.market().instrument.tick_size;
    theirs
        .submit(
            &mtx_kit::protocols::phoenix::v1::PhoenixV1Order::PostOnly {
                side: mtx_core::Side::Buy,
                price: Decimal::new(30, 0) + tick,
                quantity: Decimal::new(1, 0),
                amend_if_crossing: false,
                last_valid_slot: None,
            },
            &bot,
        )
        .await
        .expect("a resting order from a bot with a claimed seat");
    assert_eq!(theirs.order_book().await.unwrap().bids.len(), 5);

    let openbook = OpenBookV2Trader::connect(chain, address("FAKE2/USDC"), "FAKE2", "USDC", owner)
        .await
        .unwrap();
    let book = openbook.order_book(0).await.unwrap();
    assert_eq!((book.bids.len(), book.asks.len()), (3, 3));
    assert!(book.bids[0].price < Decimal::new(325, 2) && book.asks[0].price > Decimal::new(325, 2));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rebuild_is_byte_identical() {
    let dir = setup("twice");
    build(&dir, "a").await;
    build(&dir, "b").await;
    assert_eq!(
        std::fs::read(dir.join("a/manifest.json")).unwrap(),
        std::fs::read(dir.join("b/manifest.json")).unwrap(),
        "manifest"
    );
    let load = |name: &str| -> serde_json::Map<String, serde_json::Value> {
        let file = std::fs::File::open(dir.join(name).join("surfnet.json.gz")).unwrap();
        serde_json::from_reader(flate2::read::GzDecoder::new(file)).unwrap()
    };
    let (a, b) = (load("a"), load("b"));
    let differing: Vec<_> = a
        .iter()
        .filter(|(key, value)| b.get(*key) != Some(value))
        .map(|(key, _)| key.clone())
        .collect();
    let only_b: Vec<_> = b.keys().filter(|k| !a.contains_key(*k)).collect();
    let brief = |m: &serde_json::Map<String, serde_json::Value>, k: &str| {
        m.get(k).map(|v| {
            format!(
                "{k} owner {} lamports {} data {}",
                v["owner"],
                v["lamports"],
                v["data"].as_str().map_or(0, str::len)
            )
        })
    };
    let only_a: Vec<_> = a
        .keys()
        .filter(|k| !b.contains_key(*k))
        .map(|k| brief(&a, k))
        .collect();
    let only_b: Vec<_> = only_b.iter().map(|k| brief(&b, k)).collect();
    assert!(
        only_a.is_empty() && only_b.is_empty(),
        "only in a: {only_a:?}; only in b: {only_b:?}"
    );
    let describe = |key: &String| {
        let (x, y) = (&a[key], &b.get(key).cloned().unwrap_or_default());
        let fields: Vec<_> = x
            .as_object()
            .unwrap()
            .keys()
            .filter(|f| y.get(f.as_str()) != Some(&x[f.as_str()]))
            .cloned()
            .collect();
        let bytes = |v: &serde_json::Value| {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD
                .decode(v["data"].as_str().unwrap_or_default())
                .unwrap_or_default()
        };
        let (p, q) = (bytes(x), bytes(y));
        let at: Vec<_> = (0..p.len().min(q.len()))
            .filter(|i| p[*i] != q[*i])
            .take(6)
            .map(|i| format!("{i}: {} vs {}", p[i], q[i]))
            .collect();
        format!(
            "{key} owner {} len {} {fields:?} bytes {at:?}",
            x["owner"],
            p.len()
        )
    };
    let report: Vec<_> = differing.iter().map(describe).collect();
    assert!(differing.is_empty(), "accounts differ: {report:?}");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_declared_account_the_warmup_never_loads_fails_the_build() {
    let dir = setup("missing");
    let text = std::fs::read_to_string(dir.join("recipe.toml")).unwrap().replace(
        "[[account]]",
        "[[account]]\nname = \"somewhere\"\naddress = \"7UVimffxr9ow1uXYxsr4LHAcV58mLzhmwaeKvJ1pjLiE\"\n\n[[account]]",
    );
    std::fs::write(dir.join("recipe.toml"), text).unwrap();
    let error = recipe::build(&dir.join("recipe.toml"), options(&dir, "a")).await;
    // The Pyth feed is in the source snapshot, so it exports; a sysvar,
    // which exports leave out, does not.
    assert!(error.is_ok());
    let text = std::fs::read_to_string(dir.join("recipe.toml"))
        .unwrap()
        .replace(
            "7UVimffxr9ow1uXYxsr4LHAcV58mLzhmwaeKvJ1pjLiE",
            "SysvarC1ock11111111111111111111111111111111",
        );
    std::fs::write(dir.join("recipe.toml"), text).unwrap();
    let error = recipe::build(&dir.join("recipe.toml"), options(&dir, "b"))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("never loaded"), "{error}");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn token_2022_mints_exist_but_the_order_books_refuse_them() {
    let dir = setup("t22");
    let text = std::fs::read_to_string(dir.join("recipe.toml"))
        .unwrap()
        .replace(
            "name = \"FAKE2/USDC\"\nbase = \"FAKE2\"",
            "name = \"FAKE2/USDC\"\nbase = \"FAKE22\"",
        );
    std::fs::write(dir.join("recipe.toml"), text).unwrap();
    let error = recipe::build(&dir.join("recipe.toml"), options(&dir, "a"))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("Token-2022"), "{error}");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_program_hash_is_checked_and_a_snapshot_is_not_overwritten() {
    let dir = setup("pin");
    let manifest = build(&dir, "a").await;
    let hash = manifest.sources[0].code_sha256.clone().unwrap();
    let text = std::fs::read_to_string(dir.join("recipe.toml")).unwrap();

    let with = |expected: &str| {
        text.replace(
            "name = \"phoenix-v1\"\n",
            &format!("name = \"phoenix-v1\"\nexpect_code_sha256 = \"{expected}\"\n"),
        )
    };
    std::fs::write(dir.join("recipe.toml"), with(&hash)).unwrap();
    build(&dir, "b").await;
    std::fs::write(dir.join("recipe.toml"), with(&"0".repeat(64))).unwrap();
    let error = recipe::build(&dir.join("recipe.toml"), options(&dir, "c"))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("not the expected"), "{error}");

    let error = recipe::build(&dir.join("recipe.toml"), options(&dir, "a"))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("already exists"), "{error}");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn the_keys_of_a_simulated_market_depend_on_a_secret_nobody_records() {
    let dir = setup("secret");
    let one = build(&dir, "a").await;
    let mut other = options(&dir, "b");
    other.secret = Some("another-secret".into());
    let two = recipe::build(&dir.join("recipe.toml"), other)
        .await
        .unwrap();
    let market = |m: &Manifest| {
        m.created
            .iter()
            .find(|c| c.kind == "phoenix-v1-market")
            .unwrap()
            .address
            .clone()
    };
    assert_ne!(market(&one), market(&two));
    let recorded = std::fs::read_to_string(dir.join("a/manifest.json")).unwrap();
    assert!(!recorded.contains("test-secret"));
    let mint = |m: &Manifest| {
        m.created
            .iter()
            .find(|c| c.name == "FAKE")
            .unwrap()
            .address
            .clone()
    };
    let (surfnet, _) = snapshot::start_snapshot(&dir.join("a"), Access::Open)
        .await
        .unwrap();
    let account = surfnet
        .rpc_client()
        .get_account(&mint(&one).parse().unwrap())
        .unwrap();
    assert_eq!(&account.data[..4], &[0; 4], "no mint authority");
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Reads a snapshot's accounts: address to the account's JSON.
fn accounts_of(dir: &Path) -> serde_json::Map<String, serde_json::Value> {
    let file = std::fs::File::open(dir.join("surfnet.json.gz")).unwrap();
    serde_json::from_reader(flate2::read::GzDecoder::new(file)).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn pyth_accounts_are_layered_onto_the_perps_snapshot_without_touching_it() {
    use base64::Engine;
    use mtx_kit::protocols::pyth::PriceUpdate;

    let recipe_path = root().join("snapshots/phoenix-perps-oracles-2026-11-13/recipe.toml");
    let base = root().join("snapshots/phoenix-perps-2026-11-13");
    let tmp = std::env::temp_dir().join(format!("mtx-pyth-{}", uuid::Uuid::new_v4().simple()));
    let build = |out: &str| {
        let opts = BuildOptions {
            out_dir: Some(tmp.join(out)),
            fork_rpc_url: String::new(),
            force: false,
            secret: Some("test-secret".into()),
        };
        recipe::build(&recipe_path, opts)
    };
    let manifest = build("a").await.unwrap();
    build("b").await.unwrap();

    let oracles: Vec<_> = manifest
        .sources
        .iter()
        .filter(|s| s.role == "oracle")
        .collect();
    assert_eq!(oracles.len(), 9);

    let mut a = accounts_of(&tmp.join("a"));
    let before = accounts_of(&base);
    for (address, account) in &before {
        assert_eq!(a.get(address), Some(account), "{address} changed");
    }
    for source in &oracles {
        let account = a.remove(&source.address).expect("the feed's account");
        assert_eq!(
            account["owner"],
            "rec5EKMGg6MxZYaMdyBfgwp4d5rB9T1VQH5pJv5LtFJ"
        );
        let data = base64::engine::general_purpose::STANDARD
            .decode(account["data"].as_str().unwrap())
            .unwrap();
        let update = PriceUpdate::decode(&data).unwrap();
        assert!(update.price().unwrap() > Decimal::ZERO, "{}", source.name);
    }
    // Nothing but the nine accounts was added.
    let extra: Vec<_> = a.keys().filter(|k| !before.contains_key(*k)).collect();
    assert!(extra.is_empty(), "unexpected accounts {extra:?}");

    let source: Manifest =
        serde_json::from_str(&std::fs::read_to_string(base.join("manifest.json")).unwrap())
            .unwrap();
    assert_eq!(manifest.mark_price_usd, source.mark_price_usd);
    assert_eq!(manifest.marks, source.marks);
    assert_eq!(manifest.fork_slot, source.fork_slot);

    for file in ["surfnet.json.gz", "manifest.json", "exchange.json"] {
        assert_eq!(
            std::fs::read(tmp.join("a").join(file)).unwrap(),
            std::fs::read(tmp.join("b").join(file)).unwrap(),
            "{file} differs between two builds"
        );
    }
    std::fs::remove_dir_all(&tmp).ok();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_layered_pyth_feed_is_driven_stale_and_wide_on_an_nvda_session_world() {
    use mtx_grader::{chain, oracles};
    use mtx_kit::protocols::pyth::{PriceUpdate, feed_account};

    const NVDAX: &str = "4244d07890e4610f46bbde67de8f43a4bf8b569eebe904f136b469f148503b7f";
    let recipe_path = root().join("snapshots/phoenix-perps-oracles-2026-11-13/recipe.toml");
    let tmp = std::env::temp_dir().join(format!("mtx-pyth-{}", uuid::Uuid::new_v4().simple()));
    let opts = BuildOptions {
        out_dir: Some(tmp.clone()),
        fork_rpc_url: String::new(),
        force: false,
        secret: Some("test-secret".into()),
    };
    recipe::build(&recipe_path, opts).await.unwrap();

    let (surfnet, _) = snapshot::start_snapshot_stepped(&tmp).await.unwrap();
    let rpc = chain::rpc_client(&surfnet);
    let mut id = [0u8; 32];
    for (byte, pair) in id.iter_mut().zip(NVDAX.as_bytes().chunks(2)) {
        *byte = u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap();
    }
    let address = feed_account(0, &id);
    let read = || {
        let data = rpc.get_account(&address).unwrap().data;
        PriceUpdate::decode_feed(&data, &id).unwrap()
    };
    let start = read();
    assert!(start.price_f64() > 1.0, "a plausible NVDAX price");

    let oracles: oracles::Oracles = serde_json::from_value(serde_json::json!({
        "seed": 1,
        "feeds": [{
            "name": "nvdax",
            "source": { "kind": "pyth", "feed_id": NVDAX },
            "cadence_slots": 4,
            "conf_bps": 2.0,
            "path": [{ "slot": 40, "level_pct": -10.0 }],
            "failures": [
                { "mode": { "kind": "stale" }, "window": { "from_slot": 12, "slots": 12 } },
                { "mode": { "kind": "wide_confidence", "factor": 10.0 },
                  "window": { "from_slot": 28, "slots": 12 } }
            ]
        }]
    }))
    .unwrap();
    oracles::register(&rpc, &oracles, 48, &oracles::World::default()).unwrap();

    let mut seen = Vec::new();
    for _ in 0..48 {
        chain::next_slot(&surfnet, &rpc).await.unwrap();
        let u = read();
        seen.push((u.publish_time, u.conf_raw, u.price_f64()));
    }
    let (frozen_from, frozen_to) = (seen[14].0, seen[22].0);
    assert_eq!(frozen_from, frozen_to, "publish_time freezes while stale");
    assert!(seen[30].0 > frozen_to, "and resumes after");
    let healthy = seen[8].1;
    assert!(
        seen[34].1 >= healthy * 5,
        "confidence widens: {} vs {healthy}",
        seen[34].1
    );
    assert!(
        seen[47].2 < start.price_f64() * 0.95,
        "the path's drop lands"
    );
    std::fs::remove_dir_all(&tmp).ok();
}
