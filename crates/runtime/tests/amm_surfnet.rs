//! The `amm` venue's seat on a Surfpool fork of mainnet: each swap it sends
//! must move the trader's two wallet balances by exactly what the pool's
//! state quoted just before, and show up among the pool's fills as the
//! trader's.
//!
//! ```text
//! surfpool start --rpc-url <mainnet> --port 38899 --no-tui --no-deploy --yes
//! MTX_SURFNET_URL=http://127.0.0.1:38899 cargo test -p mtx-runtime \
//!   --test amm_surfnet -- --ignored --nocapture
//! ```

use std::{sync::Arc, time::Duration};

use mtx_core::{
    Decimal, Holding, Side as CoreSide,
    mandate::{InstrumentRef, Kind},
};
use mtx_kit::{
    Action, AmountUnit, Planned, Side,
    chain::{ChainClient, RpcChainClient},
    protocols::{USDC_MINT, orca::whirlpool, raydium::clmm},
    token::TOKEN_PROGRAM_ID,
};
use mtx_runtime::venue::{
    Market, Seat, Venue,
    amm::{Amm, PoolSeat, State, SwapOrder},
};
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_rpc_client_api::request::RpcRequest;
use solana_signer::Signer as _;

const WRAPPED_SOL_MINT: Pubkey =
    solana_pubkey::pubkey!("So11111111111111111111111111111111111111112");
/// Polls for a sent swap's balances: a fork lands a transaction within a
/// slot or two, 400 ms each.
const LANDING_POLLS_MAX: u32 = 40;
const LANDING_POLL_INTERVAL: Duration = Duration::from_millis(250);

#[tokio::test]
#[ignore = "needs a Surfpool fork of mainnet at MTX_SURFNET_URL"]
async fn a_seat_swaps_on_a_mainnet_fork_exactly_as_quoted() {
    let url = std::env::var("MTX_SURFNET_URL").expect("MTX_SURFNET_URL");
    let client = Arc::new(RpcChainClient::new(&url));
    let trader = Keypair::new();
    fund(&client, &trader.pubkey()).await;
    let chain: Arc<dyn ChainClient> = client.clone();
    let swaps = [
        (Side::Ask, 1.5, AmountUnit::Base),
        (Side::Bid, 200.0, AmountUnit::Quote),
        (Side::Bid, 0.75, AmountUnit::Base),
        (Side::Ask, 100.0, AmountUnit::Quote),
    ];
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
        let first_slot = chain.get_slot().await.expect("slot");
        for (side, amount, unit) in swaps {
            let (bid, ask) = market.top().await.expect("top");
            // A limit 5% away from the top, so the swap fills.
            let limit = match side {
                Side::Bid => bid.expect("a bid") * 1.05,
                Side::Ask => ask.expect("an ask") * 0.95,
            };
            let order = SwapOrder::from_swap(&pool, side, amount, unit, limit).expect("order");
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
                        side,
                        amount,
                        unit,
                        limit,
                    },
                    reduce_only: false,
                    expires_slot: None,
                    tx: None,
                })
                .await;
            assert!(sent.sent, "{venue} {side:?} {amount} {unit:?}: {sent:?}");
            let after = landed(&*market, &trader.pubkey(), before).await;
            let (amount_in, amount_out) =
                (i128::from(quoted.amount_in), i128::from(quoted.amount_out));
            let expected = match side {
                Side::Ask => (-amount_in, amount_out),
                Side::Bid => (amount_out, -amount_in),
            };
            assert_eq!(
                (after.0 - before.0, after.1 - before.1),
                expected,
                "{venue} {side:?} {amount} {unit:?}: base and quote atoms"
            );
            println!(
                "{venue} {side:?} {amount} {unit:?} at {limit:.4}: in {} out {} -> matched",
                quoted.amount_in, quoted.amount_out
            );
        }
        let fills = market
            .fills(first_slot.saturating_sub(1), &trader.pubkey())
            .await
            .expect("fills");
        let own: Vec<CoreSide> = fills
            .iter()
            .filter(|(fill, _)| fill.taker == trader.pubkey())
            .filter_map(|(fill, ids)| fill.side_for(ids))
            .collect();
        assert_eq!(
            own,
            [CoreSide::Sell, CoreSide::Buy, CoreSide::Buy, CoreSide::Sell],
            "{venue}: the trader's fills, in order"
        );
    }
}

/// Gives `owner` SOL for fees and wrapped SOL and USDC to swap.
async fn fund(client: &RpcChainClient, owner: &Pubkey) {
    cheat(
        client,
        "surfnet_setAccount",
        serde_json::json!([owner.to_string(), {"lamports": 10_000_000_000u64}]),
    )
    .await;
    for (mint, amount) in [
        (WRAPPED_SOL_MINT, 100_000_000_000u64),
        (USDC_MINT, 20_000_000_000u64),
    ] {
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
    for _ in 0..LANDING_POLLS_MAX {
        let now = atoms(market, owner).await;
        if now != before {
            return now;
        }
        tokio::time::sleep(LANDING_POLL_INTERVAL).await;
    }
    panic!("the swap didn't land within {LANDING_POLLS_MAX} polls");
}
