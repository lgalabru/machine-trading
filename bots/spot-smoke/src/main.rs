//! The spot session smoke bot: on its first turn it sells 1 SOL at any
//! price on each spot market its mandate lists, then waits out the session.
//! Run with `buy`, it also buys 0.1 SOL on the first of them, which a
//! sell-only mandate forbids.

use std::sync::Arc;

use anyhow::{Context, Result};
use mtx_kit::{
    BotEnv, Decimal, Signer, Turns,
    chain::{ChainClient, FireAndForgetOptions, RpcChainClient, SendMode},
    core::Side,
    mandate::Kind,
    protocols::{
        NamedMarket,
        openbook::v2::{self as openbook_v2, OpenBookV2Order, OpenBookV2Trader},
        phoenix::v1::{self as phoenix_v1, PhoenixV1Order, PhoenixV1Trader},
    },
};
use tracing::{info, warn};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let buy = std::env::args().nth(1).as_deref() == Some("buy");
    let env = BotEnv::from_env()?;
    let mandate = env.mandate()?;
    let keypair = env.keypair()?;
    let owner = keypair.pubkey();
    let chain: Arc<dyn ChainClient> = Arc::new(RpcChainClient::new(env.rpc_url.clone()));
    // A stepped world lands a transaction in its next block: don't wait.
    let mode = SendMode::FireAndForget(FireAndForgetOptions { simulate: false });
    let one = Decimal::ONE;
    let mut turns = Turns::connect()?;
    let mut first = true;
    while let Some(step) = turns.next()? {
        if !first {
            continue;
        }
        first = false;
        for (i, allowance) in mandate.universe.iter().enumerate() {
            let instrument = &allowance.instrument;
            if instrument.kind != Kind::Spot {
                continue;
            }
            let market = |markets: &[NamedMarket]| {
                NamedMarket::find(markets, &instrument.symbol)
                    .copied()
                    .with_context(|| format!("no market for {instrument}"))
            };
            let sent = match instrument.venue.as_str() {
                "phoenix" => {
                    let named = market(phoenix_v1::MARKETS)?;
                    let trader = PhoenixV1Trader::connect(
                        chain.clone(),
                        named.address,
                        "SOL",
                        "USDC",
                        owner,
                    )
                    .await?
                    .with_send_mode(mode);
                    let mut orders = vec![(Side::Sell, one)];
                    if buy && i == 0 {
                        orders.push((Side::Buy, Decimal::new(1, 1)));
                    }
                    for (side, quantity) in orders {
                        let order = PhoenixV1Order::ImmediateOrCancel {
                            side,
                            quantity,
                            worst_price: None,
                            all_or_none: false,
                        };
                        trader.submit(&order, &keypair).await?;
                    }
                    true
                }
                "openbook" => {
                    let named = market(openbook_v2::MARKETS)?;
                    let trader = OpenBookV2Trader::connect(
                        chain.clone(),
                        named.address,
                        "SOL",
                        "USDC",
                        owner,
                    )
                    .await?
                    .with_send_mode(mode);
                    let tick = trader.market().instrument.tick_size;
                    let mut orders = vec![(Side::Sell, one, tick)];
                    if buy && i == 0 {
                        orders.push((
                            Side::Buy,
                            Decimal::new(1, 1),
                            tick * Decimal::from(1_000_000),
                        ));
                    }
                    for (side, quantity, worst_price) in orders {
                        let order = OpenBookV2Order::Take {
                            side,
                            quantity,
                            worst_price,
                        };
                        trader.submit(&order, &keypair).await?;
                    }
                    true
                }
                venue => {
                    warn!(venue, "not a spot venue this bot trades");
                    false
                }
            };
            if sent {
                info!(%instrument, step = step.k, buy, "sent");
            }
        }
    }
    Ok(())
}
