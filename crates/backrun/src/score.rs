use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

use crate::{INITIAL_LAMPORTS, LAMPORTS_PER_SOL};

/// A sentinel strictly below losing every lamport of initial capital.
pub const DISQUALIFIED_REWARD: f64 = -101.0;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Balances {
    pub sol_lamports: u64,
    pub wsol_lamports: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Score {
    pub valid: bool,
    pub disqualification: Option<String>,
    pub net_profit_lamports: i128,
    pub reward: f64,
    pub gross_trading_profit_lamports: i128,
    pub transaction_fees_lamports: u64,
    pub priority_fees_lamports: u64,
}

impl Score {
    pub fn from_balances(
        end: &Balances,
        all_fees: u64,
        priority_fees: u64,
        violation: Option<String>,
    ) -> Result<Self> {
        ensure!(priority_fees <= all_fees, "priority fees exceed total fees");
        let profit = i128::from(end.sol_lamports) + i128::from(end.wsol_lamports)
            - i128::from(INITIAL_LAMPORTS);
        Ok(Self {
            valid: violation.is_none(),
            reward: if violation.is_some() {
                DISQUALIFIED_REWARD
            } else {
                profit as f64 / LAMPORTS_PER_SOL as f64
            },
            net_profit_lamports: profit,
            gross_trading_profit_lamports: profit + i128::from(all_fees),
            transaction_fees_lamports: all_fees,
            priority_fees_lamports: priority_fees,
            disqualification: violation,
        })
    }
}

/// Realizable native capital. Token account lamports include native backing
/// (WSOL) and refundable rent; other token amounts and LP positions get no credit.
/// Counting the same accounts at both endpoints prevents setup-rent harvesting.
pub fn capital(snapshot: &serde_json::Value, wallet: &str, scratch: &[String]) -> Result<u64> {
    use anyhow::Context;
    let accounts: std::collections::BTreeMap<String, Option<surfpool_types::AccountSnapshot>> =
        serde_json::from_value(snapshot.clone())?;
    let owner: solana_pubkey::Pubkey = wallet.parse()?;
    let mut total = 0u64;
    for (address, account) in accounts {
        let Some(account) = account else { continue };
        let account = account.to_account().map_err(anyhow::Error::msg)?;
        let owned_token = account.owner == crate::programs::key(crate::programs::TOKEN)
            && account.data.len() == 165
            && account.data[32..64] == owner.to_bytes();
        if address == wallet || owned_token || scratch.contains(&address) {
            total = total
                .checked_add(account.lamports)
                .context("capital overflow")?;
        }
    }
    Ok(total)
}
impl Score {
    pub fn from_capital(initial: u64, final_value: u64, fees: u64, priority: u64) -> Result<Self> {
        ensure!(
            initial < 101 * LAMPORTS_PER_SOL,
            "funded capital exceeds disqualification sentinel bound"
        );
        ensure!(priority <= fees, "priority fees exceed fees");
        let profit = i128::from(final_value) - i128::from(initial);
        Ok(Self {
            valid: true,
            disqualification: None,
            net_profit_lamports: profit,
            reward: profit as f64 / LAMPORTS_PER_SOL as f64,
            gross_trading_profit_lamports: profit + i128::from(fees),
            transaction_fees_lamports: fees,
            priority_fees_lamports: priority,
        })
    }
}

/// Use the pinned runtime fee calculator, including precompile signatures.
pub fn priority_fee(
    tx: &solana_transaction::versioned::VersionedTransaction,
    total: u64,
) -> Result<u64> {
    use solana_message::{
        SanitizedMessage, SanitizedVersionedMessage, SimpleAddressLoader, VersionedMessage,
        v0::LoadedAddresses,
    };
    let loader = if matches!(tx.message, VersionedMessage::V0(_)) {
        SimpleAddressLoader::Enabled(LoadedAddresses::default())
    } else {
        SimpleAddressLoader::Disabled
    };
    let message = SanitizedMessage::try_new(
        SanitizedVersionedMessage::try_from(tx.message.clone())?,
        loader,
        &std::collections::HashSet::new(),
    )?;
    let base = solana_fee::calculate_fee(&message, 5000, 0, solana_fee::FeeFeatures {});
    total
        .checked_sub(base)
        .ok_or_else(|| anyhow::anyhow!("runtime fee lower than signature fee"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FEE_LAMPORTS, TRADING_LAMPORTS};
    #[test]
    fn fees_are_not_subtracted_twice_and_losses_are_preserved() {
        let nop = Balances {
            sol_lamports: FEE_LAMPORTS,
            wsol_lamports: TRADING_LAMPORTS,
        };
        assert_eq!(Score::from_balances(&nop, 0, 0, None).unwrap().reward, 0.0);
        let failed = Balances {
            sol_lamports: FEE_LAMPORTS - 10_000,
            ..nop
        };
        let score = Score::from_balances(&failed, 10_000, 5_000, None).unwrap();
        assert_eq!(score.net_profit_lamports, -10_000);
        assert_eq!(score.gross_trading_profit_lamports, 0);
        assert!(score.reward < 0.0);
        let empty = Balances {
            sol_lamports: 0,
            wsol_lamports: 0,
        };
        assert!(Score::from_balances(&empty, 0, 0, None).unwrap().reward > DISQUALIFIED_REWARD);
    }
}
