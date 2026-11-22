//! An atomic group on a Surfpool fork of mainnet: borrow SOL from Kamino's
//! main market by flash loan, sell it for USDC on Orca's SOL/USDC
//! Whirlpool, buy SOL back with the USDC on Raydium's SOL/USDC CLMM, and
//! repay the loan, in one transaction (`desk::atomic`).
//!
//! The pools are usually within their fees of each other, so the cycle
//! loses a little. With the last swap's limit at the loan and its fee (a
//! route that must pay for itself), Raydium refuses the swap and nothing
//! happens: not the loan, not the first swap. With a loose limit the whole
//! route lands, the trader's own SOL paying for the loss and the fee, and
//! its USDC ends where it started. A price push for the reserve, written
//! after a swap, goes to the head of its transaction, where Scope takes it.
//!
//! ```text
//! surfpool start --rpc-url <mainnet> --port 38899 --no-tui --no-deploy --yes
//! MTX_SURFNET_URL=http://127.0.0.1:38899 cargo test -p mtx-runtime \
//!   --test atomic_surfnet -- --ignored --nocapture
//! ```

use std::{sync::Arc, time::Duration};

use mtx_core::mandate::{InstrumentRef, Kind};
use mtx_kit::{
    Action, AmountUnit, Planned, SendState, Side, TxOptions,
    chain::{ChainClient, RpcChainClient, token},
    protocols::{USDC_MINT, orca::whirlpool, raydium::clmm},
    token::TOKEN_PROGRAM_ID,
};
use mtx_runtime::{
    desk::atomic,
    venue::{
        Seat, Venue,
        amm::{Amm, Pool, PoolSeat, State, SwapOrder},
        kamino::Kamino,
    },
};
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_rpc_client_api::request::RpcRequest;
use solana_signer::Signer as _;

const WRAPPED_SOL_MINT: Pubkey =
    solana_pubkey::pubkey!("So11111111111111111111111111111111111111112");
/// The loan, in SOL, and what the trader holds to pay a losing route's
/// shortfall and the flash fee: a loss of a few basis points on 10 SOL.
const LOAN_SOL: f64 = 10.0;
const OWN_WSOL_ATOMS: u64 = 1_000_000_000;
const LAMPORTS_PER_SOL: f64 = 1e9;
/// Polls for a sent group's balances: a fork lands a transaction within a
/// slot or two, 400 ms each.
const LANDING_POLLS_MAX: u32 = 40;
const LANDING_POLL_INTERVAL: Duration = Duration::from_millis(250);

#[tokio::test]
#[ignore = "needs a Surfpool fork of mainnet at MTX_SURFNET_URL"]
async fn a_flash_funded_cycle_lands_whole_or_not_at_all() {
    let url = std::env::var("MTX_SURFNET_URL").expect("MTX_SURFNET_URL");
    let client = Arc::new(RpcChainClient::new(&url));
    let trader = Keypair::new();
    fund(&client, &trader.pubkey()).await;
    let chain: Arc<dyn ChainClient> = client.clone();

    let reserve = InstrumentRef {
        venue: "kamino".into(),
        kind: Kind::MoneyMarket,
        symbol: "SOL".into(),
    };
    let kamino = Kamino::connect(&url, std::slice::from_ref(&reserve))
        .await
        .expect("Kamino's SOL reserve");
    let lender = kamino
        .market(&reserve)
        .await
        .expect("the reserve")
        .seat(trader.insecure_clone(), &url)
        .await
        .expect("a seat");
    let (orca, orca_seat) = pool(&chain, "orca", whirlpool::SOL_USDC_POOL, &trader).await;
    let (raydium, raydium_seat) = pool(&chain, "raydium", clmm::SOL_USDC_POOL, &trader).await;
    let seats: Vec<Box<dyn Seat>> = vec![lender, Box::new(orca_seat), Box::new(raydium_seat)];

    // What the first swap gets, to the atom: the second spends all of it.
    let sell =
        SwapOrder::from_swap(&orca, Side::Ask, LOAN_SOL, AmountUnit::Base, 1.0).expect("an order");
    let usdc_atoms = State::read(chain.as_ref(), &orca)
        .await
        .expect("Orca's state")
        .quote(&orca.address, sell.direction, sell.mode, sell.amount)
        .expect("a quote")
        .amount_out;
    let usdc = usdc_atoms as f64 / 1e6;
    println!("{LOAN_SOL} SOL sell for {usdc} USDC on Orca");

    // A limit that makes the route pay for its loan and a 1% margin.
    let must_pay = usdc / (LOAN_SOL * 1.01);
    let before = balances(&chain, &trader.pubkey()).await;
    let state = send(&seats, &route(&orca, &raydium, usdc, must_pay)).await;
    let SendState::Failed { reason, .. } = &state else {
        panic!("a route that can't pay for itself must fail: {state:?}")
    };
    println!("tight limit: {reason}");
    // Raydium's own limit check (`swap_v2.rs`), on the instruction after
    // the loan and the first swap.
    assert!(
        reason.contains(&clmm::PROGRAM_ID.to_string())
            && reason.contains("TooLittleOutputReceived"),
        "Raydium's swap refuses it: {reason}"
    );
    settle().await;
    assert_eq!(
        balances(&chain, &trader.pubkey()).await,
        before,
        "nothing of the route happened"
    );

    // A loose limit: the route lands whole.
    let loose = route(&orca, &raydium, usdc, usdc / LOAN_SOL * 1.05);
    let state = send(&seats, &loose).await;
    assert!(
        matches!(state, SendState::Landed(_) | SendState::Delayed),
        "a loose route lands: {state:?}"
    );
    let after = landed(&chain, &trader.pubkey(), before).await;
    let cost_sol = (before.0 - after.0) as f64 / LAMPORTS_PER_SOL;
    println!("loose limit: the cycle and the flash fee cost {cost_sol} SOL");
    assert_eq!(
        after.1, before.1,
        "every USDC the first swap got, the second spent"
    );
    assert!(
        cost_sol > 0.0 && cost_sol < 0.1,
        "the loss and the fee, a few basis points of the loan: {cost_sol} SOL"
    );

    // A price push written after a swap goes first, where Scope takes it.
    // (With the route above it outgrows a transaction without a lookup
    // table of the pools' accounts.)
    let sale = Planned {
        line: 0,
        action: Action::Swap {
            side: Side::Ask,
            amount: 0.1,
            unit: AmountUnit::Base,
            limit: 1.0,
        },
        ..loose[1].clone()
    };
    let push = Planned {
        line: 1,
        action: Action::PushPrice,
        ..loose[0].clone()
    };
    let state = send(&seats, &[sale, push]).await;
    assert!(
        matches!(state, SendState::Landed(_) | SendState::Delayed),
        "a push and a swap land: {state:?}"
    );
    let pushed = landed(&chain, &trader.pubkey(), after).await;
    assert!(pushed.1 > after.1, "the sale got USDC");
}

/// The route, an atomic group in sheet order: the loan, the sale on Orca,
/// and the purchase on Raydium with `usdc`, at no more than `limit` USDC a
/// SOL.
fn route(orca: &Pool, raydium: &Pool, usdc: f64, limit: f64) -> Vec<Planned> {
    let line = |line: usize, venue: String, action: Action| Planned {
        line,
        venue: Some(venue),
        client_id: None,
        action,
        reduce_only: false,
        expires_slot: None,
        tx: Some(TxOptions {
            atomic: Some(1),
            ..TxOptions::default()
        }),
    };
    vec![
        line(
            0,
            "kamino:money-market:SOL".into(),
            Action::Flash { amount: LOAN_SOL },
        ),
        line(
            1,
            orca.instrument.to_string(),
            Action::Swap {
                side: Side::Ask,
                amount: LOAN_SOL,
                unit: AmountUnit::Base,
                limit: 1.0,
            },
        ),
        line(
            2,
            raydium.instrument.to_string(),
            Action::Swap {
                side: Side::Bid,
                amount: usdc,
                unit: AmountUnit::Quote,
                limit,
            },
        ),
    ]
}

/// Build the group and send it through its first line's seat.
async fn send(seats: &[Box<dyn Seat>], group: &[Planned]) -> SendState {
    let seat = |venue: Option<&str>| {
        seats
            .iter()
            .find(|s| Some(s.market()) == venue)
            .map(|s| s.as_ref())
    };
    let (plan, sender) = atomic::build(group, seat, None)
        .await
        .expect("the group builds");
    println!(
        "{}: {} instructions",
        plan.label(),
        plan.all_instructions().len()
    );
    sender.send_plan(&plan).await.state
}

async fn pool(
    chain: &Arc<dyn ChainClient>,
    venue: &str,
    address: Pubkey,
    trader: &Keypair,
) -> (Pool, PoolSeat) {
    let instrument = InstrumentRef {
        venue: venue.into(),
        kind: Kind::Amm,
        symbol: address.to_string(),
    };
    let amm = Amm::connect(venue, chain.clone(), std::slice::from_ref(&instrument))
        .await
        .expect("venue");
    let pool = amm.pools()[0].clone();
    let seat = PoolSeat::new(pool.clone(), chain.clone(), trader.insecure_clone());
    (pool, seat)
}

/// The trader's wrapped SOL and USDC, in atoms.
async fn balances(chain: &Arc<dyn ChainClient>, owner: &Pubkey) -> (u64, u64) {
    let mut out = [0u64; 2];
    for (held, mint) in out.iter_mut().zip([WRAPPED_SOL_MINT, USDC_MINT]) {
        *held = token::owned_amount(chain.as_ref(), owner, &mint)
            .await
            .expect("a balance");
    }
    (out[0], out[1])
}

/// The balances once they differ from `before`: the group landed.
async fn landed(chain: &Arc<dyn ChainClient>, owner: &Pubkey, before: (u64, u64)) -> (u64, u64) {
    for _ in 0..LANDING_POLLS_MAX {
        let now = balances(chain, owner).await;
        if now != before {
            return now;
        }
        tokio::time::sleep(LANDING_POLL_INTERVAL).await;
    }
    panic!("the group didn't land within {LANDING_POLLS_MAX} polls");
}

/// Long enough for a transaction sent to land: a refused one never is.
async fn settle() {
    tokio::time::sleep(LANDING_POLL_INTERVAL * 8).await;
}

/// Gives `owner` SOL for fees, a little wrapped SOL toward the loan's fee
/// and the route's loss, and an empty USDC account.
async fn fund(client: &RpcChainClient, owner: &Pubkey) {
    cheat(
        client,
        "surfnet_setAccount",
        serde_json::json!([owner.to_string(), {"lamports": 10_000_000_000u64}]),
    )
    .await;
    for (mint, amount) in [(WRAPPED_SOL_MINT, OWN_WSOL_ATOMS), (USDC_MINT, 0)] {
        cheat(
            client,
            "surfnet_setTokenAccount",
            serde_json::json!([owner.to_string(), mint.to_string(), {"amount": amount}, TOKEN_PROGRAM_ID.to_string()]),
        )
        .await;
    }
}

async fn cheat(client: &RpcChainClient, method: &'static str, params: serde_json::Value) {
    let _: serde_json::Value = client
        .rpc()
        .send(RpcRequest::Custom { method }, params)
        .await
        .unwrap_or_else(|err| panic!("{method}: {err:?}"));
}
