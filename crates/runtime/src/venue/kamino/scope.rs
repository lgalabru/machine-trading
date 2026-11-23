//! Scope, the oracle Kamino's reserves price with: the one instruction a
//! strategy's `push-price` sends, `refresh_price_list`, which recomputes
//! Scope's prices for a list of entries from their sources
//! (`docs/adr/0004-atomic-arbitrage-in-the-sheet.md`, "An oracle bots can
//! push").
//!
//! Laid out as Scope's own source has it (`programs/scope/src`,
//! `handlers/handler_refresh_prices.rs`) and as mainnet's keepers send it
//! (2026-11-21): the Anchor tag of `global:refresh_price_list`, then the
//! entries as a Borsh `Vec<u16>`; the accounts `oracle_prices` (writable),
//! `oracle_mappings`, `oracle_twaps` (writable) and the instructions
//! sysvar, then one account per entry, the mapping's `price_info_accounts`
//! for it. Scope refuses the refresh unless every instruction before it in
//! the transaction is a compute-budget instruction.
//!
//! The grader's keeper and liquidators, and its tests, push through the
//! same builder: there is one [`refresh_price_list`].

use mtx_kit::chain::ChainClient;
use solana_pubkey::{Pubkey, pubkey};
use solana_rpc_client_api::filter::{Memcmp, RpcFilterType};
use solana_transaction::{AccountMeta, Instruction};

use super::klend::Reserve;

/// Scope on mainnet.
pub const PROGRAM_ID: Pubkey = pubkey!("HFn8GnPADiny6XqUoWE8uRPPxb29ikn4yTuPa9MF2fWJ");

const INSTRUCTIONS_SYSVAR: Pubkey = pubkey!("Sysvar1nstructions1111111111111111111111111");

/// Entries one Scope feed holds (`MAX_ENTRIES`).
pub const ENTRIES_MAX: usize = 512;

/// A reserve's price chain holds at most four entries; `u16::MAX` marks an
/// unused one.
const UNUSED_ENTRY: u16 = u16::MAX;

/// Where an `OraclePrices` account names its mappings, and where an
/// `OracleTwaps` account names its prices and its mappings: after the
/// 8-byte discriminator.
const PRICES_MAPPINGS_AT: usize = 8;
const TWAPS_PRICES_AT: usize = 8;
const TWAPS_MAPPINGS_AT: usize = 40;

/// Where `OracleMappings::price_info_accounts` starts: one key per entry.
const MAPPINGS_PRICE_ACCOUNTS_AT: usize = 8;

/// The accounts of one Scope feed a refresh names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Feed {
    pub prices: Pubkey,
    pub mappings: Pubkey,
    pub twaps: Pubkey,
}

/// `refresh_price_list` for `tokens` of `feed`, each read from its source
/// account in `sources` (the mapping's `price_info_accounts` entry for it,
/// in the same order).
pub fn refresh_price_list(feed: &Feed, tokens: &[u16], sources: &[Pubkey]) -> Instruction {
    assert!(!tokens.is_empty(), "Scope refuses an empty list");
    assert!(
        tokens.len() <= ENTRIES_MAX,
        "a feed has {ENTRIES_MAX} entries"
    );
    assert_eq!(tokens.len(), sources.len(), "one source account per entry");
    let mut data = super::klend::instruction_tag("refresh_price_list").to_vec();
    let count = u32::try_from(tokens.len()).expect("at most 512 entries");
    data.extend(count.to_le_bytes());
    for token in tokens {
        data.extend(token.to_le_bytes());
    }
    let mut accounts = vec![
        AccountMeta::new(feed.prices, false),
        AccountMeta::new_readonly(feed.mappings, false),
        AccountMeta::new(feed.twaps, false),
        AccountMeta::new_readonly(INSTRUCTIONS_SYSVAR, false),
    ];
    accounts.extend(sources.iter().map(|s| AccountMeta::new_readonly(*s, false)));
    Instruction {
        program_id: PROGRAM_ID,
        accounts,
        data,
    }
}

/// The entries `reserve` prices with: its price chain's, then its TWAP
/// chain's, unused ones left out, each once. klend reads both at every
/// refresh: a TWAP entry left to age past `max_age_twap_seconds` degrades
/// the reserve's price status, so a push refreshes the two together.
pub fn price_entries(reserve: &Reserve) -> Vec<u16> {
    let mut entries: Vec<u16> = Vec::with_capacity(reserve.price_chain.len() * 2);
    for entry in reserve.price_chain.iter().chain(&reserve.twap_chain) {
        if *entry != UNUSED_ENTRY && !entries.contains(entry) {
            entries.push(*entry);
        }
    }
    assert!(entries.len() <= 2 * reserve.price_chain.len());
    entries
}

/// The refresh of `reserve`'s price and TWAP entries, its feed and their sources
/// read through `chain`: the prices account the reserve's config names,
/// the mappings it names, and the TWAPs account naming both.
pub async fn refresh_reserve(
    chain: &dyn ChainClient,
    reserve: &Reserve,
) -> Result<Instruction, String> {
    let tokens = price_entries(reserve);
    if tokens.is_empty() {
        return Err("the reserve has no Scope price entries".into());
    }
    let feed = feed(chain, &reserve.scope_prices).await?;
    let mappings = read(chain, &feed.mappings, "Scope's mappings").await?;
    let sources = tokens
        .iter()
        .map(|token| source(&mappings, *token))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(refresh_price_list(&feed, &tokens, &sources))
}

/// The feed whose prices account is `prices`: its mappings, from the prices
/// account, and the TWAPs account that names both.
pub async fn feed(chain: &dyn ChainClient, prices: &Pubkey) -> Result<Feed, String> {
    let data = read(chain, prices, "Scope's prices").await?;
    let mappings = key_at(&data, PRICES_MAPPINGS_AT).ok_or("Scope's prices are too short")?;
    let filters = vec![
        RpcFilterType::Memcmp(Memcmp::new_raw_bytes(
            TWAPS_PRICES_AT,
            prices.to_bytes().to_vec(),
        )),
        RpcFilterType::Memcmp(Memcmp::new_raw_bytes(
            TWAPS_MAPPINGS_AT,
            mappings.to_bytes().to_vec(),
        )),
    ];
    let twaps = chain
        .get_program_accounts(&PROGRAM_ID, filters)
        .await
        .map_err(|e| format!("Scope's TWAPs: {e}"))?;
    match twaps.as_slice() {
        [(twaps, _)] => Ok(Feed {
            prices: *prices,
            mappings,
            twaps: *twaps,
        }),
        [] => Err(format!("no Scope TWAPs account for the prices {prices}")),
        _ => Err(format!(
            "several Scope TWAPs accounts for the prices {prices}"
        )),
    }
}

/// The source account the mappings name for `token`.
fn source(mappings: &[u8], token: u16) -> Result<Pubkey, String> {
    if usize::from(token) >= ENTRIES_MAX {
        return Err(format!(
            "Scope entry {token} is past the feed's {ENTRIES_MAX}"
        ));
    }
    key_at(
        mappings,
        MAPPINGS_PRICE_ACCOUNTS_AT + 32 * usize::from(token),
    )
    .ok_or_else(|| "Scope's mappings are too short".into())
}

async fn read(chain: &dyn ChainClient, key: &Pubkey, what: &str) -> Result<Vec<u8>, String> {
    let account = chain
        .get_account(key)
        .await
        .map_err(|e| format!("{what}: {e}"))?
        .ok_or_else(|| format!("{what}: no account at {key}"))?;
    if account.owner != PROGRAM_ID {
        return Err(format!("{what}: {key} isn't Scope's"));
    }
    Ok(account.data)
}

fn key_at(data: &[u8], at: usize) -> Option<Pubkey> {
    let bytes: [u8; 32] = data.get(at..at + 32)?.try_into().ok()?;
    Some(Pubkey::new_from_array(bytes))
}

#[cfg(test)]
mod tests {
    use mtx_kit::chain::mock::MockChainClient;
    use solana_account::Account;

    use super::*;
    use crate::venue::kamino::klend;

    /// The main market's SOL feed on mainnet (2026-11-21), and a keeper's
    /// refresh of entries 3, 10, 42 and 51 there: tag, then a `Vec<u16>`.
    const MAINNET_PRICES: Pubkey = pubkey!("3NJYftD5sjVfxSnUdZ1wVML8f3aC6mp1CXCL6L7TnU8C");
    const MAINNET_MAPPINGS: Pubkey = pubkey!("Chpu5ZgfWX5ZzVpUx9Xvv4WPM75Xd7zPJNDPsFnCpLpk");
    const MAINNET_TWAPS: Pubkey = pubkey!("GbpsVomudPRRwmqfTmo3MYQVTikPG6QXxqpzJexA1JRb");

    #[test]
    fn a_refresh_is_laid_out_as_mainnet_sends_it() {
        let feed = Feed {
            prices: MAINNET_PRICES,
            mappings: MAINNET_MAPPINGS,
            twaps: MAINNET_TWAPS,
        };
        let sources: Vec<Pubkey> = (0..4).map(|_| Pubkey::new_unique()).collect();
        let ix = refresh_price_list(&feed, &[3, 10, 42, 51], &sources);
        assert_eq!(ix.program_id, PROGRAM_ID);
        assert_eq!(ix.data[..8], [83, 186, 207, 131, 203, 254, 198, 130]);
        assert_eq!(
            ix.data[8..],
            [4, 0, 0, 0, 3, 0, 10, 0, 42, 0, 51, 0],
            "the keeper's own bytes"
        );
        let keys: Vec<(Pubkey, bool)> = ix
            .accounts
            .iter()
            .map(|a| (a.pubkey, a.is_writable))
            .collect();
        assert_eq!(
            keys[..4],
            [
                (MAINNET_PRICES, true),
                (MAINNET_MAPPINGS, false),
                (MAINNET_TWAPS, true),
                (INSTRUCTIONS_SYSVAR, false),
            ]
        );
        assert_eq!(ix.accounts.len(), 8);
        assert!(ix.accounts.iter().all(|a| !a.is_signer), "anyone may push");
        assert_eq!(
            ix.accounts[4..]
                .iter()
                .map(|a| a.pubkey)
                .collect::<Vec<_>>(),
            sources
        );
    }

    /// The SOL reserve of the main market prices with entry 3 and reads
    /// entry 455 as its TWAP: a push refreshes both, each once.
    #[test]
    fn a_push_refreshes_the_price_and_the_twap_entries() {
        let mut data = vec![0u8; klend::RESERVE_LEN];
        data[..8].copy_from_slice(&klend::account_tag("Reserve"));
        let chains = [
            3u16,
            UNUSED_ENTRY,
            UNUSED_ENTRY,
            UNUSED_ENTRY,
            455,
            3,
            UNUSED_ENTRY,
            0,
        ];
        for (i, entry) in chains.iter().enumerate() {
            data[5144 + 2 * i..5146 + 2 * i].copy_from_slice(&entry.to_le_bytes());
        }
        let reserve = Reserve::decode(&data).expect("a reserve");
        assert_eq!(price_entries(&reserve), [3, 455, 0]);
        let unused = [UNUSED_ENTRY; 8];
        for (i, entry) in unused.iter().enumerate() {
            data[5144 + 2 * i..5146 + 2 * i].copy_from_slice(&entry.to_le_bytes());
        }
        let none = Reserve::decode(&data).expect("a reserve");
        assert!(price_entries(&none).is_empty());
    }

    #[tokio::test]
    async fn a_reserve_refreshes_its_price_chain_from_its_feed() {
        let chain = MockChainClient::new();
        let owned = |data: Vec<u8>| Account {
            lamports: 1,
            data,
            owner: PROGRAM_ID,
            executable: false,
            rent_epoch: 0,
        };
        let mut prices = vec![0u8; 8 + 32];
        prices[8..40].copy_from_slice(MAINNET_MAPPINGS.as_ref());
        let mut mappings = vec![0u8; 8 + 32 * ENTRIES_MAX];
        let expected = Pubkey::new_unique();
        mappings[8 + 32 * 7..8 + 32 * 8].copy_from_slice(expected.as_ref());
        let mut twaps = vec![0u8; 72];
        twaps[8..40].copy_from_slice(MAINNET_PRICES.as_ref());
        twaps[40..72].copy_from_slice(MAINNET_MAPPINGS.as_ref());
        chain
            .set_account(MAINNET_PRICES, owned(prices))
            .set_account(MAINNET_MAPPINGS, owned(mappings))
            .set_account(MAINNET_TWAPS, owned(twaps));

        let found = feed(&chain, &MAINNET_PRICES).await.unwrap();
        assert_eq!(
            found,
            Feed {
                prices: MAINNET_PRICES,
                mappings: MAINNET_MAPPINGS,
                twaps: MAINNET_TWAPS,
            }
        );
        let mappings = read(&chain, &MAINNET_MAPPINGS, "mappings").await.unwrap();
        assert_eq!(source(&mappings, 7), Ok(expected));
        assert!(source(&mappings, 512).is_err(), "past the feed");
        assert!(
            feed(&chain, &Pubkey::new_unique()).await.is_err(),
            "no prices there"
        );
    }
}
