//! Raw transactions the grader signs (`send` in the strategy's world,
//! `docs/rfd/0002-wasm-strategies.md`): instructions a strategy builds
//! itself, for what the order sheet doesn't cover.
//!
//! The grader makes the transaction's fresh signers, resolves its derived
//! addresses, simulates it against the paused chain, signs it with the bot's
//! key and the fresh signers, and sends it after the step's sheet. No
//! session enables raw sends yet: [`send_raw`] returns the grader's refusal.

use crate::TxOptions;

/// The most fresh signers, derived addresses and seeds a raw transaction
/// may have, and the most bytes in a seed.
pub const MAX_FRESH_SIGNERS: u8 = 8;
pub const MAX_DERIVED: usize = 32;
pub const MAX_SEEDS: usize = 16;
pub const MAX_SEED_BYTES: usize = 32;

/// A transaction of raw instructions.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RawTransaction {
    pub instructions: Vec<RawInstruction>,
    /// Address lookup tables to compile it with, base58.
    pub lookup_tables: Vec<String>,
    /// How many fresh signers its instructions name ([`AccountKey::Fresh`]).
    pub fresh_signers: u8,
    /// Its derived addresses ([`AccountKey::Derived`]), resolved in order:
    /// each may name only the ones before it.
    pub derived: Vec<Derivation>,
    pub tx: Option<TxOptions>,
}

/// One instruction of a raw transaction.
#[derive(Debug, Clone, PartialEq)]
pub struct RawInstruction {
    /// The program, base58.
    pub program: String,
    pub accounts: Vec<RawAccount>,
    pub data: Vec<u8>,
}

/// One account of an instruction.
#[derive(Debug, Clone, PartialEq)]
pub struct RawAccount {
    pub key: AccountKey,
    pub signer: bool,
    pub writable: bool,
}

/// An account of a raw transaction.
#[derive(Debug, Clone, PartialEq)]
pub enum AccountKey {
    /// By its address, base58.
    Address(String),
    /// The bot's authority, which signs every raw transaction.
    Authority,
    /// One of the transaction's fresh signers, from 0: a key the grader
    /// makes for this transaction alone.
    Fresh(u8),
    /// One of the transaction's derived addresses, from 0.
    Derived(u16),
}

/// One seed of a program-derived address.
#[derive(Debug, Clone, PartialEq)]
pub enum Seed {
    Bytes(Vec<u8>),
    /// A key's 32 bytes.
    Key(AccountKey),
}

/// How a derived address is made.
#[derive(Debug, Clone, PartialEq)]
pub enum Derivation {
    /// The program's address from `seeds`, with the bump
    /// `find_program_address` finds.
    Pda {
        program: AccountKey,
        seeds: Vec<Seed>,
    },
    /// `owner`'s associated token account for `mint`, under
    /// `token_program`.
    Ata {
        owner: AccountKey,
        mint: AccountKey,
        token_program: AccountKey,
    },
}

/// What [`send_raw`] returns for a transaction the grader took.
#[derive(Debug, Clone, PartialEq)]
pub struct RawReceipt {
    /// Its number within the step: the `SendSource::Raw` of its status.
    pub id: u32,
    /// The fresh signers' addresses, base58, in order.
    pub fresh_signers: Vec<String>,
}

impl RawTransaction {
    /// Whether the transaction is well formed: within the limits, its keys
    /// in range, its derived addresses naming only the ones before them (so
    /// they resolve in one pass, with no cycle), and no signer but the
    /// authority and the fresh signers. The grader checks this before
    /// anything else; it doesn't say the transaction would succeed.
    pub fn check(&self) -> Result<(), String> {
        if self.fresh_signers > MAX_FRESH_SIGNERS {
            return Err(format!(
                "{} fresh signers: at most {MAX_FRESH_SIGNERS}",
                self.fresh_signers
            ));
        }
        if self.derived.len() > MAX_DERIVED {
            return Err(format!(
                "{} derived addresses: at most {MAX_DERIVED}",
                self.derived.len()
            ));
        }
        // A key is good if it names a fresh signer that exists, or a
        // derived address before `before`.
        let key = |key: &AccountKey, before: usize| match *key {
            AccountKey::Fresh(n) if n >= self.fresh_signers => Err(format!(
                "fresh signer {n}: the transaction has {}",
                self.fresh_signers
            )),
            AccountKey::Derived(n) if usize::from(n) >= before => {
                Err(format!("derived address {n} is named before it is made"))
            }
            _ => Ok(()),
        };
        for (i, derivation) in self.derived.iter().enumerate() {
            match derivation {
                Derivation::Pda { program, seeds } => {
                    key(program, i)?;
                    if seeds.len() > MAX_SEEDS {
                        return Err(format!(
                            "derived address {i}: {} seeds, at most {MAX_SEEDS}",
                            seeds.len()
                        ));
                    }
                    for seed in seeds {
                        match seed {
                            Seed::Bytes(bytes) if bytes.len() > MAX_SEED_BYTES => {
                                return Err(format!(
                                    "derived address {i}: a seed of {} bytes, at most {MAX_SEED_BYTES}",
                                    bytes.len()
                                ));
                            }
                            Seed::Bytes(_) => {}
                            Seed::Key(k) => key(k, i)?,
                        }
                    }
                }
                Derivation::Ata {
                    owner,
                    mint,
                    token_program,
                } => {
                    key(owner, i)?;
                    key(mint, i)?;
                    key(token_program, i)?;
                }
            }
        }
        for instruction in &self.instructions {
            for account in &instruction.accounts {
                key(&account.key, self.derived.len())?;
                if account.signer
                    && !matches!(account.key, AccountKey::Authority | AccountKey::Fresh(_))
                {
                    return Err(format!(
                        "{:?} can't sign: only the authority and fresh signers do",
                        account.key
                    ));
                }
            }
        }
        Ok(())
    }
}

/// Ask the grader to sign and send `tx` after this step's sheet: its
/// receipt, or why it was refused. Only a component can; and no session
/// enables raw sends yet.
pub fn send_raw(tx: &RawTransaction) -> Result<RawReceipt, String> {
    #[cfg(all(feature = "component", target_arch = "wasm32"))]
    {
        crate::component::send(tx)
    }
    #[cfg(not(all(feature = "component", target_arch = "wasm32")))]
    {
        let _ = tx;
        Err("raw sends need a strategy component".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(key: AccountKey, signer: bool) -> RawAccount {
        RawAccount {
            key,
            signer,
            writable: true,
        }
    }

    /// Raydium CLMM's `open_position_v2`, in shape: a fresh NFT mint, its
    /// position PDA and the authority's token account for it.
    fn open_position() -> RawTransaction {
        let program = AccountKey::Address("CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK".into());
        RawTransaction {
            fresh_signers: 1,
            derived: vec![
                Derivation::Pda {
                    program: program.clone(),
                    seeds: vec![
                        Seed::Bytes(b"position".to_vec()),
                        Seed::Key(AccountKey::Fresh(0)),
                    ],
                },
                Derivation::Ata {
                    owner: AccountKey::Authority,
                    mint: AccountKey::Fresh(0),
                    token_program: AccountKey::Address(
                        "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb".into(),
                    ),
                },
            ],
            instructions: vec![RawInstruction {
                program: "CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK".into(),
                accounts: vec![
                    account(AccountKey::Authority, true),
                    account(AccountKey::Fresh(0), true),
                    account(AccountKey::Derived(0), false),
                    account(AccountKey::Derived(1), false),
                ],
                data: vec![1, 2, 3],
            }],
            ..RawTransaction::default()
        }
    }

    #[test]
    fn a_fresh_mint_and_what_derives_from_it_check() {
        assert_eq!(open_position().check(), Ok(()));
    }

    #[test]
    fn malformed_transactions_are_refused() {
        let refused = |change: fn(&mut RawTransaction), why: &str| {
            let mut tx = open_position();
            change(&mut tx);
            let error = tx.check().unwrap_err();
            assert!(error.contains(why), "{error}");
        };
        // A derived address naming itself, or one after it: a cycle.
        refused(
            |tx| {
                tx.derived[0] = Derivation::Pda {
                    program: AccountKey::Authority,
                    seeds: vec![Seed::Key(AccountKey::Derived(0))],
                }
            },
            "named before it is made",
        );
        refused(
            |tx| {
                tx.derived[0] = Derivation::Ata {
                    owner: AccountKey::Derived(1),
                    mint: AccountKey::Fresh(0),
                    token_program: AccountKey::Authority,
                }
            },
            "named before it is made",
        );
        refused(
            |tx| tx.instructions[0].accounts[2].key = AccountKey::Derived(2),
            "named before it is made",
        );
        refused(|tx| tx.fresh_signers = 0, "fresh signer 0");
        refused(|tx| tx.fresh_signers = 9, "at most 8");
        refused(
            |tx| tx.instructions[0].accounts[2].signer = true,
            "can't sign",
        );
        refused(
            |tx| {
                tx.instructions[0]
                    .accounts
                    .push(account(AccountKey::Address("x".into()), true))
            },
            "can't sign",
        );
        refused(
            |tx| {
                tx.derived[0] = Derivation::Pda {
                    program: AccountKey::Authority,
                    seeds: vec![Seed::Bytes(vec![0; 33])],
                }
            },
            "33 bytes",
        );
        refused(
            |tx| {
                tx.derived = vec![
                    Derivation::Pda {
                        program: AccountKey::Authority,
                        seeds: vec![]
                    };
                    33
                ]
            },
            "33 derived",
        );
    }
}
