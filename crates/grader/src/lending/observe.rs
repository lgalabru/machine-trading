//! What mandate v0.3.0's rules 11 to 13 read of a session's transactions
//! (`specs/mandate.md`): each landed or failed transaction of the bot's,
//! read back from the chain as `getTransaction` serves it, for its costs,
//! its flash borrows, and what it left the wallet holding.

use std::collections::BTreeMap;

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

/// An owner's wallet as its own transactions leave it, one after another in
/// the chain's order, from what it held before the first: rule 11's measure
/// after each, read from the transactions' own balances and never from a
/// reading between steps, which a bot that splits a route over two
/// transactions in one step would slip between. Each transaction's post
/// balances set the token accounts it lists, and the owner's lamports, so
/// what moved the wallet in between (the risk desk's close, say) is read at
/// the next transaction that touches the account.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WalletTrail {
    /// Each of the owner's token accounts: its mint and atoms.
    accounts: BTreeMap<Pubkey, (Pubkey, u64)>,
    lamports: u64,
}

impl WalletTrail {
    /// The wallet before the session's first transaction: each token
    /// account's mint and atoms, and the owner's lamports.
    pub fn new(accounts: BTreeMap<Pubkey, (Pubkey, u64)>, lamports: u64) -> Self {
        Self { accounts, lamports }
    }

    /// The wallet after `transaction` (a `getTransaction` answer, `json`
    /// encoding), one of `owner`'s that ran; `None` if it doesn't read.
    pub fn apply(&mut self, transaction: &Value, owner: &Pubkey) -> Option<()> {
        for change in token_changes(transaction)?
            .into_iter()
            .filter(|c| c.owner == Some(*owner))
        {
            self.accounts
                .insert(change.account, (change.mint, change.post));
        }
        // A bot may send a transaction signed and paid by a key of its own:
        // the owner's lamports then didn't move.
        let keys = account_keys(transaction)?;
        if let Some(i) = keys.iter().position(|k| k == owner) {
            self.lamports = transaction["meta"]["postBalances"].get(i)?.as_u64()?;
        }
        Some(())
    }

    /// The atoms held of each mint but `numeraire`, empty ones aside.
    pub fn others(&self, numeraire: &Pubkey) -> BTreeMap<Pubkey, u64> {
        let mut out: BTreeMap<Pubkey, u64> = BTreeMap::new();
        for (mint, atoms) in self.accounts.values() {
            if mint == numeraire || *atoms == 0 {
                continue;
            }
            let held = out.entry(*mint).or_default();
            *held = held.saturating_add(*atoms);
        }
        out
    }

    pub fn lamports(&self) -> u64 {
        self.lamports
    }
}

/// `owner`'s token accounts of each of `mints`: address, mint and atoms.
pub async fn token_accounts(
    chain: &dyn mtx_kit::chain::ChainClient,
    owner: &Pubkey,
    mints: impl IntoIterator<Item = Pubkey>,
) -> Result<BTreeMap<Pubkey, (Pubkey, u64)>, mtx_kit::chain::ChainError> {
    let mut out = BTreeMap::new();
    for mint in mints {
        for (address, account) in chain.get_token_accounts_by_owner(owner, &mint).await? {
            let atoms = mtx_kit::chain::token::token_balance(&account)?;
            out.insert(address, (mint, atoms));
        }
    }
    Ok(out)
}

/// Order `records`' transactions that ran (landed, or landed and failed,
/// which still pays its fee) as the chain ran them: by the slot they ran
/// in, then by their place under the transaction layer (`landing`, by
/// signature: the layer's block and the index in it, as the layer counts
/// blocks), then in the order sent, which is the chain's without the
/// layer.
pub fn chain_order<'r>(
    records: &'r [crate::session::TxRecord],
    landing: &BTreeMap<String, (u64, u64)>,
) -> Vec<&'r crate::session::TxRecord> {
    let mut ran: Vec<(usize, &crate::session::TxRecord)> = records
        .iter()
        .enumerate()
        .filter(|(_, r)| !r.duplicate && r.signature.is_some() && r.at.is_some())
        .collect();
    ran.sort_by_key(|(i, r)| {
        let signature = r.signature.as_deref().expect("filtered");
        let place = landing
            .get(signature)
            .copied()
            .unwrap_or((u64::MAX, u64::MAX));
        (r.at.expect("filtered"), place, *i)
    });
    ran.into_iter().map(|(_, r)| r).collect()
}

/// What an owner held beside its numeraire right after each of its
/// transactions that ran, by signature: `start`'s wallet moved by each
/// transaction's own balances (`found`, the `getTransaction` answers) in
/// the chain's order ([`chain_order`]), valued by `value` (the trail, and
/// the slot it ran at). Rule 11's measure: never a step's reading, which a
/// bot that splits a route over two transactions in one step slips
/// between. `Err` names a transaction that doesn't read back.
pub fn held_after_each(
    records: &[crate::session::TxRecord],
    landing: &BTreeMap<String, (u64, u64)>,
    found: &BTreeMap<String, Value>,
    owner: &Pubkey,
    start: &WalletTrail,
    value: impl Fn(&WalletTrail, u64) -> Option<f64>,
) -> Result<BTreeMap<String, Option<f64>>, String> {
    let mut trail = start.clone();
    let mut held = BTreeMap::new();
    for record in chain_order(records, landing) {
        let signature = record.signature.as_deref().expect("ordered: signed");
        let transaction = found.get(signature).ok_or_else(|| signature.to_string())?;
        trail
            .apply(transaction, owner)
            .ok_or_else(|| signature.to_string())?;
        let at = record.at.expect("ordered: ran");
        let before = held.insert(signature.to_string(), value(&trail, at));
        assert!(before.is_none(), "a transaction runs once");
    }
    Ok(held)
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

    /// One token balance as `getTransaction` lists it.
    fn balance(index: usize, mint: &Pubkey, owner: &Pubkey, atoms: u64) -> Value {
        json!({
            "accountIndex": index,
            "mint": mint.to_string(),
            "owner": owner.to_string(),
            "uiTokenAmount": { "amount": atoms.to_string() },
        })
    }

    /// A transaction naming `keys`, its token balances before and after,
    /// and its first key's lamports before and after.
    fn ran(keys: &[Pubkey], pre: Vec<Value>, post: Vec<Value>, lamports: (u64, u64)) -> Value {
        let mut pre_balances = vec![0u64; keys.len()];
        let mut post_balances = vec![0u64; keys.len()];
        (pre_balances[0], post_balances[0]) = lamports;
        json!({
            "meta": {
                "fee": 5_000,
                "preTokenBalances": pre,
                "postTokenBalances": post,
                "preBalances": pre_balances,
                "postBalances": post_balances,
            },
            "transaction": { "message": {
                "accountKeys": keys.iter().map(ToString::to_string).collect::<Vec<_>>(),
                "instructions": [],
            }},
        })
    }

    fn record(signature: &str, step: u64, at: u64, ok: bool) -> crate::session::TxRecord {
        crate::session::TxRecord {
            step: Some(step),
            signature: Some(signature.into()),
            at: Some(at),
            ok,
            duplicate: false,
            error: None,
            instructions: Vec::new(),
            compute_units: None,
            fee_lamports: None,
            logs: Vec::new(),
        }
    }

    /// A bot that buys JitoSOL in one transaction and sells it in the next,
    /// both in one step: the step's reading after them shows nothing held,
    /// but the wallet held JitoSOL between them, and rule 11 breaks.
    #[test]
    fn a_route_split_over_two_transactions_in_one_step_breaks_rule_11() {
        let (bot, wsol, jitosol) = (
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        );
        let (bot_wsol, bot_jitosol, pool) = (
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        );
        let keys = [bot, bot_wsol, bot_jitosol, pool];
        let sol = 1_000_000_000u64;
        // Buy 9.9 JitoSOL with 10 SOL, opening the JitoSOL account.
        let buy = ran(
            &keys,
            vec![balance(1, &wsol, &bot, 100 * sol)],
            vec![
                balance(1, &wsol, &bot, 90 * sol),
                balance(2, &jitosol, &bot, 9_900_000_000),
            ],
            (2 * sol, 2 * sol - 5_000),
        );
        // Sell it back for 10.05 SOL, closing the account.
        let sell = ran(
            &keys,
            vec![
                balance(1, &wsol, &bot, 90 * sol),
                balance(2, &jitosol, &bot, 9_900_000_000),
            ],
            vec![balance(1, &wsol, &bot, 100 * sol + 50_000_000)],
            (2 * sol - 5_000, 2 * sol - 10_000),
        );
        let found: BTreeMap<String, Value> =
            [("buy".to_string(), buy), ("sell".to_string(), sell)].into();
        let records = [record("buy", 3, 10, true), record("sell", 3, 10, true)];
        let start = WalletTrail::new([(bot_wsol, (wsol, 100 * sol))].into(), 2 * sol);
        // JitoSOL at 1.1 SOL, SOL at $150.
        let value = |trail: &WalletTrail, _at: u64| {
            let others = trail.others(&wsol);
            Some(others.get(&jitosol).copied().unwrap_or(0) as f64 / 1e9 * 1.1 * 150.0)
        };
        let held =
            held_after_each(&records, &BTreeMap::new(), &found, &bot, &start, value).unwrap();
        assert!((held["buy"].unwrap() - 1_633.5).abs() < 1e-6, "{held:?}");
        assert_eq!(held["sell"], Some(0.0));
        // The same, as mandate v0.3.0 audits it: a breach at the buy.
        let mandate: mtx_core::mandate::Mandate = serde_json::from_value(json!({
            "version": "v0.3.0",
            "universe": [{ "instrument": "orca:amm:*", "assets": [wsol.to_string(), jitosol.to_string()] }],
            "limits": { "inventory": { "numeraire": wsol.to_string(), "max_other_usd": 5.0 } },
        }))
        .unwrap();
        let transactions = ["buy", "sell"]
            .iter()
            .map(|signature| mtx_core::mandate::ObservedTransaction {
                signature: signature.to_string(),
                slot: 10,
                outcome: mtx_core::mandate::TransactionOutcome::Landed,
                fee_usd: 0.0,
                priority_fee_usd: 0.0,
                tip_usd: 0.0,
                other_holdings_usd: held[*signature],
                flash_borrows: Vec::new(),
            })
            .collect();
        let mut observed = mtx_core::mandate::Observed::default();
        observed.transactions = transactions;
        match mandate.audit(&observed) {
            Some(mtx_core::mandate::Breach::Inventory { signature, .. }) => {
                assert_eq!(signature, "buy")
            }
            other => panic!("expected an inventory breach at the buy, got {other:?}"),
        }
        // A route in one transaction holds nothing after it.
        let atomic = ran(
            &keys,
            vec![balance(1, &wsol, &bot, 100 * sol)],
            vec![balance(1, &wsol, &bot, 100 * sol + 50_000_000)],
            (2 * sol, 2 * sol - 5_000),
        );
        let found: BTreeMap<String, Value> = [("route".to_string(), atomic)].into();
        let held = held_after_each(
            &[record("route", 3, 10, true)],
            &BTreeMap::new(),
            &found,
            &bot,
            &start,
            value,
        )
        .unwrap();
        assert_eq!(held["route"], Some(0.0));
    }

    /// The trail follows the chain's order, not the order sent: under the
    /// layer a later send that pays more runs first.
    #[test]
    fn the_trail_runs_in_the_chains_order() {
        let records = [
            record("x", 1, 5, true),
            record("y", 1, 5, false),
            record("z", 0, 4, true),
        ];
        let order = |landing: &BTreeMap<String, (u64, u64)>| {
            chain_order(&records, landing)
                .iter()
                .map(|r| r.signature.clone().unwrap())
                .collect::<Vec<_>>()
        };
        assert_eq!(order(&BTreeMap::new()), ["z", "x", "y"]);
        let landing: BTreeMap<String, (u64, u64)> = [
            ("x".to_string(), (6, 3)),
            ("y".to_string(), (6, 0)),
            ("z".to_string(), (5, 1)),
        ]
        .into();
        assert_eq!(order(&landing), ["z", "y", "x"]);
    }

    /// Lamports come from the owner's own balance, where it signed; a token
    /// account another key owns isn't the wallet's.
    #[test]
    fn the_trail_reads_only_the_owners_accounts() {
        let (bot, other, mint) = (
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        );
        let (ours, theirs) = (Pubkey::new_unique(), Pubkey::new_unique());
        let mut trail = WalletTrail::new([(ours, (mint, 7))].into(), 1_000);
        let tx = ran(
            &[bot, ours, theirs],
            vec![balance(1, &mint, &bot, 7), balance(2, &mint, &other, 50)],
            vec![balance(1, &mint, &bot, 3), balance(2, &mint, &other, 54)],
            (1_000, 900),
        );
        trail.apply(&tx, &bot).unwrap();
        assert_eq!(trail.others(&Pubkey::new_unique()), [(mint, 3)].into());
        assert_eq!(trail.lamports(), 900);
        // Paid and signed by another key: the owner's lamports stay.
        let paid = ran(
            &[other, ours],
            vec![],
            vec![balance(1, &mint, &bot, 1)],
            (5, 4),
        );
        trail.apply(&paid, &bot).unwrap();
        assert_eq!(
            (trail.others(&Pubkey::new_unique())[&mint], trail.lamports()),
            (1, 900)
        );
        assert_eq!(trail.apply(&json!({}), &bot), None);
    }

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
