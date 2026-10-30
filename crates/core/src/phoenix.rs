//! Grader-side Phoenix setup: metadata snapshots, trader onboarding, and
//! scenario overrides that move the market. Bots use [`mth_botkit::Phoenix`].

use std::time::Duration;

pub use mth_botkit::{Phoenix, load_exchange};
use phoenix_rise::{
    accounts::spline_collection::{SPLINE_REGION_CAPACITY, SplineCollection},
    api::PhoenixHttpClient,
    types::exchange::ExchangeResponse,
};
use serde_json::json;
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use surfpool_sdk::Surfnet;
use tracing::{info, warn};

use crate::{
    Error, Result,
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
    // Capabilities land a slot after registration. Under load (a session
    // onboards the bot, eight takers and a competitor at once) that slot can
    // come late: until it does, Phoenix refuses the deposit as frozen, so
    // retry rather than guess how long to wait.
    let mut attempt = 0;
    loop {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let deposit = phoenix
            .tx()
            .build_deposit_funds(owner, trader, deposit_usdc)?;
        match mth_botkit::send(&rpc, authority, deposit, "deposit collateral") {
            Ok(_) => break,
            Err(error) if attempt < 5 && error.to_string().contains("TraderFrozen") => {
                attempt += 1;
                warn!(%owner, attempt, "trader not enabled yet; retrying the deposit");
            }
            Err(error) => return Err(error.into()),
        }
    }
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

/// Byte layout of a spline collection: a header, then fixed-size splines,
/// each with its bid and ask tick regions (see phoenix-rise's
/// `spline_collection`). A region starts with its start and end offsets from
/// the spline's mid, in ticks.
const SPLINES_HEADER: usize = 112;
const SPLINE_LEN: usize = 1336;
const REGION_LEN: usize = 48;
const BID_REGIONS_AT: usize = 120;
const ASK_REGIONS_AT: usize = 600;

/// Push every maker on `symbol` out to quote no closer than
/// `half_spread_ticks` from its mid, keeping the shape of its liquidity: each
/// tick region moves out by the same amount. Makers re-centre on every
/// market move and keep the offsets, so the spread holds all session.
/// Setup only: it writes the account with a cheatcode. Returns the ticks
/// added.
pub fn widen_makers(
    surfnet: &Surfnet,
    phoenix: &Phoenix,
    symbol: &str,
    half_spread_ticks: u64,
) -> Result<u64> {
    let address: Pubkey = phoenix
        .market(symbol)?
        .spline_pubkey
        .parse()
        .map_err(|_| Error::InvalidExchangeKey(symbol.to_string()))?;
    let rpc = chain::rpc_client(surfnet);
    let account = rpc.get_account(&address)?;
    let invalid = |reason: String| Error::Splines {
        account: address.to_string(),
        reason,
    };
    let inner = innermost_offset(&account.data).map_err(invalid)?;
    let add = half_spread_ticks.saturating_sub(inner);
    let count = SplineCollection::try_from_account_bytes(&account.data)
        .map_err(|e| invalid(e.to_string()))?
        .splines()
        .count();
    let mut data = account.data.clone();
    for spline in 0..count {
        for regions in [BID_REGIONS_AT, ASK_REGIONS_AT] {
            for region in 0..SPLINE_REGION_CAPACITY {
                let at = SPLINES_HEADER + spline * SPLINE_LEN + regions + region * REGION_LEN;
                for offset in [at, at + 8] {
                    let bytes: [u8; 8] = data[offset..offset + 8].try_into().expect("8 bytes");
                    let ticks = u64::from_le_bytes(bytes).saturating_add(add);
                    data[offset..offset + 8].copy_from_slice(&ticks.to_le_bytes());
                }
            }
        }
    }
    // The layout is read back through phoenix-rise, so a layout change fails
    // here rather than quietly writing the wrong bytes.
    let widened = innermost_offset(&data).map_err(invalid)?;
    if widened != inner + add {
        return Err(invalid(format!(
            "widening by {add} ticks moved the innermost quote from {inner} to {widened}"
        )));
    }
    surfnet
        .cheatcodes()
        .set_account(&address, account.lamports, &data, &account.owner)?;
    info!(symbol, inner, add, "makers widened");
    Ok(add)
}

/// The closest any enabled maker quotes to its mid, in ticks.
fn innermost_offset(data: &[u8]) -> std::result::Result<u64, String> {
    let collection = SplineCollection::try_from_account_bytes(data).map_err(|e| e.to_string())?;
    collection
        .splines()
        .filter_map(|spline| spline.ok())
        .filter(|spline| spline.is_enabled())
        .flat_map(|spline| {
            spline
                .active_bid_regions()
                .iter()
                .chain(spline.active_ask_regions())
                .map(|region| region.start_offset().as_inner())
                .collect::<Vec<_>>()
        })
        .min()
        .ok_or_else(|| "no enabled maker quotes".to_string())
}
