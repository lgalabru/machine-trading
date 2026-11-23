//! What the bot paid to land its transactions: the fee ledger, from the
//! chain's own record of each landed transaction, checked against the
//! wallet's SOL.

use serde::{Deserialize, Serialize};
use serde_json::json;
use solana_pubkey::Pubkey;
use solana_rpc_client::rpc_client::RpcClient;

/// The token programs whose accounts a wallet can close for their rent.
const TOKEN_PROGRAMS: [&str; 2] = [
    "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA",
    "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb",
];

/// Wrapped SOL's mint, whose token accounts hold the SOL they wrap as
/// lamports beside their rent.
const NATIVE_MINT: Pubkey = solana_pubkey::pubkey!("So11111111111111111111111111111111111111112");

/// The rent a token account (as `getTokenAccountsByOwner` serves it,
/// base64) holds: its lamports, less the SOL it wraps if it is wrapped
/// SOL's. That SOL is a holding the session values with the rest of the
/// account's tokens, not rent.
fn rent_of(account: &serde_json::Value) -> Option<u64> {
    use base64::Engine as _;
    let lamports = account["lamports"].as_u64()?;
    let data = account["data"][0]
        .as_str()
        .and_then(|d| base64::engine::general_purpose::STANDARD.decode(d).ok())?;
    let mint = Pubkey::new_from_array(data.get(..32)?.try_into().ok()?);
    if mint != NATIVE_MINT {
        return Some(lamports);
    }
    let wrapped = u64::from_le_bytes(data.get(64..72)?.try_into().ok()?);
    Some(lamports.saturating_sub(wrapped))
}

/// A wallet's SOL: its own lamports, and those in token accounts it can
/// close, which are its rent, not a cost.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Wallet {
    pub lamports: u64,
    pub closable: u64,
}

impl Wallet {
    /// `owner`'s wallet, read through the grader's own endpoint.
    pub fn read(rpc: &RpcClient, owner: &Pubkey) -> crate::Result<Self> {
        let lamports = rpc.get_balance(owner)?;
        let mut closable = 0;
        for program in TOKEN_PROGRAMS {
            let accounts = crate::chain::call(
                rpc,
                "getTokenAccountsByOwner",
                json!([owner.to_string(), { "programId": program }, { "encoding": "base64", "commitment": "processed" }]),
            )?;
            closable += accounts["value"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|a| rent_of(&a["account"]))
                .sum::<u64>();
        }
        Ok(Self { lamports, closable })
    }

    /// Its SOL change since `start`, and its closable rent's.
    pub fn change_since(&self, start: &Wallet) -> (i64, i64) {
        (
            self.lamports as i64 - start.lamports as i64,
            self.closable as i64 - start.closable as i64,
        )
    }

    /// What it has spent since `start`, rent aside, and `route_fees` paid
    /// off chain, at `sol_usd`: what counts against the account's value.
    pub fn value_change_usd(&self, start: &Wallet, route_fees: u64, sol_usd: f64) -> f64 {
        let (change, closable) = self.change_since(start);
        (change + closable - route_fees as i64) as f64 * sol_usd / 1e9
    }
}

/// Lamports the bot paid, by kind.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct FeeLedger {
    /// Base fees (per signature) and priority fees of its landed
    /// transactions that succeeded.
    pub base: u64,
    pub priority: u64,
    /// Tips its landed bundles and transactions paid Jito's tip accounts.
    pub tips: u64,
    /// Fees of its transactions that landed and failed: they pay too.
    pub failed_tx_fees: u64,
    /// What its routes charged, beyond the chain's fees (a staked
    /// connection's price): counted in PnL, not on chain.
    pub route_fees: u64,
    /// Transactions that landed.
    pub transactions: u64,
    /// Fees the risk desk's close paid from the wallet: it lands past the
    /// layer (see [`crate::risk`]).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub forced_close: u64,
    /// Fees and tips of the bot's transactions that another key paid for:
    /// not from the wallet, so outside the reconciliation.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub paid_by_others: u64,
    /// The wallet's SOL change over the session, and what the ledger leaves
    /// unexplained of it (rent moved into accounts the bot can close
    /// included): 0 when they agree.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wallet_change: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unexplained: Option<i64>,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

impl FeeLedger {
    /// A landed transaction's fee and tip: `fee` is what the chain
    /// charged, the base part `base`.
    pub fn landed(&mut self, ok: bool, fee: u64, base: u64, tip: u64) {
        self.transactions += 1;
        self.tips += tip;
        if ok {
            self.base += base.min(fee);
            self.priority += fee.saturating_sub(base);
        } else {
            self.failed_tx_fees += fee;
        }
    }

    /// Everything paid, on chain and off.
    pub fn total(&self) -> u64 {
        self.base
            + self.priority
            + self.tips
            + self.failed_tx_fees
            + self.route_fees
            + self.forced_close
    }

    /// Check the ledger against the wallet: `change` is its SOL change and
    /// `closable` the lamports moved into accounts it can close.
    pub fn reconcile(&mut self, change: i64, closable: i64) {
        self.wallet_change = Some(change);
        let paid = (self.base + self.priority + self.tips + self.failed_tx_fees + self.forced_close)
            as i64;
        self.unexplained = Some(change + paid + closable);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Wrapped SOL's account holds its rent and the SOL it wraps: only
    /// the rent is the wallet's to close; another token's lamports are all
    /// rent.
    #[test]
    fn a_wrapped_sol_account_is_rent_and_a_holding() {
        use base64::Engine as _;
        let account = |mint: Pubkey, amount: u64, lamports: u64| {
            let mut data = vec![0u8; 165];
            data[..32].copy_from_slice(mint.as_ref());
            data[64..72].copy_from_slice(&amount.to_le_bytes());
            json!({ "lamports": lamports, "data": [base64::engine::general_purpose::STANDARD.encode(&data), "base64"] })
        };
        assert_eq!(
            rent_of(&account(NATIVE_MINT, 5_000_000, 7_039_280)),
            Some(2_039_280)
        );
        assert_eq!(
            rent_of(&account(Pubkey::new_unique(), 5_000_000, 2_039_280)),
            Some(2_039_280)
        );
        assert_eq!(rent_of(&json!({ "lamports": 1 })), None);
    }

    #[test]
    fn the_ledger_splits_fees_and_reconciles() {
        let mut ledger = FeeLedger::default();
        ledger.landed(true, 12_500, 5_000, 10_000);
        ledger.landed(false, 5_000, 5_000, 0);
        ledger.route_fees = 100;
        assert_eq!(
            (
                ledger.base,
                ledger.priority,
                ledger.tips,
                ledger.failed_tx_fees
            ),
            (5_000, 7_500, 10_000, 5_000)
        );
        assert_eq!(ledger.total(), 27_600);
        ledger.reconcile(-27_500, 0);
        assert_eq!(ledger.unexplained, Some(0));
        ledger.reconcile(-27_500 - 2_039_280, 2_039_280);
        assert_eq!(ledger.unexplained, Some(0), "rent isn't a cost");
    }
}
