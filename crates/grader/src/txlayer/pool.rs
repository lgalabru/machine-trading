//! The pool: every transaction and bundle sent for the builder, until it
//! lands, fails, expires or the session ends.
//!
//! An entry keeps what the builder needs, read once from its transactions:
//! each one's compute-unit limit and price (from its ComputeBudget
//! instructions, or the runtime's default limit), its signatures, the
//! accounts it writes and reads (lookup tables resolved), its blockhash, the
//! lamports it tips to Jito's tip accounts, and whether it carries a
//! `jitodontfront` key. Its **arrival** is the step it was sent in and its
//! sequence number in that step, never a wall time; its first slot adds
//! the route's latency, a seeded draw.

use solana_pubkey::Pubkey;
use solana_transaction::{Hash, versioned::VersionedTransaction};

use super::{RouteKind, jito};

/// The compute budget program.
pub const COMPUTE_BUDGET: Pubkey =
    Pubkey::from_str_const("ComputeBudget111111111111111111111111111111");
pub const SYSTEM: Pubkey = Pubkey::from_str_const("11111111111111111111111111111111");
/// The runtime's limit for a transaction that sets none: 200,000 per
/// instruction that isn't the compute budget's, up to 1.4M.
pub const DEFAULT_UNITS_PER_INSTRUCTION: u64 = 200_000;
pub const MAX_COMPUTE_UNITS: u64 = 1_400_000;
/// Lamports per signature.
pub const BASE_FEE: u64 = 5_000;

/// One transaction of an entry, as the builder sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct PooledTx {
    pub signature: String,
    /// Its wire bytes, as sent.
    pub wire: Vec<u8>,
    pub signatures: u64,
    /// Requested compute units, and the price per unit in micro-lamports.
    pub cu_limit: u64,
    pub cu_price: u64,
    pub writable: Vec<Pubkey>,
    pub readonly: Vec<Pubkey>,
    pub blockhash: Hash,
    /// Lamports it moves to Jito's tip accounts.
    pub tip: u64,
    pub dont_front: bool,
}

impl PooledTx {
    /// Read a signed transaction's wire bytes; `tables` resolves its lookup
    /// tables' addresses.
    pub fn parse(
        wire: Vec<u8>,
        tables: &dyn Fn(&Pubkey) -> Option<Vec<Pubkey>>,
    ) -> Result<Self, String> {
        let tx: VersionedTransaction =
            wincode::deserialize(&wire).map_err(|e| format!("not a transaction: {e}"))?;
        let message = &tx.message;
        let signature = tx
            .signatures
            .first()
            .ok_or("a transaction without a signature")?
            .to_string();
        let header = message.header();
        let statics = message.static_account_keys();
        let mut keys: Vec<Pubkey> = statics.to_vec();
        let mut loaded_writable = Vec::new();
        let mut loaded_readonly = Vec::new();
        if let Some(lookups) = message.address_table_lookups() {
            for lookup in lookups {
                let addresses = tables(&lookup.account_key)
                    .ok_or_else(|| format!("lookup table {} not found", lookup.account_key))?;
                let at = |i: &u8| {
                    addresses.get(*i as usize).copied().ok_or_else(|| {
                        format!("lookup table {} has no index {i}", lookup.account_key)
                    })
                };
                for i in &lookup.writable_indexes {
                    loaded_writable.push(at(i)?);
                }
                for i in &lookup.readonly_indexes {
                    loaded_readonly.push(at(i)?);
                }
            }
        }
        keys.extend(&loaded_writable);
        keys.extend(&loaded_readonly);
        let signed = header.num_required_signatures as usize;
        let readonly_signed = header.num_readonly_signed_accounts as usize;
        let readonly_unsigned = header.num_readonly_unsigned_accounts as usize;
        let mut writable = Vec::new();
        let mut readonly = Vec::new();
        for (i, key) in statics.iter().enumerate() {
            let is_writable = if i < signed {
                i < signed.saturating_sub(readonly_signed)
            } else {
                i < statics.len().saturating_sub(readonly_unsigned)
            };
            if is_writable {
                writable.push(*key);
            } else {
                readonly.push(*key);
            }
        }
        writable.extend(loaded_writable);
        readonly.extend(loaded_readonly);

        let mut cu_limit = None;
        let mut cu_price = 0;
        let mut counted = 0u64;
        let mut tip = 0u64;
        for ix in message.instructions() {
            let program = keys
                .get(ix.program_id_index as usize)
                .ok_or("an instruction names no program")?;
            if *program == COMPUTE_BUDGET {
                match ix.data.first() {
                    Some(2) if ix.data.len() >= 5 => {
                        cu_limit = Some(u32::from_le_bytes(
                            ix.data[1..5].try_into().expect("4 bytes"),
                        ) as u64);
                    }
                    Some(3) if ix.data.len() >= 9 => {
                        cu_price = u64::from_le_bytes(ix.data[1..9].try_into().expect("8 bytes"));
                    }
                    _ => {}
                }
                continue;
            }
            counted += 1;
            // A System transfer (tag 2) to a tip account is a tip.
            if *program == SYSTEM
                && ix.data.len() >= 12
                && u32::from_le_bytes(ix.data[0..4].try_into().expect("4 bytes")) == 2
                && let Some(to) = ix.accounts.get(1).and_then(|i| keys.get(*i as usize))
                && jito::is_tip_account(to)
            {
                tip += u64::from_le_bytes(ix.data[4..12].try_into().expect("8 bytes"));
            }
        }
        let cu_limit = cu_limit
            .unwrap_or(counted * DEFAULT_UNITS_PER_INSTRUCTION)
            .min(MAX_COMPUTE_UNITS);
        let dont_front = keys.iter().any(jito::is_dont_front);
        Ok(Self {
            signature,
            signatures: header.num_required_signatures as u64,
            cu_limit,
            cu_price,
            writable,
            readonly,
            blockhash: *message.recent_blockhash(),
            tip,
            dont_front,
            wire,
        })
    }

    /// The fee it pays if it lands: the base fee per signature and the
    /// priority fee on its requested limit.
    pub fn fee(&self) -> u64 {
        BASE_FEE * self.signatures
            + (self.cu_price as u128 * self.cu_limit as u128).div_ceil(1_000_000) as u64
    }

    /// What it pays per requested compute unit, its base fee included, in
    /// micro-lamports: what the scheduler orders by.
    pub fn effective_price(&self) -> u64 {
        let base = (BASE_FEE * self.signatures) as u128 * 1_000_000 / self.cu_limit.max(1) as u128;
        self.cu_price.saturating_add(base as u64)
    }
}

/// Where an entry stands.
#[derive(Debug, Clone, PartialEq)]
pub enum EntryState {
    /// Waiting for a slot it may land in.
    Pending,
    /// Its delivery was lost: it lands only if sent again.
    Lost,
    /// It ran, at the slot and index; `ok` unless it failed (and still paid
    /// its fee). A bundle lands whole or not at all.
    Landed { slot: u64, index: u32, ok: bool },
    /// A bundle the engine refused or the chain didn't commit.
    Failed { slot: u64, reason: String },
    /// Its blockhash expired before it landed.
    Expired { height: u64 },
}

/// When it was sent: the bot's turn and its sequence in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Arrival {
    pub step: u64,
    pub seq: u64,
}

/// A transaction or a bundle in the pool.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub id: u64,
    /// `bot`, or an actor's `actor:<name>/<i>`.
    pub sender: String,
    /// Its number among its sender's entries, from 0.
    pub sender_seq: u64,
    pub arrival: Arrival,
    pub route: RouteKind,
    /// A bundle's id (Jito's: the SHA-256 of its signatures), if it is one.
    pub bundle: Option<String>,
    pub txs: Vec<PooledTx>,
    /// The first slot it may land in.
    pub ready_slot: u64,
    /// The last block height its blockhash is valid at.
    pub expiry_height: u64,
    /// Rebroadcast until this slot if lost; the grader may sign it again
    /// once its blockhash expired (`resign`) and it never landed.
    pub retry_until_slot: Option<u64>,
    pub resign: bool,
    /// Deliveries made: the first send and each rebroadcast.
    pub attempts: u64,
    /// Whether the current delivery's loss draw was made.
    pub delivered: bool,
    /// A delivery the plan's faults drop: it reaches no leader.
    pub dropped: bool,
    pub state: EntryState,
    /// Signatures it had before the grader signed it again.
    pub resigned_from: Vec<String>,
}

impl Entry {
    pub fn is_bundle(&self) -> bool {
        self.bundle.is_some()
    }

    pub fn cu(&self) -> u64 {
        self.txs.iter().map(|t| t.cu_limit).sum()
    }

    pub fn tip(&self) -> u64 {
        self.txs.iter().map(|t| t.tip).sum()
    }

    pub fn fee(&self) -> u64 {
        self.txs.iter().map(PooledTx::fee).sum()
    }

    pub fn writable(&self) -> impl Iterator<Item = &Pubkey> {
        self.txs.iter().flat_map(|t| t.writable.iter())
    }

    pub fn is_pending(&self) -> bool {
        matches!(self.state, EntryState::Pending | EntryState::Lost)
    }
}

/// The pool's entries, in the order sent.
#[derive(Debug, Clone, Default)]
pub struct Pool {
    pub entries: Vec<Entry>,
}

impl Pool {
    /// Entries still waiting from `sender`.
    pub fn pending(&self, sender: &str) -> usize {
        self.entries
            .iter()
            .filter(|e| e.sender == sender && e.is_pending())
            .count()
    }

    /// The entry holding a transaction with `signature`, if any.
    pub fn find(&self, signature: &str) -> Option<&Entry> {
        self.entries.iter().find(|e| {
            e.txs.iter().any(|t| t.signature == signature)
                || e.resigned_from.iter().any(|s| s == signature)
        })
    }

    pub fn find_mut(&mut self, signature: &str) -> Option<&mut Entry> {
        self.entries
            .iter_mut()
            .find(|e| e.txs.iter().any(|t| t.signature == signature))
    }

    pub fn bundle(&self, id: &str) -> Option<&Entry> {
        self.entries
            .iter()
            .find(|e| e.bundle.as_deref() == Some(id))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use solana_keypair::Keypair;
    use solana_message::{Message, VersionedMessage};
    use solana_signer::Signer;
    use solana_transaction::{AccountMeta, Instruction};

    use super::*;

    /// A signed transaction from `payer` writing `writes`, with a price and
    /// limit if given, a tip if given, and a memo-like `salt` instruction
    /// whose bytes change nothing but the signature.
    pub(crate) fn signed(
        payer: &Keypair,
        writes: &[Pubkey],
        limit: Option<u32>,
        price: Option<u64>,
        tip: Option<u64>,
        salt: &[u8],
        blockhash: Hash,
    ) -> Vec<u8> {
        let mut ixs = Vec::new();
        if let Some(limit) = limit {
            let mut data = vec![2];
            data.extend(limit.to_le_bytes());
            ixs.push(Instruction::new_with_bytes(COMPUTE_BUDGET, &data, vec![]));
        }
        if let Some(price) = price {
            let mut data = vec![3];
            data.extend(price.to_le_bytes());
            ixs.push(Instruction::new_with_bytes(COMPUTE_BUDGET, &data, vec![]));
        }
        let mut accounts = vec![AccountMeta::new(payer.pubkey(), true)];
        accounts.extend(writes.iter().map(|w| AccountMeta::new(*w, false)));
        ixs.push(Instruction::new_with_bytes(
            Pubkey::new_from_array([7; 32]),
            salt,
            accounts,
        ));
        if let Some(tip) = tip {
            let mut data = 2u32.to_le_bytes().to_vec();
            data.extend(tip.to_le_bytes());
            ixs.push(Instruction::new_with_bytes(
                SYSTEM,
                &data,
                vec![
                    AccountMeta::new(payer.pubkey(), true),
                    AccountMeta::new(jito::TIP_ACCOUNTS[0].parse().unwrap(), false),
                ],
            ));
        }
        let message = Message::new_with_blockhash(&ixs, Some(&payer.pubkey()), &blockhash);
        let tx =
            VersionedTransaction::try_new(VersionedMessage::Legacy(message), &[payer]).unwrap();
        wincode::serialize(&tx).unwrap()
    }

    #[test]
    fn a_transaction_gives_its_price_limit_keys_and_tip() {
        let payer = Keypair::new();
        let pool = Pubkey::new_unique();
        let wire = signed(
            &payer,
            &[pool],
            Some(300_000),
            Some(25_000),
            Some(10_000),
            b"x",
            Hash::new_unique(),
        );
        let tx = PooledTx::parse(wire, &|_| None).unwrap();
        assert_eq!(
            (tx.cu_limit, tx.cu_price, tx.tip, tx.signatures),
            (300_000, 25_000, 10_000, 1)
        );
        assert!(tx.writable.contains(&pool) && tx.writable.contains(&payer.pubkey()));
        assert!(tx.readonly.contains(&COMPUTE_BUDGET));
        assert_eq!(tx.fee(), 5_000 + 7_500);
        assert!(!tx.dont_front);

        // Without a limit: 200k per instruction that isn't the budget's.
        let wire = signed(
            &payer,
            &[pool],
            None,
            None,
            Some(1),
            b"x",
            Hash::new_unique(),
        );
        let tx = PooledTx::parse(wire, &|_| None).unwrap();
        assert_eq!((tx.cu_limit, tx.cu_price), (400_000, 0));
        assert_eq!(tx.fee(), 5_000);
        assert!(PooledTx::parse(vec![1, 2, 3], &|_| None).is_err());
    }
}
