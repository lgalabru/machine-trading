//! SPL token helpers: associated token accounts, token account state,
//! transfers and native SOL wrapping, under SPL Token or Token-2022.
//!
//! A mint's owner says which program it is under; [`read_mint`] reads it,
//! with its Token-2022 extensions ([`crate::token::Mint`]). The `_for`
//! functions take that program; the others are SPL Token's.
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

/// Associated token account of `owner` for `mint` under `token_program`.
pub fn ata_for(owner: &Pubkey, mint: &Pubkey, token_program: &Pubkey) -> Pubkey {
    crate::token::associated_token_address(owner, mint, token_program)
}

/// [`create_ata_idempotent`] under `token_program`.
pub fn create_ata_idempotent_for(
    payer: &Pubkey,
    owner: &Pubkey,
    mint: &Pubkey,
    token_program: &Pubkey,
) -> Instruction {
    spl_associated_token_account_interface::instruction::create_associated_token_account_idempotent(
        payer,
        owner,
        mint,
        token_program,
    )
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

/// An SPL token account's state, as the token program keeps it: mint,
/// owner, amount, delegate and delegated amount, state, and `is_native`'s
/// rent-exempt reserve for wrapped SOL.
pub use spl_token_interface::state::Account as TokenAccount;

/// Decodes a token account under either program: a Token-2022 account's
/// base fields, whatever extensions follow them.
pub fn token_account(account: &Account) -> Result<TokenAccount, ChainError> {
    let base = if account.owner == crate::token::TOKEN_2022_PROGRAM_ID
        && account.data.len() > TokenAccount::LEN
    {
        // Checks the account type and the extensions' bounds.
        crate::token::TokenAccountInfo::decode(&account.owner, &account.data)?;
        &account.data[..TokenAccount::LEN]
    } else {
        &account.data[..]
    };
    TokenAccount::unpack(base).map_err(|e| ChainError::Decode {
        what: "token account",
        detail: e.to_string(),
    })
}

/// The mint at `mint`, under whichever token program owns it.
pub async fn read_mint(
    chain: &dyn crate::chain::ChainClient,
    mint: &Pubkey,
) -> Result<crate::token::Mint, ChainError> {
    let account = chain
        .get_account(mint)
        .await?
        .ok_or(ChainError::AccountNotFound(*mint))?;
    Ok(crate::token::Mint::decode(&account.owner, &account.data)?)
}

/// [`transfer_checked`] under `token_program`. Token-2022's
/// `TransferChecked` has SPL Token's accounts and data; on a mint with a
/// transfer fee the destination receives the amount less the fee
/// ([`crate::token::Mint::received`]).
pub fn transfer_checked_for(
    token_program: &Pubkey,
    source: &Pubkey,
    mint: &Pubkey,
    destination: &Pubkey,
    owner: &Pubkey,
    amount: u64,
    decimals: u8,
) -> Instruction {
    let mut ix = transfer_checked(source, mint, destination, owner, amount, decimals);
    ix.program_id = *token_program;
    ix
}

/// `SyncNative`: makes a wrapped-SOL account's balance match its lamports
/// above the reserve, after SOL was sent to it.
pub fn sync_native(account: &Pubkey) -> Instruction {
    spl_token_interface::instruction::sync_native(&token_program(), account)
        .expect("static program id and account are valid")
}

/// `TransferChecked` of `amount` native units of `mint` from `source` to
/// `destination`, signed by `owner`.
pub fn transfer_checked(
    source: &Pubkey,
    mint: &Pubkey,
    destination: &Pubkey,
    owner: &Pubkey,
    amount: u64,
    decimals: u8,
) -> Instruction {
    spl_token_interface::instruction::transfer_checked(
        &token_program(),
        source,
        mint,
        destination,
        owner,
        &[],
        amount,
        decimals,
    )
    .expect("static program id and accounts are valid")
}

/// What `owner` holds of `mint`, in atoms, across every token account it
/// owns for the mint: what an enforcer counts as its wallet.
pub async fn owned_amount(
    chain: &dyn crate::chain::ChainClient,
    owner: &Pubkey,
    mint: &Pubkey,
) -> Result<u64, ChainError> {
    let mut atoms = 0u64;
    for (_, account) in chain.get_token_accounts_by_owner(owner, mint).await? {
        atoms = atoms
            .checked_add(token_account(&account)?.amount)
            .ok_or_else(|| ChainError::Decode {
                what: "token account",
                detail: "balances overflow u64".into(),
            })?;
    }
    Ok(atoms)
}

/// Parses an SPL token account's balance from raw account data.
pub fn token_balance(account: &Account) -> Result<u64, ChainError> {
    Ok(token_account(account)?.amount)
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
    fn token_account_state_parses() {
        let owner = Pubkey::new_unique();
        let state = spl_token_interface::state::Account {
            mint: native_mint(),
            owner,
            amount: 7,
            is_native: Some(2_039_280).into(),
            state: spl_token_interface::state::AccountState::Initialized,
            ..Default::default()
        };
        let mut data = vec![0u8; spl_token_interface::state::Account::LEN];
        state.pack_into_slice(&mut data);
        let parsed = token_account(&Account {
            data,
            owner: token_program(),
            ..Account::default()
        })
        .unwrap();
        assert_eq!(parsed.owner, owner);
        assert_eq!(parsed.amount, 7);
        assert_eq!(Option::<u64>::from(parsed.is_native), Some(2_039_280));
        assert!(parsed.delegate.is_none());
        assert!(!parsed.is_frozen());

        let ix = transfer_checked(
            &Pubkey::new_unique(),
            &native_mint(),
            &Pubkey::new_unique(),
            &owner,
            5,
            9,
        );
        assert_eq!(ix.data[0], 12);
        assert_eq!(sync_native(&owner).data, vec![17]);
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

    #[test]
    fn token_2022_accounts_and_transfers() {
        use spl_token_2022_interface::{
            extension::{
                BaseStateWithExtensionsMut, ExtensionType, StateWithExtensionsMut,
                immutable_owner::ImmutableOwner,
            },
            state::{Account as Account2022, AccountState as State2022},
        };
        let owner = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let len = ExtensionType::try_calculate_account_len::<Account2022>(&[
            ExtensionType::ImmutableOwner,
        ])
        .unwrap();
        let mut data = vec![0; len];
        let mut state =
            StateWithExtensionsMut::<Account2022>::unpack_uninitialized(&mut data).unwrap();
        state.init_extension::<ImmutableOwner>(true).unwrap();
        state.base = Account2022 {
            mint,
            owner,
            amount: 11,
            state: State2022::Initialized,
            ..Account2022::default()
        };
        state.pack_base();
        state.init_account_type().unwrap();
        let extended = Account {
            data,
            owner: crate::token::TOKEN_2022_PROGRAM_ID,
            ..Account::default()
        };
        let parsed = token_account(&extended).unwrap();
        assert_eq!(
            (parsed.mint, parsed.owner, parsed.amount),
            (mint, owner, 11)
        );
        // The same bytes under SPL Token are too long for its layout.
        let classic = Account {
            owner: token_program(),
            ..extended.clone()
        };
        assert!(token_account(&classic).is_err());

        let program = crate::token::TOKEN_2022_PROGRAM_ID;
        let account = ata_for(&owner, &mint, &program);
        assert_ne!(account, ata(&owner, &mint));
        let create = create_ata_idempotent_for(&owner, &owner, &mint, &program);
        assert_eq!(create.program_id, associated_token_program());
        assert_eq!(create.accounts[1].pubkey, account);
        assert_eq!(create.accounts[5].pubkey, program);
        let ix = transfer_checked_for(
            &program,
            &account,
            &mint,
            &Pubkey::new_unique(),
            &owner,
            5,
            6,
        );
        assert_eq!(ix.program_id, program);
        assert_eq!(
            ix.data,
            transfer_checked(&account, &mint, &ix.accounts[2].pubkey, &owner, 5, 6).data
        );
    }
}
