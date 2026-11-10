//! The grader's [`AccountMeter`](mtx_core::AccountMeter)s:
//! what it measures on each venue, read from the venue and the chain, never
//! from the bot (`docs/rfd/0001-instrument-traits.md`).
//!
//! | Meter | Venue profile |
//! | --- | --- |
//! | [`PhoenixPerpMeter`] | `specs/venues/phoenix.md`, Perpetual markets |
//! | [`PhoenixSpotMeter`] | `specs/venues/phoenix.md`, Spot markets |

mod phoenix_perp;
pub mod phoenix_spot;

use mtx_core::{Decimal, MeterError};
pub use phoenix_perp::PhoenixPerpMeter;
pub use phoenix_spot::{PhoenixSpotMeter, SpotMarket};
use serde_json::json;
use solana_pubkey::Pubkey;
use solana_rpc_client::rpc_client::RpcClient;

use crate::chain;

/// What `owner`'s token accounts for `mint` hold together, in atoms.
fn wallet_atoms(rpc: &RpcClient, owner: &Pubkey, mint: &Pubkey) -> Result<u64, MeterError> {
    let accounts = chain::call(
        rpc,
        "getTokenAccountsByOwner",
        json!([owner.to_string(), { "mint": mint.to_string() }, { "encoding": "jsonParsed" }]),
    )
    .map_err(read)?;
    accounts["value"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|account| {
            account["account"]["data"]["parsed"]["info"]["tokenAmount"]["amount"]
                .as_str()
                .and_then(|amount| amount.parse::<u64>().ok())
                .ok_or_else(|| MeterError::Decode {
                    what: "token account",
                    detail: format!("no amount in {account}"),
                })
        })
        .try_fold(0u64, |sum, atoms| {
            sum.checked_add(atoms?).ok_or_else(|| MeterError::Decode {
                what: "token account",
                detail: "amounts overflow u64".into(),
            })
        })
}

/// Atoms as units of a token with `decimals` decimals, exactly.
fn units(atoms: u128, decimals: u32) -> Result<Decimal, MeterError> {
    i128::try_from(atoms)
        .ok()
        .and_then(|atoms| Decimal::try_from_i128_with_scale(atoms, decimals).ok())
        .ok_or_else(|| MeterError::Decode {
            what: "amount",
            detail: format!("{atoms} atoms at {decimals} decimals doesn't fit a decimal"),
        })
}

fn read(error: impl std::fmt::Display) -> MeterError {
    MeterError::Read(error.to_string())
}
