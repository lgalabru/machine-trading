//! What mandate v0.3.0's rules 11 to 13 read of a lending session's
//! transactions (`specs/mandate.md`): each landed or failed transaction of
//! the bot's, read back from the chain as `getTransaction` serves it, for
//! its costs and its flash borrows.

use serde_json::Value;
use solana_pubkey::Pubkey;

use crate::{
    txlayer::{jito, pool},
    venue::kamino::klend,
};

/// Lamports a transaction paid, split as the costs rule sums them, and the
/// flash borrows it asked for.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Costs {
    /// The base fee: the chain's fee less the priority fee.
    pub base_lamports: u64,
    /// The compute unit price times the units it asked for.
    pub priority_lamports: u64,
    /// Transfers to Jito's tip accounts.
    pub tip_lamports: u64,
    /// Each flash borrow: the reserve and the atoms it lent.
    pub flash_borrows: Vec<(Pubkey, u64)>,
}

/// The compute units a transaction gets without a limit of its own: the
/// runtime's 200,000 per instruction other than the budget's.
const UNITS_PER_INSTRUCTION_DEFAULT: u64 = pool::DEFAULT_UNITS_PER_INSTRUCTION;

/// A system transfer's tag (`SystemInstruction::Transfer`).
const SYSTEM_TRANSFER: u32 = 2;

/// Every account `transaction` (a `getTransaction` answer, `json`
/// encoding) names, in index order: its own keys, then those its lookup
/// tables loaded, writable first.
pub fn account_keys(transaction: &Value) -> Option<Vec<Pubkey>> {
    let meta = &transaction["meta"];
    let message = &transaction["transaction"]["message"];
    let mut keys: Vec<Pubkey> = Vec::new();
    for key in message["accountKeys"]
        .as_array()?
        .iter()
        .chain(
            meta["loadedAddresses"]["writable"]
                .as_array()
                .into_iter()
                .flatten(),
        )
        .chain(
            meta["loadedAddresses"]["readonly"]
                .as_array()
                .into_iter()
                .flatten(),
        )
    {
        keys.push(key.as_str()?.parse().ok()?);
    }
    Some(keys)
}

/// One token account's balance across `transaction`: its account, mint and
/// owner, and its amount in atoms before and after (0 where it didn't
/// exist).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenChange {
    pub account: Pubkey,
    pub mint: Pubkey,
    pub owner: Option<Pubkey>,
    pub pre: u64,
    pub post: u64,
}

/// Every token balance `transaction` lists, before and after; `None` if it
/// isn't a transaction.
pub fn token_changes(transaction: &Value) -> Option<Vec<TokenChange>> {
    let keys = account_keys(transaction)?;
    let meta = &transaction["meta"];
    let mut changes: Vec<TokenChange> = Vec::new();
    for (list, post) in [("preTokenBalances", false), ("postTokenBalances", true)] {
        for balance in meta[list].as_array().into_iter().flatten() {
            let account = *keys.get(usize::try_from(balance["accountIndex"].as_u64()?).ok()?)?;
            let amount: u64 = balance["uiTokenAmount"]["amount"].as_str()?.parse().ok()?;
            let at = match changes.iter().position(|c| c.account == account) {
                Some(at) => at,
                None => {
                    changes.push(TokenChange {
                        account,
                        mint: balance["mint"].as_str()?.parse().ok()?,
                        owner: balance["owner"].as_str().and_then(|o| o.parse().ok()),
                        pre: 0,
                        post: 0,
                    });
                    changes.len() - 1
                }
            };
            match post {
                true => changes[at].post = amount,
                false => changes[at].pre = amount,
            }
        }
    }
    Some(changes)
}

/// What `transaction` (a `getTransaction` answer, `json` encoding) cost and
/// borrowed; `None` if it isn't one.
pub fn costs(transaction: &Value) -> Option<Costs> {
    let meta = &transaction["meta"];
    let fee = meta["fee"].as_u64()?;
    let message = &transaction["transaction"]["message"];
    let keys = account_keys(transaction)?;
    let tips: Vec<Pubkey> = jito::TIP_ACCOUNTS
        .iter()
        .filter_map(|a| a.parse().ok())
        .collect();
    let flash_tag = klend::instruction_tag("flash_borrow_reserve_liquidity");
    let mut limit: Option<u64> = None;
    let mut price: u64 = 0;
    let mut counted: u64 = 0;
    let mut out = Costs::default();
    for ix in message["instructions"].as_array()? {
        let program = *keys.get(usize::try_from(ix["programIdIndex"].as_u64()?).ok()?)?;
        let data = bs58::decode(ix["data"].as_str()?).into_vec().ok()?;
        let accounts: Vec<Pubkey> = ix["accounts"]
            .as_array()?
            .iter()
            .map(|i| Some(*keys.get(usize::try_from(i.as_u64()?).ok()?)?))
            .collect::<Option<_>>()?;
        if program == pool::COMPUTE_BUDGET {
            match data.first() {
                Some(2) => {
                    limit = Some(u64::from(u32::from_le_bytes(
                        data.get(1..5)?.try_into().ok()?,
                    )))
                }
                Some(3) => price = u64::from_le_bytes(data.get(1..9)?.try_into().ok()?),
                _ => {}
            }
            continue;
        }
        counted += 1;
        if program == pool::SYSTEM
            && data.get(..4) == Some(&SYSTEM_TRANSFER.to_le_bytes()[..])
            && accounts.get(1).is_some_and(|to| tips.contains(to))
        {
            out.tip_lamports += u64::from_le_bytes(data.get(4..12)?.try_into().ok()?);
        }
        if program == klend::PROGRAM_ID && data.get(..8) == Some(&flash_tag[..]) {
            let reserve = *accounts.get(3)?;
            let atoms = u64::from_le_bytes(data.get(8..16)?.try_into().ok()?);
            out.flash_borrows.push((reserve, atoms));
        }
    }
    let units = limit
        .unwrap_or(counted * UNITS_PER_INSTRUCTION_DEFAULT)
        .min(pool::MAX_COMPUTE_UNITS);
    // The runtime rounds the priority fee up to a whole lamport.
    out.priority_lamports = (u128::from(price) * u128::from(units)).div_ceil(1_000_000) as u64;
    out.base_lamports = fee.checked_sub(out.priority_lamports)?;
    Some(out)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_flash_liquidations_costs_and_borrow_read_back() {
        let user = Pubkey::new_unique();
        let reserve = Pubkey::new_unique();
        let tip_account: Pubkey = jito::TIP_ACCOUNTS[3].parse().unwrap();
        let mut borrow = klend::instruction_tag("flash_borrow_reserve_liquidity").to_vec();
        borrow.extend(1_500_000u64.to_le_bytes());
        let mut tip = SYSTEM_TRANSFER.to_le_bytes().to_vec();
        tip.extend(10_000u64.to_le_bytes());
        let b58 = |d: &[u8]| bs58::encode(d).into_string();
        let keys = [
            user,
            pool::COMPUTE_BUDGET,
            klend::PROGRAM_ID,
            pool::SYSTEM,
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        ];
        let transaction = json!({
            "meta": {
                "fee": 5_000 + 30_000,
                "loadedAddresses": { "writable": [reserve.to_string(), tip_account.to_string()], "readonly": [] },
            },
            "transaction": { "message": {
                "accountKeys": keys.iter().map(ToString::to_string).collect::<Vec<_>>(),
                "instructions": [
                    { "programIdIndex": 1, "accounts": [], "data": b58(&[&[2u8][..], &300_000u32.to_le_bytes()].concat()) },
                    { "programIdIndex": 1, "accounts": [], "data": b58(&[&[3u8][..], &100_000u64.to_le_bytes()].concat()) },
                    { "programIdIndex": 2, "accounts": [0, 4, 5, 7], "data": b58(&borrow) },
                    { "programIdIndex": 3, "accounts": [0, 8], "data": b58(&tip) },
                ],
            }},
        });
        let read = costs(&transaction).unwrap();
        assert_eq!(read.priority_lamports, 30_000, "300k units at 0.1 lamport");
        assert_eq!(read.base_lamports, 5_000);
        assert_eq!(read.tip_lamports, 10_000);
        assert_eq!(read.flash_borrows, vec![(reserve, 1_500_000)]);
        assert_eq!(costs(&json!({})), None);
        let mut short = transaction.clone();
        short["meta"]["fee"] = json!(1_000);
        assert_eq!(
            costs(&short),
            None,
            "a fee under its priority fee isn't one"
        );
    }
}
