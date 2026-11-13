//! Stake pools on the committed LST snapshot, offline, with no clock but
//! the test's: staking and redeeming at once, Marinade's liquid unstake
//! fee, and an epoch boundary that pays the pools' rewards and lets the
//! delayed exits (a deactivated stake account, a Marinade ticket) pay out.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use mtx_core::{
    Holding,
    mandate::{InstrumentRef, Kind},
};
use mtx_grader::{
    epochs::{EpochEvent, EpochPlan, Epochs, LANDING_SLOTS, WARP_BLOCKS},
    guard::Guard,
    snapshot,
    venue::{
        marinade::{self, MarinadeState},
        stake_pool::{self, POOLS, PoolState, STAKE_ACCOUNT_LEN, stake_ix},
        stake_venue,
    },
};
use mtx_kit::{
    Action, Planned, SendState, VenueView,
    chain::{ChainClient, RpcChainClient, token},
};
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_rpc_client::rpc_client::RpcClient;
use solana_rpc_client_api::config::RpcSendTransactionConfig;
use solana_signer::Signer;
use solana_transaction::{Instruction, Transaction};
use surfpool_sdk::{CheatcodeFilter, SimnetCommand, Surfnet};

const SOL: u64 = 1_000_000_000;

fn snapshot_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../snapshots/lst-2026-11-12")
}

fn jito_pool() -> InstrumentRef {
    "jito-stake-pool:stake-pool:JITOSOL".parse().unwrap()
}

fn marinade_pool() -> InstrumentRef {
    "marinade:stake-pool:MSOL".parse().unwrap()
}

/// A world with no clock: blocks land when the test forwards a slot. It is
/// read through the grader's own proxy, as a session reads it: a
/// transaction is final once it executed.
struct World {
    surfnet: Surfnet,
    guard: Guard,
}

impl World {
    fn url(&self) -> &str {
        self.guard.url()
    }
}

async fn world() -> (World, RpcClient) {
    let (surfnet, _) = snapshot::start_snapshot_stepped(&snapshot_dir())
        .await
        .unwrap();
    let guard = Guard::internal(surfnet.rpc_url()).await.unwrap();
    let rpc = mtx_kit::rpc_client(guard.url());
    (World { surfnet, guard }, rpc)
}

async fn forward(surfnet: &Surfnet, rpc: &RpcClient, slots: u64) -> u64 {
    let mut slot = rpc.get_slot().unwrap();
    let to = slot + slots;
    while slot < to {
        surfnet
            .send_command(SimnetCommand::SlotForward(None))
            .unwrap();
        while rpc.get_slot().unwrap() <= slot {
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
        slot = rpc.get_slot().unwrap();
    }
    slot
}

/// Send `instructions` signed by `signers`, the first paying, let a block
/// land them, and say how it went: the program's error, if it refused.
async fn send(
    surfnet: &Surfnet,
    rpc: &RpcClient,
    signers: &[&Keypair],
    instructions: Vec<Instruction>,
) -> Result<(), String> {
    let tx = Transaction::new_signed_with_payer(
        &instructions,
        Some(&signers[0].pubkey()),
        signers,
        rpc.get_latest_blockhash().unwrap(),
    );
    let signature = rpc
        .send_transaction_with_config(
            &tx,
            RpcSendTransactionConfig {
                skip_preflight: true,
                ..RpcSendTransactionConfig::default()
            },
        )
        .map_err(|e| e.to_string())?;
    forward(surfnet, rpc, 1).await;
    match rpc.get_signature_status(&signature).unwrap() {
        Some(Ok(())) => Ok(()),
        Some(Err(error)) => Err(error.to_string()),
        None => Err("not landed".into()),
    }
}

fn funded(surfnet: &Surfnet, lamports: u64) -> Keypair {
    let wallet = Keypair::new();
    surfnet
        .cheatcodes()
        .fund_sol(&wallet.pubkey(), lamports)
        .unwrap();
    wallet
}

fn planned(action: Action) -> Planned {
    Planned {
        line: 0,
        venue: None,
        client_id: None,
        action,
        reduce_only: false,
        expires_slot: None,
        tx: None,
    }
}

fn balance(rpc: &RpcClient, account: &Pubkey) -> u64 {
    rpc.get_account(account)
        .map(|a| token::token_balance(&a).unwrap())
        .unwrap_or(0)
}

fn jito_state(rpc: &RpcClient) -> PoolState {
    PoolState::decode(&rpc.get_account(&POOLS[0].address).unwrap().data).unwrap()
}

fn marinade_state(rpc: &RpcClient) -> MarinadeState {
    MarinadeState::decode(&rpc.get_account(&marinade::STATE).unwrap().data).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn staking_mints_at_the_pool_s_rate_and_redeeming_stops_at_the_reserve() {
    let (world, rpc) = world().await;
    let surfnet = &world.surfnet;
    let wallet = funded(surfnet, 200 * SOL);
    // A holder of about 76 jitoSOL, staked before: the reserve doesn't keep
    // its SOL.
    let reserve = jito_state(&rpc).reserve_stake;
    let held_before = rpc.get_balance(&reserve).unwrap();
    stake_pool::stake_before_session(surfnet, &rpc, &wallet, &POOLS[0], 100 * SOL).unwrap();
    assert_eq!(rpc.get_balance(&reserve).unwrap(), held_before);
    let market = stake_venue("jito-stake-pool", world.url(), &EpochPlan::default())
        .unwrap()
        .market(&jito_pool())
        .await
        .unwrap();
    assert_eq!(market.kind(), Kind::StakePool);
    let seat = market
        .seat(wallet.insecure_clone(), world.url())
        .await
        .unwrap();
    let state = jito_state(&rpc);
    let tokens = token::ata(&wallet.pubkey(), &state.pool_mint);
    let start = u128::from(balance(&rpc, &tokens));

    // Staking 5 SOL mints what the program's own math gives.
    let sent = seat
        .send(&planned(Action::Deposit { amount: Some(5.0) }))
        .await;
    assert!(sent.sent, "{:?}", sent.state);
    forward(surfnet, &rpc, 1).await;
    let minted = u128::from(5 * SOL) * u128::from(state.pool_token_supply)
        / u128::from(state.total_lamports);
    assert_eq!(u128::from(balance(&rpc, &tokens)) - start, minted);

    // The reserve pays a small redemption at once, at the rate less the
    // withdrawal fee.
    let (bid, ask) = seat.top().await.unwrap();
    let rate: f64 = jito_state(&rpc).rate().to_string().parse().unwrap();
    assert!((ask.unwrap() - rate).abs() < 1e-12, "no deposit fee");
    assert!(
        (bid.unwrap() - rate * 0.999).abs() < 1e-12,
        "a 0.1% withdrawal fee"
    );
    let before = rpc.get_balance(&wallet.pubkey()).unwrap();
    let sent = seat
        .send(&planned(Action::Withdraw { amount: Some(1.0) }))
        .await;
    assert!(sent.sent, "{:?}", sent.state);
    forward(surfnet, &rpc, 1).await;
    let paid = rpc.get_balance(&wallet.pubkey()).unwrap() + 5_000 - before;
    let expected = rate * 0.999 * 1e9;
    assert!(
        (paid as f64 - expected).abs() < 2.0,
        "{paid} lamports for 1 token, not {expected}"
    );

    // Redeeming more than the reserve holds fails on chain, with the
    // program's reason (SolWithdrawalTooLarge, 35), and moves nothing.
    let stats = seat.stats().await.unwrap();
    let capacity = stats
        .iter()
        .find(|(name, _)| name == "instant_capacity_sol")
        .unwrap()
        .1;
    assert!(capacity < 30.0 * rate, "the reserve holds {capacity} SOL");
    let held = balance(&rpc, &tokens);
    let sent = seat
        .send(&planned(Action::Withdraw { amount: Some(30.0) }))
        .await;
    forward(surfnet, &rpc, 1).await;
    match sent.state {
        SendState::Failed { reason, .. } => {
            assert!(reason.contains("custom program error: 0x23"), "{reason}")
        }
        other => panic!("a redemption past the reserve came back {other:?}"),
    }
    assert_eq!(balance(&rpc, &tokens), held);

    // The token is worth the pool's rate in SOL, at the Pyth price.
    let chain: Arc<dyn ChainClient> = Arc::new(RpcChainClient::new(world.url()));
    let holdings = market.holdings(&wallet.pubkey()).await.unwrap();
    let Holding::Spot(token_held) = &holdings[0] else {
        panic!("{holdings:?}")
    };
    let value = market
        .value_usd(chain.as_ref(), &holdings[..1])
        .await
        .unwrap()
        .unwrap();
    let sol_usd = mtx_grader::venue::price_usd(chain.as_ref(), &token::native_mint())
        .await
        .unwrap();
    assert_eq!(
        value,
        token_held.total() * jito_state(&rpc).rate() * sol_usd
    );
    assert!(
        market
            .value_usd(chain.as_ref(), &holdings[1..])
            .await
            .unwrap()
            .is_none(),
        "SOL is valued at its reference price"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn marinade_s_liquid_unstake_costs_more_the_more_it_drains() {
    let (world, rpc) = world().await;
    let surfnet = &world.surfnet;
    let wallet = funded(surfnet, 1_000 * SOL);
    let market = stake_venue("marinade", world.url(), &EpochPlan::default())
        .unwrap()
        .market(&marinade_pool())
        .await
        .unwrap();
    let seat = market
        .seat(wallet.insecure_clone(), world.url())
        .await
        .unwrap();
    let sent = seat
        .send(&planned(Action::Deposit {
            amount: Some(900.0),
        }))
        .await;
    assert!(sent.sent, "{:?}", sent.state);
    forward(surfnet, &rpc, 1).await;
    let rate: f64 = marinade_state(&rpc).rate().to_string().parse().unwrap();
    let mut fees = Vec::new();
    for msol in [1.0, 500.0] {
        let before = rpc.get_balance(&wallet.pubkey()).unwrap();
        let sent = seat
            .send(&planned(Action::Withdraw { amount: Some(msol) }))
            .await;
        assert!(sent.sent, "{:?}", sent.state);
        forward(surfnet, &rpc, 1).await;
        let paid = (rpc.get_balance(&wallet.pubkey()).unwrap() + 5_000 - before) as f64 / 1e9;
        fees.push(1.0 - paid / (msol * rate));
    }
    assert!(fees[0] > 0.0 && fees[1] > fees[0], "fees {fees:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn each_pool_s_view_reports_its_own_token_and_the_session_s_boundary() {
    let (world, rpc) = world().await;
    let surfnet = &world.surfnet;
    let wallet = funded(surfnet, 200 * SOL);
    let crank = funded(surfnet, 10 * SOL);
    let plan = EpochPlan::default();
    let pools = [jito_pool(), marinade_pool()];
    let events = [EpochEvent {
        step: 3,
        reward_bps: 1.4,
    }];
    Epochs::schedule(&rpc, &events, &pools, rpc.get_slot().unwrap(), crank, &plan).unwrap();
    surfnet.cheatcodes().lock(CheatcodeFilter::all()).unwrap();
    let mut seats = Vec::new();
    for pool in &pools {
        let market = stake_venue(&pool.venue, world.url(), &plan)
            .unwrap()
            .market(pool)
            .await
            .unwrap();
        seats.push(
            market
                .seat(wallet.insecure_clone(), world.url())
                .await
                .unwrap(),
        );
    }
    // Different amounts, so a view that read the wrong pool's token shows.
    for (seat, sol) in seats.iter().zip([60.0, 25.0]) {
        let sent = seat
            .send(&planned(Action::Deposit { amount: Some(sol) }))
            .await;
        assert!(sent.sent, "{:?}", sent.state);
    }
    forward(surfnet, &rpc, 1).await;
    let owner = wallet.pubkey();
    let mints = [jito_state(&rpc).pool_mint, marinade_state(&rpc).msol_mint];
    let mut positions = Vec::new();
    for (seat, mint) in seats.iter().zip(mints) {
        let held = balance(&rpc, &token::ata(&owner, &mint)) as f64 / 1e9;
        assert!(held > 0.0);
        // The session's reader puts the first pool's token there for every
        // market; each seat corrects it to its own.
        let mut view = VenueView {
            id: seat.market().to_string(),
            position: Some(mtx_kit::Position {
                size: 999.0,
                entry_usd: 150.0,
            }),
            ..VenueView::default()
        };
        seat.view(&mut view).await.unwrap();
        let position = view.position.unwrap();
        assert_eq!(position.size, held, "{}", view.id);
        assert_eq!(position.entry_usd, 150.0);
        let stat = |name: &str| view.stats.iter().find(|(n, _)| n == name).map(|(_, v)| *v);
        assert!(stat("rate").unwrap() > 1.0);
        assert_eq!(stat("slots_to_epoch_end"), plan.next(rpc.get_slot().unwrap()).map(|(d, _)| d as f64));
        assert_eq!(
            stat("next_epoch"),
            Some((rpc.get_epoch_info().unwrap().epoch + 1) as f64)
        );
        positions.push(position.size);
    }
    assert_ne!(positions[0], positions[1]);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_epoch_boundary_pays_the_rewards_and_frees_the_delayed_exits() {
    let (world, rpc) = world().await;
    let surfnet = &world.surfnet;
    let wallet = funded(surfnet, 100 * SOL);
    let crank = funded(surfnet, 10 * SOL);
    let owner = wallet.pubkey();
    let jito = POOLS[0];
    let state = jito_state(&rpc);
    let tokens = token::ata(&owner, &state.pool_mint);
    let mstate = marinade_state(&rpc);
    let msol = token::ata(&owner, &mstate.msol_mint);
    send(
        surfnet,
        &rpc,
        &[&wallet],
        vec![
            token::create_ata_idempotent(&owner, &owner, &state.pool_mint),
            stake_pool::ix::deposit_sol(
                &jito.program,
                &jito.address,
                &state,
                &owner,
                &tokens,
                20 * SOL,
            ),
            token::create_ata_idempotent(&owner, &owner, &mstate.msol_mint),
            marinade::ix::deposit(&mstate, &owner, &msol, 20 * SOL),
        ],
    )
    .await
    .unwrap();

    // The delayed exits: a stake account split off a validator and
    // deactivated, and a Marinade ticket.
    let list =
        stake_pool::decode_validator_list(&rpc.get_account(&state.validator_list).unwrap().data)
            .unwrap();
    let in_world = list
        .iter()
        .map(|v| {
            stake_pool::validator_stake_address(
                &jito.program,
                &jito.address,
                &v.vote_account,
                v.validator_seed_suffix,
            )
        })
        .find(|address| rpc.get_account(address).is_ok())
        .expect("the snapshot holds a validator's stake account");
    let stake = Keypair::new();
    let rent = rpc
        .get_minimum_balance_for_rent_exemption(STAKE_ACCOUNT_LEN as usize)
        .unwrap();
    send(
        surfnet,
        &rpc,
        &[&wallet, &stake],
        vec![
            stake_ix::allocate(&owner, &stake.pubkey(), rent),
            stake_pool::ix::withdraw_stake(
                &jito.program,
                &jito.address,
                &state,
                &in_world,
                &stake.pubkey(),
                &owner,
                &owner,
                &tokens,
                5 * SOL,
            ),
            stake_ix::deactivate(&stake.pubkey(), &owner),
        ],
    )
    .await
    .unwrap();
    let ticket = Keypair::new();
    let ticket_rent = rpc
        .get_minimum_balance_for_rent_exemption(marinade::TICKET_LEN as usize)
        .unwrap();
    send(
        surfnet,
        &rpc,
        &[&wallet, &ticket],
        vec![
            marinade::ix::allocate_ticket(&owner, &ticket.pubkey(), ticket_rent),
            marinade::ix::order_unstake(&mstate, &owner, &msol, &ticket.pubkey(), 5 * SOL),
        ],
    )
    .await
    .unwrap();
    let chain = RpcChainClient::new(world.url());
    let waiting = marinade::tickets(&chain, &owner).await.unwrap();
    assert_eq!(waiting.len(), 1);
    let staked = rpc.get_account(&stake.pubkey()).unwrap().lamports;
    assert_eq!(
        stake_pool::stake_accounts(&chain, &owner).await.unwrap(),
        staked,
        "the wallet's one stake account, counted once"
    );

    // Neither pays before the epoch ends.
    let claim = marinade::ix::claim(&ticket.pubkey(), &owner);
    assert!(
        send(surfnet, &rpc, &[&wallet], vec![claim.clone()])
            .await
            .is_err()
    );
    let withdraw = stake_ix::withdraw(&stake.pubkey(), &owner, &owner, staked);
    assert!(
        send(surfnet, &rpc, &[&wallet], vec![withdraw.clone()])
            .await
            .is_err()
    );

    // A boundary after step 3, planned from where step 0 starts, with the
    // cheatcodes locked as a session's are.
    let base = rpc.get_slot().unwrap();
    let events = [EpochEvent {
        step: 3,
        reward_bps: 1.4,
    }];
    let plan = EpochPlan::default();
    let (mut epochs, added) = Epochs::schedule(
        &rpc,
        &events,
        &[jito_pool(), marinade_pool()],
        base,
        crank,
        &plan,
    )
    .unwrap();
    surfnet.cheatcodes().lock(CheatcodeFilter::all()).unwrap();
    let epoch_before = rpc.get_epoch_info().unwrap().epoch;
    // The seats report the session's boundary, not the schedule's own: the
    // distance to it runs out exactly where the clock jumps.
    let (distance, next) = plan.next(base).unwrap();
    assert_eq!(next, epoch_before + 1);
    let slot = forward(surfnet, &rpc, distance - 2).await;
    assert_eq!(plan.next(slot), Some((2, next)));
    let slot = forward(surfnet, &rpc, 2).await;
    for k in 0..3 {
        assert_eq!(epochs.warp(surfnet, &rpc, k, slot).await.unwrap(), slot);
    }
    let landed = epochs.warp(surfnet, &rpc, 3, slot).await.unwrap();
    assert_eq!(landed, slot + added, "the plan's slots");
    let info = rpc.get_epoch_info().unwrap();
    assert_eq!(info.epoch, epoch_before + 1);
    assert_eq!(info.slot_index, LANDING_SLOTS + WARP_BLOCKS);
    assert_eq!(info.epoch, next, "the epoch the plan named");
    assert_eq!(plan.next(landed), None, "no boundary left");
    let record = &epochs.records()[0];
    assert!(record.refused.is_empty(), "{:?}", record.refused);

    // Jito's rate moved by the reward less its 4% epoch fee; Marinade's by
    // the reward on its active stake.
    let jito_rate = &record.rates[0];
    let step = jito_rate.after / jito_rate.before - 1.0;
    assert!(
        (step - 1.4e-4 * 0.96).abs() < 1e-9,
        "Jito's rate moved {step}"
    );
    let after = jito_state(&rpc);
    assert_eq!(after.last_update_epoch, info.epoch);
    let marinade_rate = &record.rates[1];
    let active_share =
        mstate.total_active_balance as f64 / mstate.total_virtual_staked_lamports() as f64;
    let step = marinade_rate.after / marinade_rate.before - 1.0;
    assert!(
        (step - 1.4e-4 * active_share).abs() < 1e-8,
        "Marinade's rate moved {step}"
    );

    // The pool takes deposits again, and both delayed exits pay.
    let before = rpc.get_balance(&owner).unwrap();
    send(
        surfnet,
        &rpc,
        &[&wallet],
        vec![stake_pool::ix::deposit_sol(
            &jito.program,
            &jito.address,
            &after,
            &owner,
            &tokens,
            SOL,
        )],
    )
    .await
    .unwrap();
    send(surfnet, &rpc, &[&wallet], vec![claim]).await.unwrap();
    send(surfnet, &rpc, &[&wallet], vec![withdraw])
        .await
        .unwrap();
    let gained = rpc.get_balance(&owner).unwrap() as i128 - before as i128;
    let expected =
        i128::from(waiting[0].lamports + ticket_rent + staked) - i128::from(SOL) - 3 * 5_000;
    assert_eq!(gained, expected);
    assert!(marinade::tickets(&chain, &owner).await.unwrap().is_empty());
}
