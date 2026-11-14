//! Jito, as a session's mock block engine serves it.
//!
//! A bundle is up to [`MAX_BUNDLE`] transactions that land in order and
//! atomically, in one slot of a leader running Jito's client, or not at all.
//! It pays its leader a **tip**: a System transfer to one of the eight real
//! tip accounts (`getTipAccounts`). A transaction carrying a key that starts
//! with `jitodontfront` makes any bundle holding it invalid unless it comes
//! first. A bundle's id is Jito's (and Surfpool's): the SHA-256 of its
//! signatures, joined by commas.

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use solana_pubkey::Pubkey;

use super::calibration::Tips;

/// Jito's tip accounts on mainnet (`getTipAccounts`, 2026-11-12), owned by
/// the tip payment program.
pub const TIP_ACCOUNTS: [&str; 8] = [
    "DttWaMuVvTiduZRnguLF7jNxTgiMBZ1hyAumKUiL2KRL",
    "3AVi9Tg9Uo68tJfuvoKvqKNWKkC5wPdSSdeBnizKZ6jT",
    "ADuUkR4vqLUMWXxW9gh6D6L8pMSawimctcNZ5pGwDcEt",
    "ADaUMid9yfUytqMBgopwjb2DTLSokTSzL1zt6iGPaS49",
    "DfXygSm4jCyNCybVYYK6DwvWqjKee8pbDmJGcLWNDXjh",
    "HFqU5x63VTqvQss8hp11i4wVV8bD44PvwucfZ2bU7gRe",
    "Cw8CFyM9FkoMi7K7Crf6HNQqf4uEMzpKw6QNghXLvLkY",
    "96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5",
];
/// Jito's tip payment program, which owns the tip accounts.
pub const TIP_PAYMENT_PROGRAM: Pubkey =
    Pubkey::from_str_const("T1pyyaTNZsKv2WcRAB8oVnk93mLJw2XzjtVYqCsaHqt");
/// A tip account's data on mainnet: its 8-byte discriminator.
pub const TIP_ACCOUNT_DATA: [u8; 8] = [0xc9, 0x21, 0xf4, 0x74, 0xe0, 0x44, 0x61, 0x28];
/// The key a `dont-front` route adds, read-only, to a bundle's first
/// transaction.
pub const DONT_FRONT: Pubkey =
    Pubkey::from_str_const("jitodontfront111111111111111111111111111111");
/// The most transactions in a bundle, and the least tip that lands one.
pub const MAX_BUNDLE: usize = 5;
pub const MIN_TIP: u64 = 1_000;

pub fn is_tip_account(key: &Pubkey) -> bool {
    let key = key.to_string();
    TIP_ACCOUNTS.contains(&key.as_str())
}

pub fn is_dont_front(key: &Pubkey) -> bool {
    key.to_string().starts_with("jitodontfront")
}

/// A bundle's id, from its signatures in order.
pub fn bundle_id(signatures: &[String]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(signatures.join(",").as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// `GET /api/v1/bundles/tip_floor`: the calibration's landed-tip
/// percentiles, in SOL, as Jito serves them.
pub fn tip_floor(tips: &Tips) -> Value {
    let sol = |lamports: u64| lamports as f64 / 1e9;
    json!([{
        "landed_tips_25th_percentile": sol(tips.p25),
        "landed_tips_50th_percentile": sol(tips.p50),
        "landed_tips_75th_percentile": sol(tips.p75),
        "landed_tips_95th_percentile": sol(tips.p95),
        "landed_tips_99th_percentile": sol(tips.p99),
        "ema_landed_tips_50th_percentile": sol(tips.p50),
    }])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tip_accounts_and_dont_front_key_parse() {
        for account in TIP_ACCOUNTS {
            assert!(is_tip_account(&account.parse().unwrap()));
        }
        assert!(!is_tip_account(&Pubkey::new_unique()));
        assert!(is_dont_front(&DONT_FRONT));
        assert!(!is_dont_front(&Pubkey::new_unique()));
        let data =
            base64::Engine::decode(&base64::engine::general_purpose::STANDARD, "ySH0dOBEYSg=")
                .unwrap();
        assert_eq!(data, TIP_ACCOUNT_DATA);
    }

    #[test]
    fn a_bundle_id_is_jitos() {
        let id = bundle_id(&["a".into(), "b".into()]);
        assert_eq!(id.len(), 64);
        assert_eq!(id, bundle_id(&["a".into(), "b".into()]));
        assert_ne!(id, bundle_id(&["b".into(), "a".into()]));
    }
}
