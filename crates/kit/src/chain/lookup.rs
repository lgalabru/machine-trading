//! Address lookup tables, for v0 transactions: reading one into what
//! [`pack_with_lookup_tables`](crate::chain::pack_with_lookup_tables) takes,
//! and the instructions that create and extend one.
//!
//! A table extended in a slot serves its new addresses from the next slot
//! on: create and extend tables during setup, a slot before they're used.

use solana_address_lookup_table_interface::{instruction, state::AddressLookupTable};
use solana_pubkey::Pubkey;
use solana_transaction::Instruction;

pub use super::tx::AddressLookupTableAccount;
use crate::chain::{ChainClient, ChainError};

/// The address lookup table program.
pub fn program() -> Pubkey {
    solana_address_lookup_table_interface::program::id()
}

/// The lookup table at `address`, as v0 messages compile against it.
pub async fn lookup_table(
    chain: &dyn ChainClient,
    address: &Pubkey,
) -> Result<AddressLookupTableAccount, ChainError> {
    let account = chain
        .get_account(address)
        .await?
        .ok_or(ChainError::AccountNotFound(*address))?;
    decode(address, &account.owner, &account.data)
}

/// A lookup table from its account's owner and data.
pub fn decode(
    address: &Pubkey,
    owner: &Pubkey,
    data: &[u8],
) -> Result<AddressLookupTableAccount, ChainError> {
    let error = |detail: String| ChainError::Decode {
        what: "address lookup table",
        detail,
    };
    if *owner != program() {
        return Err(error(format!("{address} is owned by {owner}")));
    }
    let table = AddressLookupTable::deserialize(data).map_err(|e| error(e.to_string()))?;
    Ok(AddressLookupTableAccount {
        key: *address,
        addresses: table.addresses.to_vec(),
    })
}

/// Creates a table owned by `authority`, paid by `payer`, at an address
/// derived from `recent_slot` (a slot the chain still remembers). Returns
/// the instruction and the table's address.
pub fn create(authority: &Pubkey, payer: &Pubkey, recent_slot: u64) -> (Instruction, Pubkey) {
    instruction::create_lookup_table(*authority, *payer, recent_slot)
}

/// Appends `addresses` to `table`; `payer` funds the extra rent.
pub fn extend(
    table: &Pubkey,
    authority: &Pubkey,
    payer: &Pubkey,
    addresses: Vec<Pubkey>,
) -> Instruction {
    instruction::extend_lookup_table(*table, *authority, Some(*payer), addresses)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_table_and_refuses_other_accounts() {
        let address = Pubkey::new_unique();
        let addresses = vec![Pubkey::new_unique(), Pubkey::new_unique()];
        // The 56-byte meta (a lookup table, never deactivated, no
        // authority), then the addresses.
        let mut data = vec![0; 56];
        data[0] = 1;
        data[4..12].copy_from_slice(&u64::MAX.to_le_bytes());
        for a in &addresses {
            data.extend_from_slice(a.as_ref());
        }
        let table = decode(&address, &program(), &data).unwrap();
        assert_eq!(table.key, address);
        assert_eq!(table.addresses, addresses);
        assert!(decode(&address, &Pubkey::new_unique(), &data).is_err());
        assert!(decode(&address, &program(), &data[..10]).is_err());

        let authority = Pubkey::new_unique();
        let (ix, at) = create(&authority, &authority, 7);
        assert_eq!(ix.program_id, program());
        assert_eq!(ix.accounts[0].pubkey, at);
        let ix = extend(&at, &authority, &authority, addresses);
        assert_eq!(ix.accounts[0].pubkey, at);
    }
}
