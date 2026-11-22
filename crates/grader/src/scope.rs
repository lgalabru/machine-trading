//! Kamino Scope as a feed bots can push (ADR 0004, "An oracle bots can
//! push"): a scenario rewrites the Scope entries it moves to `FixedPrice`
//! and writes the upstream price into the mapping's `generic` data, and the
//! entry's price changes only when someone sends `refresh_price_list`.
//!
//! From Scope `a09283d`, `programs/scope/src`:
//!
//! - `oracles/fixed_price.rs`: a `FixedPrice` entry's price is the Borsh
//!   `Price { value: u64, exp: u64 }` at the start of its 20 `generic`
//!   bytes, and it takes no price account.
//! - `states/oracle_mappings.rs`, `set_entry_mapping`: an entry with no
//!   price account stores Scope's own program id, not the zero key.
//!   `handler_refresh_prices.rs` skips an entry whose account is the zero
//!   key ("no mapping is set"), so a `FixedPrice` entry must name Scope.
//! - `handler_refresh_prices.rs`: `refresh_price_list` takes, after its
//!   three accounts and the instructions sysvar, each entry's price account
//!   in order, and must be a top-level instruction preceded only by
//!   compute-budget ones. It stamps a `FixedPrice` with the current slot
//!   and time.

use serde_json::{Value, json};
use solana_pubkey::Pubkey;

/// Scope's program, its feeds' accounts, and the one `refresh_price_list`
/// builder: the runtime's, which the strategies' `push-price` sends too.
pub use crate::venue::kamino::scope::{Feed, PROGRAM_ID, refresh_price_list};
use crate::{chain::Override, oracles::SCOPE_ENTRIES};

/// `OracleType::FixedPrice` (`states/oracle_type.rs`).
pub const ORACLE_TYPE_FIXED_PRICE: u8 = 23;
/// The surfpool template that rewrites an `OracleMappings` entry.
pub const MAPPING_TEMPLATE: &str = "kamino-scope-price-source";
/// An entry's `generic` data, in bytes.
pub const GENERIC_LEN: usize = 20;

/// Kamino's main feed (configuration `6cMwdbrJ95D7v5655Zsoe7oXmjQJMnagWK8EcdG6qmGM`):
/// the one the main lending market's reserves read. SOL is entry 3, USDC 13.
pub const MAIN_FEED: Feed = Feed {
    prices: solana_pubkey::pubkey!("3t4JZcueEzTbVP6kLxXrL3VpWx45jDer4eqysweBchNH"),
    mappings: solana_pubkey::pubkey!("4zh6bmb77qX2CL7t5AJYCqa6YqFafbz3QJNeFvZjLowg"),
    twaps: solana_pubkey::pubkey!("6L6vUts9tYqxHVUCEFVc2mzZw6yxMn8C6a44cp5ga7e9"),
};

/// [`refresh_price_list`] for `FixedPrice` entries, which take Scope's own
/// id as their price account. It must be its transaction's first
/// instruction after the compute budget.
pub fn refresh_fixed_prices(feed: &Feed, entries: &[u16]) -> solana_transaction::Instruction {
    refresh_price_list(feed, entries, &vec![PROGRAM_ID; entries.len()])
}

/// Scope's `Price`: `value` times ten to the minus `exp`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Price {
    pub value: u64,
    pub exp: u64,
}

/// The largest exponent a price takes here: Scope compares exponents up to
/// 19 apart (`dated_price.rs`, `MAX_SAFE_EXP_DIFF`), and klend reads 18
/// decimals at most.
pub const PRICE_EXP_MAX: u64 = 18;

impl Price {
    /// `price` in units of ten to the minus `exp`, rounded to the nearest;
    /// `None` when it isn't positive or doesn't fit.
    pub fn from_f64(price: f64, exp: u64) -> Option<Self> {
        if exp > PRICE_EXP_MAX || !(price.is_finite() && price > 0.0) {
            return None;
        }
        let scaled = (price * 10f64.powi(i32::try_from(exp).ok()?)).round();
        if !(1.0..=u64::MAX as f64).contains(&scaled) {
            return None;
        }
        Some(Self {
            value: scaled as u64,
            exp,
        })
    }

    pub fn to_f64(self) -> f64 {
        assert!(self.exp <= PRICE_EXP_MAX);
        self.value as f64 / 10f64.powi(self.exp as i32)
    }

    /// The `generic` data of a `FixedPrice` entry at this price: the Borsh
    /// `Price`, then zeros.
    pub fn generic(self) -> [u8; GENERIC_LEN] {
        let mut generic = [0u8; GENERIC_LEN];
        generic[..8].copy_from_slice(&self.value.to_le_bytes());
        generic[8..16].copy_from_slice(&self.exp.to_le_bytes());
        generic
    }

    /// The price a `FixedPrice` entry's `generic` data holds.
    pub fn from_generic(generic: &[u8; GENERIC_LEN]) -> Self {
        Self {
            value: u64::from_le_bytes(generic[..8].try_into().expect("8 bytes")),
            exp: u64::from_le_bytes(generic[8..16].try_into().expect("8 bytes")),
        }
    }
}

/// The override that turns `entry` of `feed` into a `FixedPrice` at
/// `price`, at a scenario-relative `slot`: the type, the price account
/// Scope stores for "none", and the price in `generic`. The entry's price
/// in `OraclePrices` is untouched until a refresh.
pub fn fixed_price_mapping(feed: &Feed, entry: u16, price: Price, slot: u64) -> Override {
    assert!(entry < SCOPE_ENTRIES);
    let mut values = generic_values(entry, price);
    values.insert(
        format!("price_types.{entry}"),
        json!(ORACLE_TYPE_FIXED_PRICE),
    );
    values.insert(
        format!("price_info_accounts.{entry}"),
        json!(PROGRAM_ID.to_string()),
    );
    Override {
        template_id: MAPPING_TEMPLATE,
        account: feed.mappings,
        values: Value::Object(values),
        slot,
        label: format!("scope {entry} fixed price @{slot}"),
    }
}

/// The override that moves a `FixedPrice` entry's upstream price, at a
/// scenario-relative `slot`: only `generic`.
pub fn fixed_price_update(feed: &Feed, entry: u16, price: Price, slot: u64) -> Override {
    assert!(entry < SCOPE_ENTRIES);
    Override {
        template_id: MAPPING_TEMPLATE,
        account: feed.mappings,
        values: Value::Object(generic_values(entry, price)),
        slot,
        label: format!("scope {entry} upstream @{slot}"),
    }
}

/// `generic.<entry>.<byte>`, each of the 20 bytes: the template writes
/// fields, and a byte array's elements are its fields.
pub(crate) fn generic_values(entry: u16, price: Price) -> serde_json::Map<String, Value> {
    price
        .generic()
        .iter()
        .enumerate()
        .map(|(i, byte)| (format!("generic.{entry}.{i}"), json!(byte)))
        .collect()
}

/// What an `OracleMappings` account says of one entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MappingEntry {
    pub price_account: Pubkey,
    /// The `OracleType`, with Scope's frozen flag cleared.
    pub oracle_type: u8,
    pub ref_price: Option<u16>,
    pub generic: [u8; GENERIC_LEN],
}

/// `OracleMappings`: a discriminator, then 512 of each field in turn.
const MAPPINGS_ACCOUNTS_AT: usize = 8;
const MAPPINGS_TYPES_AT: usize = MAPPINGS_ACCOUNTS_AT + 32 * SCOPE_ENTRIES as usize;
const MAPPINGS_TOLERANCES_AT: usize = MAPPINGS_TYPES_AT + SCOPE_ENTRIES as usize;
const MAPPINGS_TWAP_MASKS_AT: usize = MAPPINGS_TOLERANCES_AT + 2 * SCOPE_ENTRIES as usize;
const MAPPINGS_REF_PRICES_AT: usize = MAPPINGS_TWAP_MASKS_AT + SCOPE_ENTRIES as usize;
const MAPPINGS_GENERIC_AT: usize = MAPPINGS_REF_PRICES_AT + 2 * SCOPE_ENTRIES as usize;
const MAPPINGS_LEN_MIN: usize = MAPPINGS_GENERIC_AT + GENERIC_LEN * SCOPE_ENTRIES as usize;
/// `FROZEN_FLAG` in `states/oracle_mappings.rs`: the type byte's top bit.
const FROZEN_FLAG: u8 = 0x80;

/// One entry of an `OracleMappings` account's data, or `None` when it is
/// too short or `entry` past the last.
pub fn mapping_entry(data: &[u8], entry: u16) -> Option<MappingEntry> {
    if data.len() < MAPPINGS_LEN_MIN || entry >= SCOPE_ENTRIES {
        return None;
    }
    let i = usize::from(entry);
    let account_at = MAPPINGS_ACCOUNTS_AT + 32 * i;
    let ref_at = MAPPINGS_REF_PRICES_AT + 2 * i;
    let generic_at = MAPPINGS_GENERIC_AT + GENERIC_LEN * i;
    let ref_price = u16::from_le_bytes([data[ref_at], data[ref_at + 1]]);
    Some(MappingEntry {
        price_account: Pubkey::new_from_array(data[account_at..account_at + 32].try_into().ok()?),
        oracle_type: data[MAPPINGS_TYPES_AT + i] & !FROZEN_FLAG,
        ref_price: (ref_price != u16::MAX).then_some(ref_price),
        generic: data[generic_at..generic_at + GENERIC_LEN].try_into().ok()?,
    })
}

/// Turn `entry` of an `OracleMappings` account's `data` into a
/// `FixedPrice` at `price`, in place: the type, Scope's own id as its price
/// account, `price` in `generic`, and no reference price, since the
/// scenario's upstream is the reference. What a lending session writes
/// before its cheatcodes lock, for each entry it pushes (`mode: pushed`).
pub fn fix_entry(data: &mut [u8], entry: u16, price: Price) -> Result<(), String> {
    if data.len() < MAPPINGS_LEN_MIN || entry >= SCOPE_ENTRIES {
        return Err(format!("no mapping entry {entry} in {} bytes", data.len()));
    }
    let i = usize::from(entry);
    data[MAPPINGS_ACCOUNTS_AT + 32 * i..][..32].copy_from_slice(PROGRAM_ID.as_ref());
    data[MAPPINGS_TYPES_AT + i] = ORACLE_TYPE_FIXED_PRICE;
    data[MAPPINGS_REF_PRICES_AT + 2 * i..][..2].copy_from_slice(&u16::MAX.to_le_bytes());
    data[MAPPINGS_GENERIC_AT + GENERIC_LEN * i..][..GENERIC_LEN].copy_from_slice(&price.generic());
    let written = mapping_entry(data, entry).expect("the entry is in range");
    assert_eq!(Price::from_generic(&written.generic), price);
    assert_eq!(written.oracle_type, ORACLE_TYPE_FIXED_PRICE);
    Ok(())
}

/// An `OraclePrices` entry's price, slot and Unix time.
pub fn dated_price(data: &[u8], entry: u16) -> Option<(Price, u64, u64)> {
    /// A discriminator and the mappings' key, then 56-byte entries.
    const ENTRIES_AT: usize = 8 + 32;
    const ENTRY_LEN: usize = 56;
    if entry >= SCOPE_ENTRIES {
        return None;
    }
    let at = ENTRIES_AT + usize::from(entry) * ENTRY_LEN;
    let read = |offset: usize| -> Option<u64> {
        Some(u64::from_le_bytes(
            data.get(at + offset..at + offset + 8)?.try_into().ok()?,
        ))
    };
    Some((
        Price {
            value: read(0)?,
            exp: read(8)?,
        },
        read(16)?,
        read(24)?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_price_round_trips_through_generic_data() {
        let price = Price::from_f64(142.37, 8).unwrap();
        assert_eq!(
            price,
            Price {
                value: 14_237_000_000,
                exp: 8
            }
        );
        let generic = price.generic();
        assert_eq!(generic[16..], [0; 4]);
        assert_eq!(Price::from_generic(&generic), price);
        assert!((price.to_f64() - 142.37).abs() < 1e-9);
        assert_eq!(Price::from_f64(0.0, 8), None);
        assert_eq!(Price::from_f64(-1.0, 8), None);
        assert_eq!(Price::from_f64(f64::NAN, 8), None);
        assert_eq!(Price::from_f64(1.0, PRICE_EXP_MAX + 1), None);
        assert_eq!(Price::from_f64(1e30, 18), None);
    }

    #[test]
    fn the_mapping_override_names_scope_and_writes_every_generic_byte() {
        let price = Price {
            value: 0x0102_0304_0506_0708,
            exp: 8,
        };
        let o = fixed_price_mapping(&MAIN_FEED, 3, price, 2);
        assert_eq!(o.template_id, "kamino-scope-price-source");
        assert_eq!(o.account, MAIN_FEED.mappings);
        assert_eq!(o.values["price_types.3"], 23);
        assert_eq!(o.values["price_info_accounts.3"], PROGRAM_ID.to_string());
        assert_eq!(o.values["generic.3.0"], 8);
        assert_eq!(o.values["generic.3.7"], 1);
        assert_eq!(o.values["generic.3.8"], 8);
        assert_eq!(o.values["generic.3.19"], 0);
        assert_eq!(o.values.as_object().unwrap().len(), GENERIC_LEN + 2);
        let update = fixed_price_update(&MAIN_FEED, 3, price, 5);
        assert_eq!(update.values.as_object().unwrap().len(), GENERIC_LEN);
        assert!(update.values.get("price_types.3").is_none());
    }

    #[test]
    fn a_mapping_entry_and_a_dated_price_decode() {
        let mut mappings = vec![0u8; MAPPINGS_LEN_MIN];
        let i = 3usize;
        mappings[MAPPINGS_ACCOUNTS_AT + 32 * i..][..32].copy_from_slice(PROGRAM_ID.as_ref());
        mappings[MAPPINGS_TYPES_AT + i] = ORACLE_TYPE_FIXED_PRICE | FROZEN_FLAG;
        mappings[MAPPINGS_REF_PRICES_AT + 2 * i..][..2].copy_from_slice(&u16::MAX.to_le_bytes());
        let price = Price { value: 9, exp: 1 };
        mappings[MAPPINGS_GENERIC_AT + GENERIC_LEN * i..][..GENERIC_LEN]
            .copy_from_slice(&price.generic());
        let entry = mapping_entry(&mappings, 3).unwrap();
        assert_eq!(entry.price_account, PROGRAM_ID);
        assert_eq!(entry.oracle_type, ORACLE_TYPE_FIXED_PRICE);
        assert_eq!(entry.ref_price, None);
        assert_eq!(Price::from_generic(&entry.generic), price);
        assert_eq!(mapping_entry(&mappings[1..], 3), None);

        let pushed = Price { value: 70, exp: 0 };
        let mut fixed = vec![0u8; MAPPINGS_LEN_MIN];
        fixed[MAPPINGS_REF_PRICES_AT + 2 * 455..][..2].copy_from_slice(&7u16.to_le_bytes());
        fix_entry(&mut fixed, 455, pushed).unwrap();
        let entry = mapping_entry(&fixed, 455).unwrap();
        assert_eq!(entry.price_account, PROGRAM_ID);
        assert_eq!(entry.ref_price, None, "the reference price is cleared");
        assert_eq!(Price::from_generic(&entry.generic), pushed);
        assert_eq!(
            mapping_entry(&fixed, 454).unwrap().oracle_type,
            0,
            "its neighbour"
        );
        assert!(fix_entry(&mut fixed, SCOPE_ENTRIES, pushed).is_err());
        assert!(fix_entry(&mut fixed[1..], 3, pushed).is_err());
        assert_eq!(mapping_entry(&mappings, SCOPE_ENTRIES), None);

        let mut prices = vec![0u8; 40 + 56 * 4];
        let at = 40 + 56 * 3;
        for (k, v) in [7u64, 8, 100, 1_700_000_000].iter().enumerate() {
            prices[at + 8 * k..][..8].copy_from_slice(&v.to_le_bytes());
        }
        assert_eq!(
            dated_price(&prices, 3),
            Some((Price { value: 7, exp: 8 }, 100, 1_700_000_000))
        );
        assert_eq!(dated_price(&prices, 4), None);
    }
}
