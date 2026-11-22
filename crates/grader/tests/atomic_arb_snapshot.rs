//! `snapshots/atomic-arb-2026-11-21`, offline: what the atomic-arbitrage
//! tasks need of it (RFD 0005).
//!
//! - A 500 SOL sell through each cloned pool lands and moves the trader's
//!   balances by exactly what the kit quoted.
//! - The main market's SOL/USDC pair takes a deposit, a borrow, a repay and
//!   a withdrawal (the recipe's own warm-up path).
//! - Scope's SOL entry turns into a `FixedPrice` by override, a top-level
//!   `refresh_price_list` publishes it, and klend's `refresh_reserve`
//!   accepts it.
//!
//! The snapshot is built, not committed with the test's data, so the test
//! is ignored by default:
//!
//! ```text
//! cargo test -p mtx-grader --test atomic_arb_snapshot -- --ignored --nocapture --test-threads 1
//! ```

use std::{path::PathBuf, sync::Arc, time::Duration};

use mtx_core::{
    Decimal, Holding,
    mandate::{InstrumentRef, Kind},
};
use mtx_grader::{
    chain, lending,
    scope::{self, MAIN_FEED, Price},
    snapshot::{self, Access},
    venue::{
        Market, Seat, Venue,
        amm::{Amm, PoolSeat, State, SwapOrder},
        kamino::klend::{self, PRICE_STATUS_OK, Reserve, ReserveKeys},
    },
};
use mtx_kit::{
    Action, AmountUnit, Planned, Side,
    chain::{ChainClient, RpcChainClient},
    protocols::{USDC_MINT, orca::whirlpool, raydium::clmm},
};
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_rpc_client::rpc_client::RpcClient;
use solana_signer::Signer as _;
use surfpool_sdk::Surfnet;

const WRAPPED_SOL_MINT: Pubkey =
    solana_pubkey::pubkey!("So11111111111111111111111111111111111111112");
const SOL_RESERVE: Pubkey = solana_pubkey::pubkey!("d4A2prbA2whesmvHaL88BH6Ewn5N4bTSU2Ze8P6Bc4Q");
const USDC_RESERVE: Pubkey = solana_pubkey::pubkey!("D6q6wuQSrifJKZYpR1M8R4YawnLDtDsMmWM1NbBmgJ59");
/// Scope's SOL entry on the main feed, and the Pyth Lazer EMA the SOL
/// reserve reads as its TWAP.
const SOL_ENTRY: u16 = 3;
const SOL_TWAP_ENTRY: u16 = 455;
/// The research's median trigger.
const SWAP_SOL: f64 = 500.0;
/// Polls for a landed swap or a played override, a slot (400 ms) apart.
const POLLS_MAX: u32 = 60;
const POLL_INTERVAL: Duration = Duration::from_millis(250);

fn snapshot_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../snapshots/atomic-arb-2026-11-21")
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the built snapshot (just snapshot-build atomic-arb-2026-11-21)"]
async fn the_atomic_arb_snapshot_swaps_lends_and_takes_a_pushed_price() {
    let (surfnet, manifest) = snapshot::start_snapshot(&snapshot_dir(), Access::Open)
        .await
        .expect("the snapshot starts");
    println!(
        "snapshot at slot {}, {} accounts",
        manifest.fork_slot, manifest.accounts
    );
    swaps_match_their_quotes(&surfnet).await;
    lending_works(&surfnet).await;
    a_pushed_price_reaches_the_reserve(&surfnet).await;
}

/// (a) A 500 SOL sell through each pool, against the kit's quote.
async fn swaps_match_their_quotes(surfnet: &Surfnet) {
    let client = Arc::new(RpcChainClient::new(surfnet.rpc_url()));
    let chain: Arc<dyn ChainClient> = client.clone();
    let trader = Keypair::new();
    let cheats = surfnet.cheatcodes();
    cheats.fund_sol(&trader.pubkey(), 10_000_000_000).unwrap();
    cheats
        .fund_token(&trader.pubkey(), &WRAPPED_SOL_MINT, 2_000_000_000_000, None)
        .unwrap();
    cheats
        .fund_token(&trader.pubkey(), &USDC_MINT, 1_000_000_000, None)
        .unwrap();
    for (venue, address) in [
        ("orca", whirlpool::SOL_USDC_POOL),
        ("raydium", clmm::SOL_USDC_POOL),
    ] {
        let instrument = InstrumentRef {
            venue: venue.into(),
            kind: Kind::Amm,
            symbol: address.to_string(),
        };
        let amm = Amm::connect(venue, chain.clone(), std::slice::from_ref(&instrument))
            .await
            .expect("venue");
        let pool = amm.pools()[0].clone();
        let market = amm.market(&instrument).await.expect("market");
        let seat = PoolSeat::new(pool.clone(), chain.clone(), trader.insecure_clone());
        let (_, ask) = market.top().await.expect("top");
        let limit = ask.expect("an ask") * 0.9;
        let order =
            SwapOrder::from_swap(&pool, Side::Ask, SWAP_SOL, AmountUnit::Base, limit).unwrap();
        let state = State::read(chain.as_ref(), &pool).await.expect("state");
        let quoted = state
            .quote(&pool.address, order.direction, order.mode, order.amount)
            .expect("quote");
        let before = atoms(&*market, &trader.pubkey()).await;
        let sent = seat
            .send(&Planned {
                line: 0,
                venue: None,
                client_id: None,
                action: Action::Swap {
                    side: Side::Ask,
                    amount: SWAP_SOL,
                    unit: AmountUnit::Base,
                    limit,
                },
                reduce_only: false,
                expires_slot: None,
                tx: None,
            })
            .await;
        assert!(sent.sent, "{venue}: {sent:?}");
        let after = landed(&*market, &trader.pubkey(), before).await;
        let moved = (after.0 - before.0, after.1 - before.1);
        let expected = (-i128::from(quoted.amount_in), i128::from(quoted.amount_out));
        assert_eq!(moved, expected, "{venue}: base and quote atoms");
        let price = quoted.amount_out as f64 / 1e6 / (quoted.amount_in as f64 / 1e9);
        println!(
            "(a) {venue}: sold {} SOL atoms for {} USDC atoms ({price:.4} USDC/SOL, top ask {:.4}), as quoted",
            quoted.amount_in,
            quoted.amount_out,
            ask.unwrap()
        );
    }
}

/// (b) The recipe's lending path on the main market, offline.
async fn lending_works(surfnet: &Surfnet) {
    let rpc = chain::rpc_client(surfnet);
    let spec = lending::MarketsSpec {
        pairs: vec![[SOL_RESERVE.to_string(), USDC_RESERVE.to_string()]],
    };
    let price = lending::exercise(surfnet, &rpc, &spec)
        .await
        .expect("deposit, borrow, repay, withdraw");
    println!("(b) SOL/USDC on the main market: deposit, borrow, repay, withdraw at SOL {price:.4}");
}

/// (c) Scope's SOL entry as a `FixedPrice` a bot pushes, read by klend.
async fn a_pushed_price_reaches_the_reserve(surfnet: &Surfnet) {
    let rpc = chain::rpc_client(surfnet);
    let payer = Keypair::new();
    surfnet
        .cheatcodes()
        .fund_sol(&payer.pubkey(), 10_000_000_000)
        .unwrap();
    let reserve = read_reserve(&rpc);
    println!(
        "(c) SOL reserve: price chain {:?}, TWAP chain {:?}, max age {} s, TWAP max age {} s, divergence {} bp",
        reserve.price_chain,
        reserve.twap_chain,
        reserve.max_age_price_seconds,
        reserve.max_age_twap_seconds,
        reserve.max_twap_divergence_bps
    );
    assert_eq!(reserve.price_chain[0], SOL_ENTRY);
    let before = mapping(&rpc, SOL_ENTRY);
    println!(
        "(c) entry {SOL_ENTRY} before: type {} account {}",
        before.oracle_type, before.price_account
    );

    // The zero key, as `validate_mapping` might suggest, makes the refresh
    // skip the entry ("no mapping is set").
    let skipped = Price::from_f64(reserve.price * 1.01, 8).unwrap();
    let mut zero = scope::fixed_price_mapping(&MAIN_FEED, SOL_ENTRY, skipped, 1);
    zero.values["price_info_accounts.3"] = serde_json::json!(Pubkey::default().to_string());
    play(&rpc, zero, |m| {
        m.oracle_type == scope::ORACLE_TYPE_FIXED_PRICE
    })
    .await;
    let (old, _, _) = prices(&rpc, SOL_ENTRY);
    let refresh = scope::refresh_price_list(&MAIN_FEED, &[SOL_ENTRY], &[Pubkey::default()]);
    mtx_kit::send(&rpc, &payer, vec![refresh], "refresh (zero key)").expect("a skip lands");
    assert_eq!(prices(&rpc, SOL_ENTRY).0, old, "a zero key is skipped");
    println!("(c) price account = zero key: refresh skips the entry");

    // Scope's own id, as `set_entry_mapping` stores for "no account".
    let pushed = Price::from_f64(reserve.price * 0.98, 8).unwrap();
    let mapped = scope::fixed_price_mapping(&MAIN_FEED, SOL_ENTRY, pushed, 1);
    play(&rpc, mapped, |m| {
        m.price_account == scope::PROGRAM_ID && Price::from_generic(&m.generic) == pushed
    })
    .await;
    let landed_slot = refresh_and_check(&rpc, &payer, &[SOL_ENTRY], pushed);
    println!(
        "(c) entry {SOL_ENTRY} = FixedPrice {pushed:?}: refresh_price_list wrote it at slot {landed_slot}"
    );

    // Anything but the compute budget before the refresh is refused.
    let memo = solana_transaction::Instruction {
        program_id: whirlpool::ix::MEMO_PROGRAM_ID,
        accounts: vec![],
        data: b"first".to_vec(),
    };
    let refused = mtx_kit::send(
        &rpc,
        &payer,
        vec![memo, scope::refresh_fixed_prices(&MAIN_FEED, &[SOL_ENTRY])],
        "refresh after a memo",
    );
    let reason = format!("{refused:?}");
    assert!(reason.contains("RefreshWithUnexpectedIxs"), "{reason}");
    println!("(c) a refresh after another instruction: RefreshWithUnexpectedIxs");

    let status = refresh_reserve(&rpc, &payer);
    let read = read_reserve(&rpc);
    println!(
        "(c) refresh_reserve: price {:.6} (pushed {:.6}), status {status:#08b}",
        read.price,
        pushed.to_f64()
    );
    assert_eq!(status, PRICE_STATUS_OK, "fresh price and TWAP");
    assert!((read.price - pushed.to_f64()).abs() < 1e-6);

    // Past the TWAP's age, the price alone is not enough.
    let (slot, unix) = clock(&rpc);
    let ahead_s = read.max_age_twap_seconds + 60;
    surfnet
        .cheatcodes()
        .time_travel_to_slot_at(slot + ahead_s * 1_000 / 400, (unix + ahead_s) * 1_000)
        .unwrap();
    refresh_and_check(&rpc, &payer, &[SOL_ENTRY], pushed);
    let stale = refresh_reserve(&rpc, &payer);
    println!("(c) {ahead_s} s later, SOL entry refreshed alone: status {stale:#08b}");
    assert_ne!(stale, PRICE_STATUS_OK, "a stale TWAP fails a check");

    // The TWAP entry as a FixedPrice too, pushed with the price.
    let twap = scope::fixed_price_mapping(&MAIN_FEED, SOL_TWAP_ENTRY, pushed, 1);
    play_entry(&rpc, twap, SOL_TWAP_ENTRY, |m| {
        m.oracle_type == scope::ORACLE_TYPE_FIXED_PRICE
    })
    .await;
    refresh_and_check(&rpc, &payer, &[SOL_ENTRY, SOL_TWAP_ENTRY], pushed);
    let both = refresh_reserve(&rpc, &payer);
    println!("(c) entries {SOL_ENTRY} and {SOL_TWAP_ENTRY} refreshed together: status {both:#08b}");
    assert_eq!(both, PRICE_STATUS_OK);
}

/// Refreshes FixedPrice `entries` in one top-level instruction and checks
/// each now holds `price`, stamped with the slot it landed in.
fn refresh_and_check(rpc: &RpcClient, payer: &Keypair, entries: &[u16], price: Price) -> u64 {
    let refresh = scope::refresh_fixed_prices(&MAIN_FEED, entries);
    let signature = mtx_kit::send(rpc, payer, vec![refresh], "refresh").expect("refresh");
    let landed = rpc
        .get_signature_statuses(&[signature])
        .expect("statuses")
        .value[0]
        .as_ref()
        .expect("the refresh's status")
        .slot;
    let (_, unix) = clock(rpc);
    for entry in entries {
        let (written, slot, time) = prices(rpc, *entry);
        assert_eq!(written, price, "entry {entry}");
        assert_eq!(slot, landed, "entry {entry} stamped with its slot");
        assert!(
            time <= unix && time + 5 >= unix,
            "entry {entry} stamped now"
        );
    }
    landed
}

/// Sends `refresh_reserve` on the SOL reserve; returns its price status.
fn refresh_reserve(rpc: &RpcClient, payer: &Keypair) -> u8 {
    let reserve = read_reserve(rpc);
    let keys = ReserveKeys::of(SOL_RESERVE, &reserve);
    mtx_kit::send(
        rpc,
        payer,
        vec![klend::refresh_reserve(&keys)],
        "refresh reserve",
    )
    .expect("refresh_reserve");
    read_reserve(rpc).price_status
}

fn read_reserve(rpc: &RpcClient) -> Reserve {
    Reserve::decode(&rpc.get_account(&SOL_RESERVE).unwrap().data).expect("a reserve")
}

fn mapping(rpc: &RpcClient, entry: u16) -> scope::MappingEntry {
    let data = rpc.get_account(&MAIN_FEED.mappings).unwrap().data;
    scope::mapping_entry(&data, entry).expect("a mapping entry")
}

fn prices(rpc: &RpcClient, entry: u16) -> (Price, u64, u64) {
    let data = rpc.get_account(&MAIN_FEED.prices).unwrap().data;
    scope::dated_price(&data, entry).expect("a price entry")
}

fn clock(rpc: &RpcClient) -> (u64, u64) {
    let data = rpc
        .get_account(&solana_pubkey::pubkey!(
            "SysvarC1ock11111111111111111111111111111111"
        ))
        .unwrap()
        .data;
    let read = |at: usize| u64::from_le_bytes(data[at..at + 8].try_into().unwrap());
    (read(0), read(32))
}

async fn play(rpc: &RpcClient, o: chain::Override, done: impl Fn(&scope::MappingEntry) -> bool) {
    play_entry(rpc, o, SOL_ENTRY, done).await;
}

/// Registers one override and waits until the mapping shows it.
async fn play_entry(
    rpc: &RpcClient,
    o: chain::Override,
    entry: u16,
    done: impl Fn(&scope::MappingEntry) -> bool,
) {
    let label = o.label.clone();
    chain::register_scenario(rpc, &label, vec![o]).expect("scenario");
    for _ in 0..POLLS_MAX {
        if done(&mapping(rpc, entry)) {
            return;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
    panic!(
        "{label}: the override never played: {:?}",
        mapping(rpc, entry)
    );
}

/// The owner's base and quote in the market's wallet holdings, in atoms.
async fn atoms(market: &dyn Market, owner: &Pubkey) -> (i128, i128) {
    let held = market.holdings(owner).await.expect("holdings");
    let atoms = |holding: &Holding| -> i128 {
        let Holding::Spot(spot) = holding else {
            panic!("a pool holds spot assets, not {holding:?}");
        };
        let scaled = spot.wallet * Decimal::from(10u64.pow(spot.base.decimals.into()));
        assert_eq!(scaled.fract(), Decimal::ZERO, "whole atoms");
        scaled.mantissa() / 10i128.pow(scaled.scale())
    };
    assert_eq!(held.len(), 2, "base, then quote");
    (atoms(&held[0]), atoms(&held[1]))
}

/// The balances once they differ from `before`: the swap landed.
async fn landed(market: &dyn Market, owner: &Pubkey, before: (i128, i128)) -> (i128, i128) {
    for _ in 0..POLLS_MAX {
        let now = atoms(market, owner).await;
        if now != before {
            return now;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
    panic!("the swap didn't land within {POLLS_MAX} polls");
}
