//! Grader-side Phoenix setup: metadata snapshots, trader onboarding, and
//! scenario overrides that move the market. Bots use [`mth_botkit::Phoenix`].

use std::time::Duration;

pub use mth_botkit::{Phoenix, load_exchange};
use phoenix_rise::{api::PhoenixHttpClient, types::exchange::ExchangeResponse};
use serde_json::json;
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use surfpool_sdk::Surfnet;
use tracing::info;

use crate::{
    Result,
    chain::{self, Override},
};

pub const DEFAULT_API_URL: &str = "https://perp-api.phoenix.trade";

/// Deposits take USDC from the wallet and wrap it into Phoenix's canonical
/// mint (`PhUsd…`), so the wallet must hold real USDC, not the canonical mint.
pub const USDC_MINT: Pubkey =
    solana_pubkey::pubkey!("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");

/// Trader capabilities a fresh account needs before it can trade on a fork of
/// mainnet, where Phoenix Eternal gates new traders.
const TRADER_CAPABILITIES: [&str; 6] = [
    "PlaceLimitOrder",
    "PlaceMarketOrder",
    "RiskIncreasingTrade",
    "RiskReducingTrade",
    "DepositCollateral",
    "WithdrawCollateral",
];

/// USDC left in the wallet on top of the deposit, so withdrawals and token
/// account rent never run short.
const WALLET_USDC_BUFFER: f64 = 100.0;

pub async fn fetch_exchange(api_url: &str) -> Result<ExchangeResponse> {
    Ok(PhoenixHttpClient::new(api_url)?.get_exchange().await?)
}

/// Fund, register, enable and capitalise a fresh cross-margin trader.
/// Returns the trader account (PDA).
pub async fn onboard_trader(
    surfnet: &Surfnet,
    phoenix: &Phoenix,
    authority: &Keypair,
    deposit_usdc: f64,
) -> Result<Pubkey> {
    let rpc = chain::rpc_client(surfnet);
    let owner = authority.pubkey();
    let trader = phoenix.trader_account(&owner);
    let cheats = surfnet.cheatcodes();
    cheats.fund_sol(&owner, 10_000_000_000)?;
    // USDC has 6 decimals.
    let usdc_units = ((deposit_usdc + WALLET_USDC_BUFFER) * 1e6) as u64;
    cheats.fund_token(&owner, &USDC_MINT, usdc_units, None)?;

    let register = phoenix.tx().build_register_trader(owner, 0, 0)?;
    mth_botkit::send(&rpc, authority, register, "register trader")?;

    let capabilities: serde_json::Map<_, _> = TRADER_CAPABILITIES
        .iter()
        .map(|name| (name.to_string(), json!("true")))
        .collect();
    chain::register_scenario(
        &rpc,
        "enable trader",
        vec![Override {
            template_id: "phoenix-trader-capabilities",
            account: trader,
            values: capabilities.into(),
            slot: 1,
            label: "enable trader".into(),
        }],
    )?;
    // Capabilities land a slot after registration; give it a few.
    tokio::time::sleep(Duration::from_secs(2)).await;

    let deposit = phoenix
        .tx()
        .build_deposit_funds(owner, trader, deposit_usdc)?;
    mth_botkit::send(&rpc, authority, deposit, "deposit collateral")?;
    info!(%owner, %trader, deposit_usdc, "trader onboarded");
    Ok(trader)
}

/// Phoenix rejects an oracle update more than 5% away from the oracle median,
/// so a larger move has to arrive as a sequence of smaller ones.
const MAX_MOVE_STEP_PCT: f64 = 4.0;
pub const SLOTS_BETWEEN_STEPS: u64 = 2;

/// Overrides that walk `symbol` from `from_ticks` to `to_ticks` in steps
/// Phoenix accepts, starting at `start_slot`, then uncross the book the move
/// crossed. Returns the overrides and the slot of the final uncross.
pub fn market_move(
    perp_asset_map: Pubkey,
    symbol: &str,
    from_ticks: u64,
    to_ticks: u64,
    start_slot: u64,
) -> (Vec<Override>, u64) {
    let total_pct = (to_ticks as f64 / from_ticks as f64 - 1.0) * 100.0;
    let steps = ((total_pct.abs() / MAX_MOVE_STEP_PCT).ceil() as u64).max(1);
    let mut overrides = Vec::new();
    let mut slot = start_slot;
    for step in 1..=steps {
        let ticks =
            from_ticks as f64 + (to_ticks as f64 - from_ticks as f64) * step as f64 / steps as f64;
        overrides.push(Override {
            template_id: "phoenix-market-move",
            account: perp_asset_map,
            values: json!({ "symbol": symbol, "target_ticks": (ticks.round() as u64).to_string() }),
            slot,
            label: format!("{symbol} step {step}/{steps}"),
        });
        slot += SLOTS_BETWEEN_STEPS;
    }
    overrides.push(Override {
        template_id: "phoenix-market-uncross",
        account: perp_asset_map,
        values: json!({ "symbol": symbol }),
        slot,
        label: "uncross".into(),
    });
    (overrides, slot)
}
