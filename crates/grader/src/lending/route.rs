//! What a route through a lending or an `amm` session needs to fit in a
//! transaction: address lookup tables holding every account of the
//! session's reserves, Scope feeds and pools that a push, a flash loan, a
//! liquidation and a swap name, and the bot's own token accounts. A lending
//! session's fit in one table; an `amm` session of many pools takes as many
//! as it needs ([`write_tables`]): the thin-pools snapshot's ten pools name
//! 279 addresses.
//!
//! RFD 0005 measured a flash cycle with a price push at 1,338 bytes without
//! one, past the 1,232 a transaction may have. The session writes the
//! table before its cheatcodes lock, as an account the lookup table
//! program owns, already active: a table created by instructions serves
//! its addresses only from the slot after it is extended. Every lending
//! and pool seat of the bot's compiles with it, and the session's venue
//! metadata names it (`lookup_table`) for a bot's raw routes.

use mtx_kit::chain::{AddressLookupTableAccount, ChainClient};
use solana_pubkey::Pubkey;
use solana_rpc_client::rpc_client::RpcClient;
use surfpool_sdk::Surfnet;

use crate::{
    Result,
    venue::{
        amm::Pool,
        kamino::{Books, klend, scope},
    },
};

/// Most addresses a lookup table holds (`LOOKUP_TABLE_MAX_ADDRESSES`).
pub const TABLE_ADDRESSES_MAX: usize = 256;
/// Most tables a session writes: [`crate::pools::POOLS_MAX`] pools of at
/// most 37 accounts each, and the bot's token accounts, fit in two.
pub const TABLES_MAX: usize = 4;
/// A table's metadata before its addresses (`LOOKUP_TABLE_META_SIZE`).
const TABLE_META_LEN: usize = 56;
/// Rent's lamports per byte-year, and the two years exemption takes, over
/// the 128 bytes every account is charged for besides its data.
const RENT_LAMPORTS_PER_BYTE_YEAR: u64 = 3_480;
const RENT_EXEMPT_YEARS: u64 = 2;
const ACCOUNT_STORAGE_OVERHEAD: u64 = 128;

/// The session's table: every account a route names that isn't the
/// signer's own key or a program it only invokes, in a stable order, once
/// each. `bot` is the strategy's key: its token accounts and obligation
/// go in too. An `amm` session has pools and no `books`.
pub async fn route_accounts(
    chain: &dyn ChainClient,
    rpc: &RpcClient,
    books: Option<&Books>,
    pools: &[Pool],
    bot: &Pubkey,
) -> Result<Vec<Pubkey>> {
    let mut accounts: Vec<Pubkey> = Vec::new();
    let mut add = |keys: &mut dyn Iterator<Item = Pubkey>| {
        for key in keys {
            if key != *bot && key != Pubkey::default() && !accounts.contains(&key) {
                accounts.push(key);
            }
        }
    };
    // A liquidation names its target and the target's farm accounts: a
    // stand-in target's are left out, since every target differs.
    let target = Pubkey::new_unique();
    let reserves = books.map(|b| b.reserves.as_slice()).unwrap_or_default();
    let target_farms: Vec<Pubkey> = reserves
        .iter()
        .flat_map(|b| [b.keys.farm_collateral, b.keys.farm_debt])
        .map(|farm| klend::obligation_farm(&farm, &target))
        .collect();
    for book in reserves {
        let keys = &book.keys;
        let reserve = chain
            .get_account(&keys.address)
            .await
            .map_err(|e| super::venue_error("lookup table", e))?
            .and_then(|a| klend::Reserve::decode(&a.data))
            .ok_or_else(|| super::venue_error("lookup table", "a reserve is gone"))?;
        let push = scope::refresh_reserve(chain, &reserve)
            .await
            .map_err(|e| super::venue_error("lookup table", e))?;
        let mut instructions = vec![
            push,
            klend::refresh_reserve(keys),
            klend::flash_borrow(bot, keys, 1),
            klend::flash_repay(bot, keys, 1, 0),
            klend::deposit(bot, keys, 1),
        ];
        for other in reserves {
            if other.keys.market == keys.market {
                instructions.push(klend::liquidate(bot, &target, keys, &other.keys, 1, 0));
                instructions.extend(klend::liquidator_accounts(bot, keys, &other.keys));
            }
        }
        add(&mut instructions
            .iter()
            .flat_map(|ix| ix.accounts.iter().map(|a| a.pubkey))
            .filter(|key| *key != target && !target_farms.contains(key)));
    }
    for pool in pools {
        add(&mut crate::pools::route_accounts(rpc, &pool.address)?.into_iter());
        let atas = [&pool.base, &pool.quote]
            .map(|t| mtx_kit::token::associated_token_address(bot, &t.mint, &t.token_program));
        add(&mut atas.into_iter());
    }
    Ok(accounts)
}

/// Write `addresses` into as few tables as hold them, in order, the `i`th at
/// `address(i)`. At most [`TABLES_MAX`].
pub fn write_tables(
    surfnet: &Surfnet,
    address: &dyn Fn(u32) -> Pubkey,
    addresses: &[Pubkey],
) -> Result<Vec<AddressLookupTableAccount>> {
    let count = addresses.len().div_ceil(TABLE_ADDRESSES_MAX).max(1);
    if count > TABLES_MAX {
        return Err(super::venue_error(
            "lookup table",
            format!(
                "the session's routes name {} accounts, more than {TABLES_MAX} tables hold",
                addresses.len()
            ),
        ));
    }
    let tables = (0..count)
        .map(|i| {
            let chunk = addresses
                .chunks(TABLE_ADDRESSES_MAX)
                .nth(i)
                .unwrap_or_default();
            write_table(surfnet, &address(i as u32), chunk)
        })
        .collect::<Result<Vec<_>>>()?;
    assert_eq!(
        tables.iter().map(|t| t.addresses.len()).sum::<usize>(),
        addresses.len()
    );
    Ok(tables)
}

/// Write the table at `address` holding `addresses`, active, with no
/// authority: nobody extends or closes it during the session.
pub fn write_table(
    surfnet: &Surfnet,
    address: &Pubkey,
    addresses: &[Pubkey],
) -> Result<AddressLookupTableAccount> {
    if addresses.len() > TABLE_ADDRESSES_MAX {
        return Err(super::venue_error(
            "lookup table",
            format!(
                "the session's routes name {} accounts, more than a table's {TABLE_ADDRESSES_MAX}",
                addresses.len()
            ),
        ));
    }
    let data = table_data(addresses);
    let lamports = rent_exempt_lamports(data.len());
    surfnet.cheatcodes().set_account(
        address,
        lamports,
        &data,
        &mtx_kit::chain::lookup::program(),
    )?;
    let table = mtx_kit::chain::lookup::decode(address, &mtx_kit::chain::lookup::program(), &data)
        .map_err(|e| super::venue_error("lookup table", e))?;
    assert_eq!(table.addresses, addresses, "the table reads back");
    Ok(table)
}

/// A lookup table's account data: never deactivated, last extended at slot
/// 0 (so every address is active from the start), no authority, then the
/// addresses.
fn table_data(addresses: &[Pubkey]) -> Vec<u8> {
    assert!(addresses.len() <= TABLE_ADDRESSES_MAX);
    let mut data = vec![0u8; TABLE_META_LEN];
    // `ProgramState::LookupTable`, a little-endian u32.
    data[0] = 1;
    data[4..12].copy_from_slice(&u64::MAX.to_le_bytes());
    for address in addresses {
        data.extend_from_slice(address.as_ref());
    }
    data
}

fn rent_exempt_lamports(data_len: usize) -> u64 {
    (ACCOUNT_STORAGE_OVERHEAD + data_len as u64) * RENT_LAMPORTS_PER_BYTE_YEAR * RENT_EXEMPT_YEARS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_written_table_reads_back_as_the_program_lays_it_out() {
        let addresses: Vec<Pubkey> = (0..3).map(|_| Pubkey::new_unique()).collect();
        let data = table_data(&addresses);
        assert_eq!(data.len(), TABLE_META_LEN + 3 * 32);
        let key = Pubkey::new_unique();
        let table = mtx_kit::chain::lookup::decode(&key, &mtx_kit::chain::lookup::program(), &data)
            .unwrap();
        assert_eq!(table.addresses, addresses);
        // (128 + 152 bytes) at 3,480 lamports a byte-year, for two years.
        assert_eq!(rent_exempt_lamports(data.len()), 1_948_800);
        assert_eq!(table_data(&[]).len(), TABLE_META_LEN);
    }

    #[test]
    #[should_panic]
    fn a_table_past_its_size_is_refused() {
        table_data(&vec![Pubkey::new_unique(); TABLE_ADDRESSES_MAX + 1]);
    }
}
