//! Requires binaries built from the pinned upstream sources; never substitutes mocks.
use std::path::PathBuf;

use mth_backrun::{WSOL, world::World};
use solana_keypair::read_keypair_file;
use solana_signer::Signer;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires MTH_BACKRUN_PROGRAMS, MTH_CPMM_ADMIN, MTH_ORCA_ADMIN"]
async fn both_actual_amms_initialize_and_execute_offline() -> anyhow::Result<()> {
    let root = PathBuf::from(std::env::var("MTH_BACKRUN_PROGRAMS")?);
    let cp = PathBuf::from(std::env::var("MTH_CPMM_ADMIN")?);
    let orca =
        read_keypair_file(std::env::var("MTH_ORCA_ADMIN")?).map_err(|e| anyhow::anyhow!("{e}"))?;
    let world = World::new(&root, &cp).await?;
    let tokens = [
        key(WSOL),
        world.create_mint().await?,
        world.create_mint().await?,
    ];
    for mint in tokens {
        world
            .fund_tokens(world.payer().pubkey(), mint, 1_000_000_000_000_000)
            .await?;
    }
    let config = world.cpmm_config(0, 2500).await?;
    let cpmm = world
        .create_cpmm(config, &tokens, [0, 1], 1_000_000_000_000, 2500)
        .await?;
    let bridge = world
        .create_cpmm(config, &tokens, [1, 2], 1_000_000_000_000, 2500)
        .await?;
    let config = world.whirlpool_config(&orca, 8, 3000).await?;
    let whirlpool = world
        .create_whirlpool(config, &tokens, [0, 1], 8, 3000, 2_000_000_000_000)
        .await?;
    let third_leg = world
        .create_whirlpool(config, &tokens, [0, 2], 8, 3000, 2_000_000_000_000)
        .await?;
    // CPMM only opens after the setup clock advances past its open_time.
    world
        .surfnet
        .cheatcodes()
        .time_travel_to_slot_at(10_000, 1_800_000_000_000)?;
    for pool in [&cpmm, &whirlpool] {
        let before = world.token_balance(world.payer().pubkey(), tokens[pool.mints[1]])?;
        world.swap(pool, true, 1_000_000_000, 1).await?;
        let after = world.token_balance(world.payer().pubkey(), tokens[pool.mints[1]])?;
        assert!(after > before, "real AMM must transfer output tokens");
        world.swap(pool, false, 500_000_000, 1).await?;
    }
    world.swap(&whirlpool, true, 50_000_000_000, 1).await?;
    let crossed = world.rpc.get_account_data(&key(&whirlpool.address))?;
    assert!(
        i32::from_le_bytes(crossed[81..85].try_into()?).abs() > 32,
        "real swap crosses the inner liquidity band"
    );
    world.swap(&whirlpool, false, 50_000_000_000, 1).await?;
    // Optional custom execution is an ordinary signer-authorized CPI call.
    use mth_backrun::{fixture::Market, programs::*, wallet::Wallet};
    use solana_pubkey::Pubkey;
    use surfpool_sdk::cheatcodes::builders::DeployProgram;
    let executor = Pubkey::new_unique();
    world
        .surfnet
        .cheatcodes()
        .deploy(DeployProgram::new(executor).so_path(root.join("mth_backrun_executor.so")))?;
    world.make_immutable(executor)?;
    let market = Market {
        version: 1,
        tokens: tokens.iter().map(ToString::to_string).collect(),
        pools: vec![
            cpmm.clone(),
            whirlpool.clone(),
            bridge.clone(),
            third_leg.clone(),
        ],
        program_hashes: Default::default(),
    };
    let session = Wallet::prepare(&world, &market, executor).await?;
    let buy_with_wsol = cpmm.mints[0] == 0;
    world.swap(&cpmm, buy_with_wsol, 50_000_000_000, 1).await?;
    let data = world.rpc.get_account_data(&key(&whirlpool.address))?;
    let tick = i32::from_le_bytes(data[81..85].try_into()?);
    let swaps = [
        whirlpool_swap(
            &whirlpool,
            session.trader.pubkey(),
            whirlpool.mints[0] == 0,
            1_000_000_000,
            1,
            tick,
        )?,
        cpmm_swap(&cpmm, session.trader.pubkey(), !buy_with_wsol, 1, 1),
    ];
    let inputs = [
        ata(session.trader.pubkey(), tokens[0]),
        ata(session.trader.pubkey(), tokens[1]),
    ];
    let before = world.token_balance(session.trader.pubkey(), tokens[0])?;
    let ix = session.route(&swaps, &inputs)?;
    world.send(vec![ix], &[&session.trader]).await?;
    assert!(world.token_balance(session.trader.pubkey(), tokens[0])? > before);
    assert_eq!(world.token_balance(session.trader.pubkey(), tokens[1])?, 0);
    let third_tick = world.rpc.get_account_data(&key(&third_leg.address))?;
    let cycle = [
        whirlpool_swap(
            &third_leg,
            session.trader.pubkey(),
            third_leg.mints[0] == 0,
            1_000_000_000,
            1,
            i32::from_le_bytes(third_tick[81..85].try_into()?),
        )?,
        cpmm_swap(&bridge, session.trader.pubkey(), bridge.mints[0] == 2, 1, 1),
        cpmm_swap(&cpmm, session.trader.pubkey(), !buy_with_wsol, 1, 1),
    ];
    let cycle_inputs = [
        ata(session.trader.pubkey(), tokens[0]),
        ata(session.trader.pubkey(), tokens[2]),
        ata(session.trader.pubkey(), tokens[1]),
    ];
    let before_three = world.token_balance(session.trader.pubkey(), tokens[0])?;
    world
        .send(
            vec![session.route(&cycle, &cycle_inputs)?],
            &[&session.trader],
        )
        .await?;
    assert!(world.token_balance(session.trader.pubkey(), tokens[0])? > before_three);
    assert_eq!(world.token_balance(session.trader.pubkey(), tokens[1])?, 0);
    assert_eq!(world.token_balance(session.trader.pubkey(), tokens[2])?, 0);
    // Ordinary AMM slippage rolls back the entire multi-hop transaction.
    let mut impossible = swaps.clone();
    let last = impossible.last_mut().unwrap();
    last.data[16..24].copy_from_slice(&u64::MAX.to_le_bytes());
    let before = world.token_balance(session.trader.pubkey(), tokens[0])?;
    assert!(
        world
            .send(
                vec![session.route(&impossible, &inputs)?],
                &[&session.trader]
            )
            .await
            .is_err()
    );
    assert_eq!(
        world.token_balance(session.trader.pubkey(), tokens[0])?,
        before
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires MTH_BACKRUN_FIXTURE"]
async fn full_fixture_restores_and_all_128_pools_swap() -> anyhow::Result<()> {
    let root = PathBuf::from(std::env::var("MTH_BACKRUN_FIXTURE")?);
    mth_backrun::build::verify(&root)?;
    let world = World::restore(&root.join("snapshot.json")).await?;
    world
        .surfnet
        .cheatcodes()
        .fund_sol(&world.payer().pubkey(), 1_000_000_000_000_000_000)?;
    world
        .surfnet
        .cheatcodes()
        .time_travel_to_slot_at(100_000, 1_800_000_000_000)?;
    let mut market: mth_backrun::fixture::Market =
        serde_json::from_slice(&std::fs::read(root.join("market.json"))?)?;
    market.hydrate();
    // Original provisioning wallet is private. Transfer test input from a fixed
    // user wallet, using actual SPL instructions; never overwrite pool reserves.
    let wallets: Vec<Vec<u8>> = serde_json::from_slice(&std::fs::read(root.join("users.json"))?)?;
    let user = solana_keypair::Keypair::try_from(wallets[0].as_slice())?;
    for mint in &market.tokens {
        let mint = mth_backrun::programs::key(mint);
        world.fund_tokens(world.payer().pubkey(), mint, 0).await?;
        world
            .send(
                vec![spl_token_interface::instruction::transfer(
                    &mth_backrun::programs::key(mth_backrun::programs::TOKEN),
                    &mth_backrun::programs::ata(user.pubkey(), mint),
                    &mth_backrun::programs::ata(world.payer().pubkey(), mint),
                    &user.pubkey(),
                    &[],
                    1_000_000_000_000,
                )?],
                &[&user],
            )
            .await?;
        if mint.to_string() != WSOL {
            let bytes = world.rpc.get_account_data(&mint)?;
            assert_eq!(&bytes[..4], &[0, 0, 0, 0], "mint authority revoked");
            assert_eq!(&bytes[46..50], &[0, 0, 0, 0], "freeze authority absent");
        }
    }
    for pool in &market.pools {
        let before = world.token_balance(
            world.payer().pubkey(),
            mth_backrun::programs::key(&market.tokens[pool.mints[1]]),
        )?;
        world.swap(pool, true, 1_000_000_000, 1).await?;
        assert!(
            world.token_balance(
                world.payer().pubkey(),
                mth_backrun::programs::key(&market.tokens[pool.mints[1]])
            )? > before
        );
        world.swap(pool, false, 1_000_000_000, 1).await?;
    }
    // Exercise the actual user-flow injector: successful swaps, a failed order,
    // a quiet round, and subsequent execution without retries or state resets.
    use mth_backrun::{
        fixture::{Leg, PrivateFlow, UserOrder},
        injector::SwapInjector,
    };
    let order = |slot, min_out, a_to_b| UserOrder {
        slot,
        wallet: 0,
        route: vec![Leg { pool: 0, a_to_b }],
        amount_in: 123_456_789,
        min_out,
    };
    let flow = PrivateFlow {
        version: 1,
        duration_slots: 2000,
        orders: vec![
            order(0, 1, true),
            order(0, u64::MAX, true),
            order(2, 1, false),
        ],
    };
    let users = vec![solana_keypair::Keypair::try_from(
        world.payer().to_bytes().as_slice(),
    )?];
    let mut injector = SwapInjector::new(&flow, &market)?;
    let fee_before = world.rpc.get_balance(&users[0].pubkey())?;
    let receipts = injector.inject_round(0, &users, &world, &market).await?;
    assert_eq!(
        receipts.iter().map(|r| r.order).collect::<Vec<_>>(),
        vec![0, 1]
    );
    assert!(receipts[0].receipt["meta"]["err"].is_null());
    assert!(
        !receipts[1].receipt["meta"]["err"].is_null(),
        "fixed slippage failure must be recorded"
    );
    assert_eq!(
        receipts[1].receipt["meta"]["preTokenBalances"],
        receipts[1].receipt["meta"]["postTokenBalances"]
    );
    let paid: u64 = receipts
        .iter()
        .map(|r| r.receipt["meta"]["fee"].as_u64().unwrap())
        .sum();
    assert_eq!(paid, 10_000);
    assert_eq!(
        world.rpc.get_balance(&users[0].pubkey())?,
        fee_before - paid
    );
    assert!(
        injector
            .inject_round(0, &users, &world, &market)
            .await
            .is_err()
    );
    assert!(
        injector
            .inject_round(2, &users, &world, &market)
            .await
            .is_err()
    );
    assert!(
        injector
            .inject_round(1, &users, &world, &market)
            .await?
            .is_empty()
    );
    let last = injector.inject_round(2, &users, &world, &market).await?;
    assert_eq!(last.len(), 1);
    assert_eq!(last[0].order, 2);
    assert!(last[0].receipt["meta"]["err"].is_null());
    // A missing signing key is an infrastructure error; it cannot silently
    // skip an order and continue into a later auction.
    let mut stopped = SwapInjector::new(&flow, &market)?;
    assert!(stopped.inject_round(0, &[], &world, &market).await.is_err());
    assert!(
        stopped
            .inject_round(0, &users, &world, &market)
            .await
            .is_err()
    );
    Ok(())
}
