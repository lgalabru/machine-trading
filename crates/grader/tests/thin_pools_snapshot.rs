//! `snapshots/thin-pools-2026-11-21`, offline: what `backrun-thin-pools`
//! (RFD 0005) needs of it.
//!
//! - Each pool's depth: what it takes, in SOL, to move its price 10, 50 and
//!   100 bp either way (the kit's exact quote, ticks crossed).
//! - A swap of about 1 SOL's worth through every pool, both ways, lands and
//!   moves the trader's balances by exactly the kit's quote.
//! - SOL→JitoSOL→JTO→SOL lands as one v0 transaction, its accounts in a
//!   lookup table as a session writes one; its size and compute units.
//! - With the session's own tables (all ten pools' accounts once each, 256
//!   a table),
//!   a three-leg cycle and a four-swap route, each leg as a searcher sends
//!   it (its token accounts opened), fit in a transaction.
//! - JitoSOL's stake pool takes a `DepositSol` and a `WithdrawSol`.
//!
//! The snapshot is built, not committed with the test's data, so the test
//! is ignored by default:
//!
//! ```text
//! cargo test -p mtx-grader --test thin_pools_snapshot -- --ignored --nocapture
//! ```

use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

use mtx_core::mandate::{InstrumentRef, Kind};
use mtx_grader::{
    snapshot::{self, Access},
    venue::amm::{Direction, Mode, Pool, PoolSeat, Quoted, State, SwapOrder},
};
use mtx_kit::{
    chain::{
        AddressLookupTableAccount, ChainClient, ExecuteOptions, ExecutionPlan, InstructionGroup,
        MAX_TRANSACTION_SIZE, RpcChainClient, TransactionPlan, execute, lookup,
        token::{create_ata_idempotent_for, owned_amount},
    },
    protocols::USDC_MINT,
    token::{SYSTEM_PROGRAM_ID, TOKEN_PROGRAM_ID, associated_token_address},
};
use solana_keypair::Keypair;
use solana_pubkey::{Pubkey, pubkey};
use solana_signer::Signer as _;
use solana_transaction::{AccountMeta, Instruction};
use surfpool_sdk::Surfnet;

const WRAPPED_SOL_MINT: Pubkey = pubkey!("So11111111111111111111111111111111111111112");
const JITOSOL_MINT: Pubkey = pubkey!("J1toso1uCk3RLmjorhTtrVwY9HJ7X8V9yYac6Y7kGCPn");
const JTO_MINT: Pubkey = pubkey!("jtojtomepa8beP8AuQc6eXt5FriJwfFMwQx2v2f9mCL");
const GEOD_MINT: Pubkey = pubkey!("7JA5eZdCzztSfQbJvS8aVVxMFfd81Rs9VvwnocV1mKHu");

/// The snapshot's pools, by venue and address.
const POOLS: [(&str, &str); 10] = [
    ("orca", "Czfq3xZZDmsdGdUyrNLtRhGc47cXcZtLG4crryfu44zE"),
    ("raydium", "3ucNos4NbumPLZNWztqGHNFFgkHeRMBQAVemeeomsUxv"),
    ("orca", "Hp53XEtt4S8SvPCXarsLSdGfZBuUr5mMmZmX2DRNXQKp"),
    ("orca", "G2FiE1yn9N9ZJx5e1E2LxxMnHvb1H3hCuHLPfKJ98smA"),
    ("orca", "4UVvSVdeth9mxpPe4nwwZseXPd2bqqDm4xVaJSve3DXw"),
    ("raydium", "JVoPtWWDsRcLvQosu5fWc2CaNF6jEtJzbxdPtcEuvZo"),
    ("orca", "2UhFnySoJi6c89aydGAGS7ZRemo2dbkFRhvSJqDX4gHJ"),
    ("orca", "FsPFkRRTyCJrwK3S8x8JvU8Vmruk5EGKKQsLqywfvF5E"),
    ("raydium", "3RA4PDZAZ49nm6hdDCnCTrnECqQD93ny8oHP4T1CSTcR"),
    ("raydium", "BkX9RSHoDJGkU7pbHiksv3DprdLM3KWGoDSfGN9cGJ5d"),
];
/// The pools that price USDC, JitoSOL, JTO and GEOD in SOL: each its
/// token's deepest SOL pool.
const SOL_USDC: usize = 0;
const SOL_JITOSOL: usize = 2;
const SOL_JTO: usize = 6;
const SOL_GEOD: usize = 8;
/// The route SOL→JitoSOL→JTO→SOL: SOL/JitoSOL, JTO/JitoSOL (30 bp),
/// SOL/JTO (Whirlpool).
const ROUTE: [usize; 3] = [SOL_JITOSOL, 3, SOL_JTO];

/// Price moves the depth table reports, in basis points.
const DEPTH_MOVES_BPS: [u32; 3] = [10, 50, 100];
/// What each pool's test swaps pay, in SOL's worth: about seven times the
/// thinnest pool's 10 bp depth, and a hundredth of a deep one's.
const SWAP_SOL_WORTH: f64 = 1.0;
/// What the route puts in, in SOL.
const ROUTE_SOL: f64 = 1.0;
/// What the trader is funded with, of each token, in whole units: more
/// than any swap here pays.
const FUNDED_UNITS: u64 = 1_000_000;
/// A transaction's compute ceiling: the route asks for all of it.
const COMPUTE_UNIT_LIMIT_MAX: u32 = 1_400_000;
/// A lookup table's addresses, at most (the program's limit).
const TABLE_ADDRESSES_MAX: usize = 256;

const STAKE_POOL_PROGRAM: Pubkey = pubkey!("SPoo1Ku8WFXoNDMHPsrGSTSG1Y47rzgn41SLUNakuHy");
const STAKE_POOL: Pubkey = pubkey!("Jito4APyf642JPZPx3hGc6WWJ8zPKtRbRs4P815Awbb");
const STAKE_POOL_RESERVE: Pubkey = pubkey!("BgKUXdS29YcHCFrPm5M8oLHiTzZaMDjsebggjoaQ6KFL");
const STAKE_POOL_MANAGER_FEE: Pubkey = pubkey!("8yoigZfzZ1nNaadumY9uPVD118225UYHTDpmjpr2nrSa");
const STAKE_PROGRAM: Pubkey = pubkey!("Stake11111111111111111111111111111111111111");
const CLOCK_SYSVAR: Pubkey = pubkey!("SysvarC1ock11111111111111111111111111111111");
const STAKE_HISTORY_SYSVAR: Pubkey = pubkey!("SysvarStakeHistory1111111111111111111111111");
/// `StakePoolInstruction`'s variants, by their Borsh tag.
const DEPOSIT_SOL: u8 = 14;
const WITHDRAW_SOL: u8 = 16;
/// The stake pool's `total_lamports` and `pool_token_supply` (Borsh, after
/// the account type, five keys... and the token program: offset 258).
const STAKE_POOL_TOTALS_OFFSET: usize = 258;
const STAKE_DEPOSIT_LAMPORTS: u64 = 10_000_000_000;

fn snapshot_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../snapshots/thin-pools-2026-11-21")
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the built snapshot (just snapshot-build thin-pools-2026-11-21)"]
async fn the_thin_pools_snapshot_swaps_every_pool_and_routes_atomically() {
    let (surfnet, manifest) = snapshot::start_snapshot(&snapshot_dir(), Access::Open)
        .await
        .expect("the snapshot starts");
    println!(
        "snapshot at slot {}, {} accounts, SOL {:.4}",
        manifest.fork_slot, manifest.accounts, manifest.mark_price_usd
    );
    let chain: Arc<dyn ChainClient> = Arc::new(RpcChainClient::new(surfnet.rpc_url()));
    let pools = read_pools(chain.as_ref()).await;
    let sol_per_unit = sol_per_unit(chain.as_ref(), &pools).await;
    depths(chain.as_ref(), &pools, &sol_per_unit).await;
    let trader = funded_trader(&surfnet);
    swaps_match_their_quotes(&chain, &pools, &sol_per_unit, &trader).await;
    route_lands_in_one_transaction(&surfnet, &chain, &pools, &trader).await;
    routes_fit_the_session_tables(&surfnet, &chain, &pools, &trader).await;
    stake_pool_deposits_and_withdraws(&surfnet, chain.as_ref(), &trader).await;
}

async fn read_pools(chain: &dyn ChainClient) -> Vec<Pool> {
    let mut pools = Vec::with_capacity(POOLS.len());
    for (venue, address) in POOLS {
        let instrument = InstrumentRef {
            venue: venue.into(),
            kind: Kind::Amm,
            symbol: address.into(),
        };
        pools.push(Pool::read(chain, &instrument).await.expect("a pool"));
    }
    pools
}

/// SOL per whole unit of each of the graph's tokens, at the mids of their
/// deepest SOL pools.
async fn sol_per_unit(chain: &dyn ChainClient, pools: &[Pool]) -> BTreeMap<Pubkey, f64> {
    let mut prices = BTreeMap::from([(WRAPPED_SOL_MINT, 1.0)]);
    for (index, mint) in [
        (SOL_USDC, USDC_MINT),
        (SOL_JITOSOL, JITOSOL_MINT),
        (SOL_JTO, JTO_MINT),
        (SOL_GEOD, GEOD_MINT),
    ] {
        let pool = &pools[index];
        assert_eq!(pool.base.mint, WRAPPED_SOL_MINT, "SOL is the base");
        assert_eq!(pool.quote.mint, mint);
        let mid = State::read(chain, pool).await.expect("state").mid(pool);
        prices.insert(mint, 1.0 / mid);
    }
    assert_eq!(prices.len(), 5);
    prices
}

fn units(atoms: u64, decimals: u8) -> f64 {
    atoms as f64 / 10f64.powi(decimals.into())
}

fn name(mint: &Pubkey) -> &'static str {
    match *mint {
        WRAPPED_SOL_MINT => "SOL",
        USDC_MINT => "USDC",
        JITOSOL_MINT => "JitoSOL",
        JTO_MINT => "JTO",
        GEOD_MINT => "GEOD",
        _ => unreachable!("a token of the graph's: {mint}"),
    }
}

/// What it takes to move each pool's price down (base in) and up (quote
/// in), in SOL's worth of what goes in.
async fn depths(chain: &dyn ChainClient, pools: &[Pool], sol_per_unit: &BTreeMap<Pubkey, f64>) {
    for pool in pools {
        let state = State::read(chain, pool).await.expect("state");
        let mid = state.mid(pool);
        let mut line = format!(
            "(depth) {} {}/{} {:.0} bp, mid {mid:.6}:",
            pool.address,
            name(&pool.base.mint),
            name(&pool.quote.mint),
            state.fee_fraction() * 1e4
        );
        for bps in DEPTH_MOVES_BPS {
            let mut sides = Vec::with_capacity(2);
            for sign in [-1.0, 1.0] {
                let target = mid * (1.0 + sign * f64::from(bps) / 1e4);
                let sol = match state.quote_to_price(pool, target, u64::MAX / 2) {
                    Ok(Some((quoted, _))) => {
                        let token = match quoted.direction {
                            Direction::BaseIn => &pool.base,
                            Direction::QuoteIn => &pool.quote,
                        };
                        let worth = sol_per_unit[&token.mint];
                        format!(
                            "{:.2}",
                            units(quoted.amount_in, token.asset.decimals) * worth
                        )
                    }
                    Ok(None) => "0".into(),
                    Err(reason) => format!("n/a ({reason})"),
                };
                sides.push(sol);
            }
            line += &format!(" {bps} bp -{} +{} SOL;", sides[0], sides[1]);
        }
        println!("{line}");
    }
}

fn funded_trader(surfnet: &Surfnet) -> Keypair {
    let trader = Keypair::new();
    let cheats = surfnet.cheatcodes();
    cheats
        .fund_sol(&trader.pubkey(), 1_000_000_000_000)
        .expect("SOL");
    for (mint, decimals) in [
        (WRAPPED_SOL_MINT, 9),
        (USDC_MINT, 6),
        (JITOSOL_MINT, 9),
        (JTO_MINT, 9),
        (GEOD_MINT, 9),
    ] {
        let atoms = FUNDED_UNITS * 10u64.pow(decimals);
        cheats
            .fund_token(&trader.pubkey(), &mint, atoms, None)
            .expect("a token");
    }
    trader
}

/// Every pool, both ways, about [`SWAP_SOL_WORTH`] in, against its quote.
async fn swaps_match_their_quotes(
    chain: &Arc<dyn ChainClient>,
    pools: &[Pool],
    sol_per_unit: &BTreeMap<Pubkey, f64>,
    trader: &Keypair,
) {
    for pool in pools {
        let seat = PoolSeat::new(pool.clone(), chain.clone(), trader.insecure_clone());
        for direction in [Direction::BaseIn, Direction::QuoteIn] {
            let token = match direction {
                Direction::BaseIn => &pool.base,
                Direction::QuoteIn => &pool.quote,
            };
            let whole = SWAP_SOL_WORTH / sol_per_unit[&token.mint];
            let amount = (whole * 10f64.powi(token.asset.decimals.into())) as u64;
            let state = State::read(chain.as_ref(), pool).await.expect("state");
            let quoted = state
                .quote(&pool.address, direction, Mode::ExactIn, amount)
                .expect("a quote");
            let before = balances(chain.as_ref(), pool, &trader.pubkey()).await;
            let instructions = seat.swap_instructions(&state, &exact(&quoted), &quoted);
            send(chain.as_ref(), trader, instructions, "swap").await;
            let after = balances(chain.as_ref(), pool, &trader.pubkey()).await;
            let moved = (after.0 - before.0, after.1 - before.1);
            let (paid, got) = (-i128::from(quoted.amount_in), i128::from(quoted.amount_out));
            let expected = match direction {
                Direction::BaseIn => (paid, got),
                Direction::QuoteIn => (got, paid),
            };
            assert_eq!(moved, expected, "{} {direction:?}", pool.address);
            println!(
                "(swap) {} {direction:?}: {} {} atoms in, {} out, as quoted",
                pool.address,
                quoted.amount_in,
                name(&token.mint),
                quoted.amount_out
            );
        }
    }
}

/// An exact-in order that takes nothing less than `quoted`'s fill.
fn exact(quoted: &Quoted) -> SwapOrder {
    assert_eq!(quoted.mode, Mode::ExactIn);
    SwapOrder {
        direction: quoted.direction,
        mode: Mode::ExactIn,
        amount: quoted.amount_in,
        other_amount_threshold: quoted.amount_out,
    }
}

async fn balances(chain: &dyn ChainClient, pool: &Pool, owner: &Pubkey) -> (i128, i128) {
    let base = owned_amount(chain, owner, &pool.base.mint).await.unwrap();
    let quote = owned_amount(chain, owner, &pool.quote.mint).await.unwrap();
    (i128::from(base), i128::from(quote))
}

/// Sends `instructions` in one transaction; returns its signature and
/// simulated compute units.
async fn send(
    chain: &dyn ChainClient,
    signer: &Keypair,
    instructions: Vec<Instruction>,
    label: &str,
) -> (String, u64) {
    let plan = TransactionPlan::from_group(
        &InstructionGroup::new(label, instructions),
        COMPUTE_UNIT_LIMIT_MAX,
    );
    send_plan(chain, signer, plan).await
}

async fn send_plan(
    chain: &dyn ChainClient,
    signer: &Keypair,
    plan: TransactionPlan,
) -> (String, u64) {
    let plan = ExecutionPlan {
        payer: signer.pubkey(),
        steps: vec![plan],
    };
    let receipts = execute(
        &plan,
        signer,
        chain,
        &[],
        ExecuteOptions::without_receipts(),
    )
    .await
    .expect("the transaction lands");
    assert_eq!(receipts.len(), 1, "one transaction");
    let units = receipts[0].simulation.units_consumed.unwrap_or(0);
    (receipts[0].signature.to_string(), units)
}

/// SOL→JitoSOL→JTO→SOL in one v0 transaction, its accounts in a table
/// written as a session writes its own.
async fn route_lands_in_one_transaction(
    surfnet: &Surfnet,
    chain: &Arc<dyn ChainClient>,
    pools: &[Pool],
    trader: &Keypair,
) {
    let rpc = mtx_grader::chain::rpc_client(surfnet);
    let owner = trader.pubkey();
    let mut all = 0;
    for pool in pools {
        let accounts = mtx_grader::pools::route_accounts(&rpc, &pool.address).expect("accounts");
        all += accounts.len();
        println!("(route) {}: {} accounts", pool.address, accounts.len());
    }
    println!("(route) all ten pools: {all} accounts, a table holds {TABLE_ADDRESSES_MAX}");

    let mut table_addresses: Vec<Pubkey> = Vec::new();
    let mut instructions: Vec<Instruction> = Vec::new();
    let mut amount = (ROUTE_SOL * 1e9) as u64;
    let mut mint_in = WRAPPED_SOL_MINT;
    let mut legs = Vec::with_capacity(ROUTE.len());
    for index in ROUTE {
        let pool = &pools[index];
        for key in mtx_grader::pools::route_accounts(&rpc, &pool.address).expect("accounts") {
            if !table_addresses.contains(&key) {
                table_addresses.push(key);
            }
        }
        for token in [&pool.base, &pool.quote] {
            let ata = associated_token_address(&owner, &token.mint, &token.token_program);
            if !table_addresses.contains(&ata) {
                table_addresses.push(ata);
            }
        }
        let direction = if pool.base.mint == mint_in {
            Direction::BaseIn
        } else {
            assert_eq!(pool.quote.mint, mint_in, "the route chains");
            Direction::QuoteIn
        };
        let state = State::read(chain.as_ref(), pool).await.expect("state");
        let quoted = state
            .quote(&pool.address, direction, Mode::ExactIn, amount)
            .expect("a quote");
        let seat = PoolSeat::new(pool.clone(), chain.clone(), trader.insecure_clone());
        for instruction in seat.swap_instructions(&state, &exact(&quoted), &quoted) {
            // Each leg opens its accounts; one open of each is enough.
            if !instructions.contains(&instruction) {
                instructions.push(instruction);
            }
        }
        mint_in = match direction {
            Direction::BaseIn => pool.quote.mint,
            Direction::QuoteIn => pool.base.mint,
        };
        amount = quoted.amount_out;
        legs.push(quoted);
    }
    assert_eq!(mint_in, WRAPPED_SOL_MINT, "the route ends in SOL");
    let table = write_table(surfnet, &table_addresses);
    send_route(chain.as_ref(), trader, instructions, table, &legs).await;
}

async fn send_route(
    chain: &dyn ChainClient,
    trader: &Keypair,
    instructions: Vec<Instruction>,
    table: AddressLookupTableAccount,
    legs: &[Quoted],
) {
    let owner = trader.pubkey();
    let group = InstructionGroup::new("SOL→JitoSOL→JTO→SOL", instructions);
    let legacy = TransactionPlan::from_group(&group, COMPUTE_UNIT_LIMIT_MAX);
    let addresses = table.addresses.len();
    let plan = legacy.clone().with_lookup_tables(&[table]);
    let size = plan.serialized_size(&owner);
    println!(
        "(route) {} instructions: {size} bytes with the table of {addresses} (legacy: {} bytes, limit {MAX_TRANSACTION_SIZE})",
        group.instructions.len() + 1,
        legacy.serialized_size(&owner)
    );
    assert!(
        size <= MAX_TRANSACTION_SIZE,
        "the route fits one transaction"
    );

    let mints = [WRAPPED_SOL_MINT, JITOSOL_MINT, JTO_MINT];
    let mut before = [0u64; 3];
    for (held, mint) in before.iter_mut().zip(&mints) {
        *held = owned_amount(chain, &owner, mint).await.unwrap();
    }
    let (signature, units) = send_plan(chain, trader, plan).await;
    let mut moved = [0i128; 3];
    for ((delta, mint), held) in moved.iter_mut().zip(&mints).zip(before) {
        *delta = i128::from(owned_amount(chain, &owner, mint).await.unwrap()) - i128::from(held);
    }
    let sol_in = i128::from(legs[0].amount_in);
    let sol_out = i128::from(legs[2].amount_out);
    assert_eq!(moved, [sol_out - sol_in, 0, 0], "only SOL moves, as quoted");
    println!(
        "(route) landed {signature}: {units} CU; {sol_in} SOL atoms in, {sol_out} out ({:+.1} bp)",
        (sol_out - sol_in) as f64 / sol_in as f64 * 1e4
    );
}

/// A three-leg cycle (SOL→GEOD→USDC→SOL) and a four-swap route
/// (SOL→USDC→GEOD→SOL→JitoSOL), every leg's instructions as a searcher's
/// seat makes them, compiled with the tables an `amm` session over the ten
/// pools writes: both fit in a transaction.
async fn routes_fit_the_session_tables(
    surfnet: &Surfnet,
    chain: &Arc<dyn ChainClient>,
    pools: &[Pool],
    trader: &Keypair,
) {
    let rpc = mtx_grader::chain::rpc_client(surfnet);
    let owner = trader.pubkey();
    let accounts =
        mtx_grader::lending::route::route_accounts(chain.as_ref(), &rpc, None, pools, &owner)
            .await
            .expect("the session's accounts");
    let address = |_: u32| Pubkey::new_unique();
    let tables = mtx_grader::lending::route::write_tables(surfnet, &address, &accounts)
        .expect("the session's tables");
    println!(
        "(tables) {} addresses in {} tables",
        accounts.len(),
        tables.len()
    );
    // Ten pools name 279 accounts counted pool by pool, 229 once each
    // (their programs, mints and token programs are shared): one table.
    assert!(!tables.is_empty() && tables.len() <= mtx_grader::lending::route::TABLES_MAX);
    // Pools by index: SOL/USDC Whirlpool 0, SOL/JitoSOL 2, SOL/GEOD Raydium
    // 8, GEOD/USDC 9.
    let routes: [(&str, &[usize]); 2] = [
        ("three-leg cycle SOL→GEOD→USDC→SOL", &[8, 9, 0]),
        ("four-swap route SOL→USDC→GEOD→SOL→JitoSOL", &[0, 9, 8, 2]),
    ];
    for (name, route) in routes {
        let mut instructions: Vec<Instruction> = Vec::new();
        let mut amount = (ROUTE_SOL * 1e9) as u64;
        let mut mint_in = WRAPPED_SOL_MINT;
        for &index in route {
            let pool = &pools[index];
            let direction = match pool.base.mint == mint_in {
                true => Direction::BaseIn,
                false => Direction::QuoteIn,
            };
            let state = State::read(chain.as_ref(), pool).await.expect("state");
            let quoted = state
                .quote(&pool.address, direction, Mode::ExactIn, amount)
                .expect("a quote");
            let seat = PoolSeat::new(pool.clone(), chain.clone(), trader.insecure_clone());
            instructions.extend(seat.swap_instructions(&state, &exact(&quoted), &quoted));
            mint_in = match direction {
                Direction::BaseIn => pool.quote.mint,
                Direction::QuoteIn => pool.base.mint,
            };
            amount = quoted.amount_out;
        }
        let group = InstructionGroup::new(name, instructions);
        let plan = TransactionPlan::from_group(&group, COMPUTE_UNIT_LIMIT_MAX)
            .with_compute_unit_price(1_000)
            .with_lookup_tables(&tables);
        let size = plan.serialized_size(&owner);
        println!(
            "(tables) {name}: {} instructions, {size} bytes (limit {MAX_TRANSACTION_SIZE})",
            group.instructions.len() + 2
        );
        assert!(size <= MAX_TRANSACTION_SIZE, "{name} fits a transaction");
    }
}

/// A lookup table holding `addresses`, active from the start with no
/// authority: what a session writes for its routes.
fn write_table(surfnet: &Surfnet, addresses: &[Pubkey]) -> AddressLookupTableAccount {
    /// `LookupTableMeta`'s serialized length.
    const META_LEN: usize = 56;
    assert!(addresses.len() <= TABLE_ADDRESSES_MAX);
    let mut data = vec![0u8; META_LEN];
    // `ProgramState::LookupTable`, then a deactivation slot of u64::MAX.
    data[0] = 1;
    data[4..12].copy_from_slice(&u64::MAX.to_le_bytes());
    for address in addresses {
        data.extend_from_slice(address.as_ref());
    }
    let address = Pubkey::new_unique();
    surfnet
        .cheatcodes()
        .set_account(&address, 1_000_000_000, &data, &lookup::program())
        .expect("the table");
    let table = lookup::decode(&address, &lookup::program(), &data).expect("a table");
    assert_eq!(table.addresses, addresses);
    table
}

/// `DepositSol` then `WithdrawSol` of what it minted, on JitoSOL's pool.
async fn stake_pool_deposits_and_withdraws(
    surfnet: &Surfnet,
    chain: &dyn ChainClient,
    trader: &Keypair,
) {
    let rpc = mtx_grader::chain::rpc_client(surfnet);
    let owner = trader.pubkey();
    let jitosol = associated_token_address(&owner, &JITOSOL_MINT, &TOKEN_PROGRAM_ID);
    let (total_lamports, supply) = stake_pool_totals(&rpc);
    let nav = total_lamports as f64 / supply as f64;
    let before = owned_amount(chain, &owner, &JITOSOL_MINT).await.unwrap();
    let deposit = vec![
        create_ata_idempotent_for(&owner, &owner, &JITOSOL_MINT, &TOKEN_PROGRAM_ID),
        stake_pool_instruction(
            DEPOSIT_SOL,
            STAKE_DEPOSIT_LAMPORTS,
            &[
                (owner, true, true),
                (jitosol, false, true),
                (STAKE_POOL_MANAGER_FEE, false, true),
                (jitosol, false, true),
                (JITOSOL_MINT, false, true),
                (SYSTEM_PROGRAM_ID, false, false),
                (TOKEN_PROGRAM_ID, false, false),
            ],
        ),
    ];
    let (_, deposit_units) = send(chain, trader, deposit, "DepositSol").await;
    let minted = owned_amount(chain, &owner, &JITOSOL_MINT).await.unwrap() - before;
    let expected =
        u128::from(STAKE_DEPOSIT_LAMPORTS) * u128::from(supply) / u128::from(total_lamports);
    assert_eq!(
        u128::from(minted),
        expected,
        "the pool's rate, no deposit fee"
    );
    println!(
        "(stake) DepositSol {STAKE_DEPOSIT_LAMPORTS} lamports: {minted} JitoSOL atoms (NAV {nav:.6}), {deposit_units} CU"
    );

    let lamports_before = rpc.get_balance(&owner).unwrap();
    let withdraw = vec![stake_pool_instruction(
        WITHDRAW_SOL,
        minted,
        &[
            (owner, true, false),
            (jitosol, false, true),
            (STAKE_POOL_RESERVE, false, true),
            (owner, false, true),
            (STAKE_POOL_MANAGER_FEE, false, true),
            (JITOSOL_MINT, false, true),
            (CLOCK_SYSVAR, false, false),
            (STAKE_HISTORY_SYSVAR, false, false),
            (STAKE_PROGRAM, false, false),
            (TOKEN_PROGRAM_ID, false, false),
        ],
    )];
    let (_, withdraw_units) = send(chain, trader, withdraw, "WithdrawSol").await;
    let returned = rpc.get_balance(&owner).unwrap() as i128 - lamports_before as i128;
    let kept_bps = (1.0 - returned as f64 / STAKE_DEPOSIT_LAMPORTS as f64) * 1e4;
    println!(
        "(stake) WithdrawSol {minted} JitoSOL atoms: {returned} lamports net of the fee and the transaction's ({kept_bps:.2} bp kept), {withdraw_units} CU"
    );
    assert!((9.0..12.0).contains(&kept_bps), "the 10 bp withdrawal fee");
}

/// A stake-pool instruction: the pool and its withdraw authority, then
/// `rest`, as (key, signer, writable).
fn stake_pool_instruction(tag: u8, amount: u64, rest: &[(Pubkey, bool, bool)]) -> Instruction {
    let (authority, _) =
        Pubkey::find_program_address(&[STAKE_POOL.as_ref(), b"withdraw"], &STAKE_POOL_PROGRAM);
    let mut accounts = vec![
        AccountMeta::new(STAKE_POOL, false),
        AccountMeta::new_readonly(authority, false),
    ];
    // DepositSol names the reserve third; WithdrawSol names its own order.
    if tag == DEPOSIT_SOL {
        accounts.push(AccountMeta::new(STAKE_POOL_RESERVE, false));
    }
    for &(key, is_signer, is_writable) in rest {
        accounts.push(match is_writable {
            true => AccountMeta::new(key, is_signer),
            false => AccountMeta::new_readonly(key, is_signer),
        });
    }
    let mut data = vec![tag];
    data.extend_from_slice(&amount.to_le_bytes());
    Instruction {
        program_id: STAKE_POOL_PROGRAM,
        accounts,
        data,
    }
}

fn stake_pool_totals(rpc: &solana_rpc_client::rpc_client::RpcClient) -> (u64, u64) {
    let data = rpc.get_account(&STAKE_POOL).unwrap().data;
    let read = |at: usize| u64::from_le_bytes(data[at..at + 8].try_into().unwrap());
    let totals = (
        read(STAKE_POOL_TOTALS_OFFSET),
        read(STAKE_POOL_TOTALS_OFFSET + 8),
    );
    assert!(totals.0 > totals.1, "a JitoSOL is worth more than a SOL");
    totals
}
