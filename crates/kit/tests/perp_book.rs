//! `mtx_kit::venue::perp_top_of_book` against Phoenix's Hawkeye `view_bbo`.
//!
//! Each fixture is one simulation (`crates/grader/examples/phoenix_sample.rs`
//! on mainnet, `crates/grader/tests/geyser_parity.rs` on the bench): the
//! view's answer and the accounts it read, as one bank held them, gzipped,
//! beside the `/exchange` response of their market.
//!
//! - `phoenix-sol`: SOL on mainnet. Three with the makers' quotes apart,
//!   two locked or crossed with a base width, one crossed by two ticks.
//! - `phoenix-spcx`: SPCX in a flash-crash session, where a maker with
//!   $0.08 of collateral can't fund the side that adds to its position.
//!
//! `MTX_PHOENIX_SAMPLES=$PWD/target/phoenix-samples/SOL cargo test -p
//! mtx-kit --test perp_book -- --ignored` checks every sample a run of the
//! sampler took (the directory is named by the symbol).

use std::{
    io::Read as _,
    path::{Path, PathBuf},
};

use mtx_kit::venue::{
    PerpBookAccounts, PhoenixVenue, TopOfBook, perp_position_live, perp_top_of_book,
};
use serde::Deserialize;

/// Samples read from a directory at most: a sampler run takes 1,000.
const SAMPLES_MAX: usize = 1_000;

#[derive(Deserialize)]
struct View {
    slot: u64,
    best_bid_ticks: Option<u64>,
    best_ask_ticks: Option<u64>,
}

fn fixtures(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// A file's bytes: `<name>.gz` in the fixtures, `<name>` in a sampler's
/// output.
fn file(dir: &Path, name: &str) -> Vec<u8> {
    let gzipped = dir.join(format!("{name}.gz"));
    if gzipped.exists() {
        let mut bytes = Vec::new();
        flate2::read::GzDecoder::new(std::fs::File::open(&gzipped).expect("the fixture"))
            .read_to_end(&mut bytes)
            .expect("a gzipped file");
        return bytes;
    }
    std::fs::read(dir.join(name)).expect("the sample's file")
}

fn account(sample: &Path, address: &str) -> Vec<u8> {
    file(sample, &format!("{address}.bin"))
}

/// The `/exchange` response saved beside the samples.
fn venue(dir: &Path) -> PhoenixVenue {
    let json = String::from_utf8(file(dir, "exchange.json")).expect("utf-8");
    PhoenixVenue::from_json(&json).expect("an exchange")
}

/// A sample's accounts, owned, in the order [`PerpBookAccounts`] takes them.
struct Accounts {
    perp_asset_map: Vec<u8>,
    orderbook: Vec<u8>,
    spline_collection: Vec<u8>,
    global_trader_index: Vec<Vec<u8>>,
    active_trader_buffer: Vec<Vec<u8>>,
}

impl Accounts {
    fn read(venue: &PhoenixVenue, symbol: &str, sample: &Path) -> Self {
        let market = venue.market(symbol).expect("the market");
        let keys = &venue.exchange().keys;
        let all = |addresses: &[String]| addresses.iter().map(|a| account(sample, a)).collect();
        Self {
            perp_asset_map: account(sample, &keys.perp_asset_map),
            orderbook: account(sample, &market.market_pubkey),
            spline_collection: account(sample, &market.spline_pubkey),
            global_trader_index: all(&keys.global_trader_index),
            active_trader_buffer: all(&keys.active_trader_buffer),
        }
    }

    fn index(&self) -> Vec<&[u8]> {
        self.global_trader_index.iter().map(Vec::as_slice).collect()
    }

    fn buffer(&self) -> Vec<&[u8]> {
        self.active_trader_buffer
            .iter()
            .map(Vec::as_slice)
            .collect()
    }
}

/// Our top of book for one sample, and the view's.
fn check(venue: &PhoenixVenue, symbol: &str, sample: &Path) -> (TopOfBook, TopOfBook) {
    let view: View = serde_json::from_slice(&file(sample, "view.json")).expect("a view");
    let accounts = Accounts::read(venue, symbol, sample);
    let (index, buffer) = (accounts.index(), accounts.buffer());
    let book = PerpBookAccounts {
        perp_asset_map: &accounts.perp_asset_map,
        orderbook: &accounts.orderbook,
        spline_collection: &accounts.spline_collection,
        global_trader_index: &index,
        active_trader_buffer: &buffer,
    };
    let ours = perp_top_of_book(venue, symbol, &book, view.slot).expect("a book");
    let theirs = TopOfBook {
        best_bid_ticks: view.best_bid_ticks,
        best_ask_ticks: view.best_ask_ticks,
    };
    (ours, theirs)
}

fn samples(dir: &Path) -> Vec<PathBuf> {
    let mut samples: Vec<PathBuf> = std::fs::read_dir(dir)
        .expect("the samples")
        .map(|entry| entry.expect("an entry").path())
        .filter(|path| path.join("view.json").exists())
        .take(SAMPLES_MAX)
        .collect();
    samples.sort();
    samples
}

/// How many samples `dir` holds, and each whose book isn't the view's.
fn misses(dir: &Path, symbol: &str) -> (usize, Vec<String>) {
    let venue = venue(dir);
    let samples = samples(dir);
    let misses = samples
        .iter()
        .filter_map(|sample| {
            let (ours, theirs) = check(&venue, symbol, sample);
            (ours != theirs).then(|| format!("{}: {ours:?} != {theirs:?}", sample.display()))
        })
        .collect();
    (samples.len(), misses)
}

#[test]
fn equals_hawkeye_on_every_fixture() {
    for (dir, symbol, count) in [("phoenix-sol", "SOL", 6), ("phoenix-spcx", "SPCX", 3)] {
        let (checked, misses) = misses(&fixtures(dir), symbol);
        assert_eq!(checked, count, "{dir}'s fixtures");
        assert!(misses.is_empty(), "{}", misses.join("\n"));
    }
}

/// The rules the published 0.6 decoders miss, each on a fixture where it
/// decides the answer.
#[test]
fn base_width_crossing_and_margin_each_decide_a_fixture() {
    let sol = fixtures("phoenix-sol");
    let venue_sol = venue(&sol);
    // DUG quotes six ticks wider than its regions say (a base width of 6),
    // which puts its bid on 4UN's ask: the two match and 4UN's ask is gone.
    let (ours, _) = check(&venue_sol, "SOL", &sol.join("471817316"));
    assert_eq!(
        (ours.best_bid_ticks, ours.best_ask_ticks),
        (Some(6899), Some(6900))
    );
    // DUG's bid is a tick over 4UN's best ask: its 10,151 lots take both of
    // 4UN's asks it crosses, and stay best.
    let (ours, _) = check(&venue_sol, "SOL", &sol.join("471818499"));
    assert_eq!(
        (ours.best_bid_ticks, ours.best_ask_ticks),
        (Some(6899), Some(6900))
    );
    // 3t7LR is 8 lots short on $0.08: its ask two ticks over the mid would
    // add risk it can't margin, so the best ask is the next maker's, eight
    // ticks further.
    let spcx = fixtures("phoenix-spcx");
    let (ours, _) = check(&venue(&spcx), "SPCX", &spcx.join("462882506"));
    assert_eq!(
        (ours.best_bid_ticks, ours.best_ask_ticks),
        (Some(14504), Some(14514))
    );
}

/// A maker's live position is its entry in the active trader buffer: DUG
/// held 164,953 lots and 4UN 1,059 in this sample.
#[test]
fn a_makers_live_position_is_in_the_active_trader_buffer() {
    use mtx_kit::venue::phoenix_rise::accounts::spline_collection::SplineCollection;
    let dir = fixtures("phoenix-sol");
    let venue = venue(&dir);
    let market = venue.market("SOL").expect("SOL");
    let sample = dir.join("471818986");
    let accounts = Accounts::read(&venue, "SOL", &sample);
    let (index, buffer) = (accounts.index(), accounts.buffer());
    let splines =
        SplineCollection::try_from_account_bytes(&accounts.spline_collection).expect("splines");
    let mut found = Vec::new();
    for spline in splines.splines() {
        let trader = mtx_kit::Pubkey::new_from_array(spline.expect("a spline").trader());
        let name = trader.to_string();
        if name.starts_with("DUGirc") || name.starts_with("4UN4dr") {
            let position = perp_position_live(market, &trader, &index, &buffer)
                .expect("the trader accounts")
                .expect("an active maker");
            found.push((name[..6].to_string(), position.size));
        }
    }
    found.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        found,
        [
            ("4UN4dr".to_string(), 10.59),
            ("DUGirc".to_string(), 1649.53)
        ]
    );
    // A trader the index doesn't hold isn't active: its account holds it.
    let stranger = mtx_kit::Pubkey::new_from_array([9; 32]);
    assert_eq!(
        perp_position_live(market, &stranger, &index, &buffer),
        Ok(None)
    );
}

#[test]
#[ignore = "needs a sampler's output: MTX_PHOENIX_SAMPLES"]
fn equals_hawkeye_on_every_sample() {
    let dir = PathBuf::from(std::env::var("MTX_PHOENIX_SAMPLES").expect("MTX_PHOENIX_SAMPLES"));
    let symbol = dir
        .file_name()
        .and_then(|n| n.to_str())
        .expect("a symbol")
        .to_string();
    let (checked, misses) = misses(&dir, &symbol);
    assert!(checked > 0, "no samples in {}", dir.display());
    assert!(
        misses.is_empty(),
        "{} of {checked}:\n{}",
        misses.len(),
        misses.join("\n")
    );
}
