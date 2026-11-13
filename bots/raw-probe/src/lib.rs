//! A probe of raw sends (`send` in `mtx:strategy@0.2.0`): at its first step
//! it tries one of each kind, allowed and not, and logs what the grader
//! answered; at the next it logs what became of the ones taken. It never
//! trades on its sheet.
//!
//! Its scenario (`scenario.json`) lets raw sends run the system, token and
//! associated token programs, under a drawdown limit of $50. The grader's
//! tests grade it in a session and read its log; `just raw-probe` does the
//! same by hand.

use mtx_kit::{
    SendSource, log,
    prelude::*,
    raw::{AccountKey, Derivation, RawAccount, RawInstruction, RawTransaction, Seed},
    send_raw,
};

const SYSTEM: &str = "11111111111111111111111111111111";
const TOKEN: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
const ASSOCIATED_TOKEN: &str = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL";
const MEMO: &str = "MemoSq4gqABAXKb96qnH83TBF8eB5RvwJGAtnwGk8bv";
const USDC: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";

#[derive(Default)]
struct Probe;

fn account(key: AccountKey, signer: bool, writable: bool) -> RawAccount {
    RawAccount {
        key,
        signer,
        writable,
    }
}

fn address(address: &str) -> AccountKey {
    AccountKey::Address(address.into())
}

/// The system program's `CreateAccount`: `lamports` into a new account of
/// no data, owned by the system program.
fn create_account(new: AccountKey, lamports: u64) -> RawInstruction {
    let mut data = 0u32.to_le_bytes().to_vec();
    data.extend_from_slice(&lamports.to_le_bytes());
    data.extend_from_slice(&0u64.to_le_bytes());
    data.extend_from_slice(&[0; 32]);
    RawInstruction {
        program: SYSTEM.into(),
        accounts: vec![
            account(AccountKey::Authority, true, true),
            account(new, true, true),
        ],
        data,
    }
}

/// The associated token program's `CreateIdempotent`: `owner`'s USDC
/// account, at `ata`, under `token_program`.
fn create_usdc_account(ata: AccountKey, owner: AccountKey, token_program: &str) -> RawInstruction {
    RawInstruction {
        program: ASSOCIATED_TOKEN.into(),
        accounts: vec![
            account(AccountKey::Authority, true, true),
            account(ata, false, true),
            account(owner, false, false),
            account(address(USDC), false, false),
            account(address(SYSTEM), false, false),
            account(address(token_program), false, false),
        ],
        data: vec![1],
    }
}

/// The token program's `Transfer` of `atoms` from `from`, which the
/// authority owns.
fn transfer(from: AccountKey, to: AccountKey, atoms: u64) -> RawInstruction {
    let mut data = vec![3];
    data.extend_from_slice(&atoms.to_le_bytes());
    RawInstruction {
        program: TOKEN.into(),
        accounts: vec![
            account(from, false, true),
            account(to, false, true),
            account(AccountKey::Authority, true, false),
        ],
        data,
    }
}

fn usdc_account(owner: AccountKey) -> Derivation {
    Derivation::Ata {
        owner,
        mint: address(USDC),
        token_program: address(TOKEN),
    }
}

/// `atoms` of the authority's USDC to a new owner's account, made in the
/// same transaction: the wallet is derived address 0, the new account 1.
fn pay_a_fresh_owner(atoms: u64) -> RawTransaction {
    RawTransaction {
        instructions: vec![
            create_usdc_account(AccountKey::Derived(1), AccountKey::Fresh(0), TOKEN),
            transfer(AccountKey::Derived(0), AccountKey::Derived(1), atoms),
        ],
        fresh_signers: 1,
        derived: vec![
            usdc_account(AccountKey::Authority),
            usdc_account(AccountKey::Fresh(0)),
        ],
        ..RawTransaction::default()
    }
}

fn send(what: &str, tx: &RawTransaction) {
    log(&format!("{what}: {:?}", send_raw(tx)));
}

/// One of each kind of raw send.
fn every_kind() {
    // Taken: a new account, signed for by a fresh signer; and a dollar to
    // a new owner, its account made by the associated token program.
    send(
        "create",
        &RawTransaction {
            instructions: vec![create_account(AccountKey::Fresh(0), 1_000_000)],
            fresh_signers: 1,
            ..RawTransaction::default()
        },
    );
    send("pay", &pay_a_fresh_owner(1_000_000));
    // Refused: a program off the list.
    send(
        "unlisted",
        &RawTransaction {
            instructions: vec![RawInstruction {
                program: MEMO.into(),
                accounts: vec![],
                data: b"hello".to_vec(),
            }],
            ..RawTransaction::default()
        },
    );
    // Refused: a listed program running one off the list, as its token
    // program.
    send(
        "cpi",
        &RawTransaction {
            instructions: vec![create_usdc_account(
                AccountKey::Derived(0),
                AccountKey::Fresh(0),
                MEMO,
            )],
            fresh_signers: 1,
            derived: vec![Derivation::Ata {
                owner: AccountKey::Fresh(0),
                mint: address(USDC),
                token_program: address(MEMO),
            }],
            ..RawTransaction::default()
        },
    );
    // Refused: $60 out of the wallet, past the $50 drawdown limit.
    send("drain", &pay_a_fresh_owner(60_000_000));
    // Refused: a signer other than the authority and the fresh ones.
    let mut as_another = pay_a_fresh_owner(1);
    as_another.instructions[1].accounts[2] = account(address(USDC), true, false);
    send("as another", &as_another);
    // Refused: too big for a packet.
    send(
        "too big",
        &RawTransaction {
            instructions: vec![RawInstruction {
                program: SYSTEM.into(),
                accounts: vec![],
                data: vec![0; 1_300],
            }],
            ..RawTransaction::default()
        },
    );
    // Refused, never panicked on: malformed in every way the shape allows.
    send(
        "not an address",
        &RawTransaction {
            instructions: vec![RawInstruction {
                program: "not an address".into(),
                accounts: vec![],
                data: vec![],
            }],
            ..RawTransaction::default()
        },
    );
    send(
        "long seed",
        &RawTransaction {
            instructions: vec![RawInstruction {
                program: SYSTEM.into(),
                accounts: vec![account(AccountKey::Derived(0), false, false)],
                data: vec![],
            }],
            derived: vec![Derivation::Pda {
                program: address(SYSTEM),
                seeds: vec![Seed::Bytes(vec![1; 33])],
            }],
            ..RawTransaction::default()
        },
    );
    send("empty", &RawTransaction::default());
}

impl Strategy for Probe {
    fn start(&mut self, session: &Session) -> BotResult {
        log(&format!(
            "programs {:?}",
            session.var("MTX_RAW_PROGRAMS").unwrap_or_default()
        ));
        send("in start", &RawTransaction::default());
        Ok(())
    }

    fn step(&mut self, market: &Market, _orders: &mut Orders) -> BotResult {
        for sent in &market.sends {
            if let SendSource::Raw(id) = sent.source {
                log(&format!(
                    "raw {id} of step {}: {:?}, slot {:?}, fee {:?}",
                    sent.step, sent.state, sent.slot, sent.fee
                ));
            }
        }
        match market.k {
            0 => every_kind(),
            // A fresh signer is the step's own: the same send, a new key.
            1 => send(
                "create again",
                &RawTransaction {
                    instructions: vec![create_account(AccountKey::Fresh(0), 1_000_000)],
                    fresh_signers: 1,
                    ..RawTransaction::default()
                },
            ),
            _ => {}
        }
        Ok(())
    }

    fn end(&mut self, _last: Option<&Market>) {}
}

mtx_kit::export!(Probe);
