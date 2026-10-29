//! SPL token helpers: associated token accounts and native SOL wrapping.
//!
//! Most protocols only deal in SPL tokens, so using SOL means wrapping it:
//! move lamports into the owner's wrapped-SOL associated token account and
//! `sync_native` so the token balance reflects them. Exiting reverses it:
//! close the account and the lamports flow back to the owner.

use solana_account::Account;
use solana_program_pack::Pack;
use solana_pubkey::Pubkey;
use solana_transaction::Instruction;

use crate::chain::error::ChainError;

/// The wrapped-SOL mint.
pub fn native_mint() -> Pubkey {
    spl_token_interface::native_mint::id()
}

/// The SPL Token program.
pub fn token_program() -> Pubkey {
    spl_token_interface::id()
}

/// The Associated Token Account program.
pub fn associated_token_program() -> Pubkey {
    spl_associated_token_account_interface::program::id()
}

/// Associated token account of `owner` for `mint` under the SPL Token program.
pub fn ata(owner: &Pubkey, mint: &Pubkey) -> Pubkey {
    spl_associated_token_account_interface::address::get_associated_token_address(owner, mint)
}

/// Creates the ATA if it does not exist; a no-op otherwise.
pub fn create_ata_idempotent(payer: &Pubkey, owner: &Pubkey, mint: &Pubkey) -> Instruction {
    spl_associated_token_account_interface::instruction::create_associated_token_account_idempotent(
        payer,
        owner,
        mint,
        &token_program(),
    )
}

/// Instructions that put `lamports` of wrapped SOL into the owner's wSOL ATA.
///
/// With `create_ata` the ATA is created idempotently first. The transfer and
/// `sync_native` follow. Returns the ATA address alongside the instructions.
pub fn wrap_sol(owner: &Pubkey, lamports: u64, create_ata: bool) -> (Pubkey, Vec<Instruction>) {
    let account = ata(owner, &native_mint());
    let mut instructions = Vec::with_capacity(3);
    if create_ata {
        instructions.push(create_ata_idempotent(owner, owner, &native_mint()));
    }
    instructions.push(solana_system_interface::instruction::transfer(
        owner, &account, lamports,
    ));
    instructions.push(
        spl_token_interface::instruction::sync_native(&token_program(), &account)
            .expect("static program id and account are valid"),
    );
    (account, instructions)
}

/// Closes a token account owned by `owner`, sending its lamports (rent and,
/// for wSOL, the wrapped balance) back to `owner`.
pub fn close_token_account(owner: &Pubkey, account: &Pubkey) -> Instruction {
    spl_token_interface::instruction::close_account(&token_program(), account, owner, owner, &[])
        .expect("static program id and accounts are valid")
}

/// Parses an SPL token account's balance from raw account data.
pub fn token_balance(account: &Account) -> Result<u64, ChainError> {
    spl_token_interface::state::Account::unpack(&account.data)
        .map(|state| state.amount)
        .map_err(|e| ChainError::Decode {
            what: "token account",
            detail: e.to_string(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_sol_sequence() {
        let owner = Pubkey::new_unique();
        let (account, ixs) = wrap_sol(&owner, 1_500_000_000, true);
        assert_eq!(account, ata(&owner, &native_mint()));
        assert_eq!(ixs.len(), 3);
        assert_eq!(ixs[0].program_id, associated_token_program());
        assert_eq!(ixs[1].program_id, solana_system_interface::program::id());
        assert_eq!(ixs[2].program_id, token_program());
        // sync_native carries the account as its only account.
        assert_eq!(ixs[2].accounts.len(), 1);
        assert_eq!(ixs[2].accounts[0].pubkey, account);

        let (_, without) = wrap_sol(&owner, 1, false);
        assert_eq!(without.len(), 2);
        assert_eq!(
            without[0].program_id,
            solana_system_interface::program::id()
        );
    }

    #[test]
    fn ata_is_a_pda_of_the_ata_program() {
        let owner = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let (expected, _) = Pubkey::find_program_address(
            &[owner.as_ref(), token_program().as_ref(), mint.as_ref()],
            &associated_token_program(),
        );
        assert_eq!(ata(&owner, &mint), expected);
    }

    #[test]
    fn close_sends_to_owner() {
        let owner = Pubkey::new_unique();
        let account = Pubkey::new_unique();
        let ix = close_token_account(&owner, &account);
        assert_eq!(ix.program_id, token_program());
        assert_eq!(ix.accounts[0].pubkey, account);
        assert_eq!(ix.accounts[1].pubkey, owner);
        assert_eq!(ix.accounts[2].pubkey, owner);
        assert!(ix.accounts[2].is_signer);
    }

    #[test]
    fn token_balance_parses_and_rejects_garbage() {
        let state = spl_token_interface::state::Account {
            mint: native_mint(),
            owner: Pubkey::new_unique(),
            amount: 42,
            state: spl_token_interface::state::AccountState::Initialized,
            ..Default::default()
        };
        let mut data = vec![0u8; spl_token_interface::state::Account::LEN];
        state.pack_into_slice(&mut data);
        let account = Account {
            data,
            owner: token_program(),
            ..Account::default()
        };
        assert_eq!(token_balance(&account).unwrap(), 42);

        let garbage = Account {
            data: vec![1, 2, 3],
            ..Account::default()
        };
        assert!(matches!(
            token_balance(&garbage).unwrap_err(),
            ChainError::Decode { .. }
        ));
    }
}
