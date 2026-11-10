//! Pyth push-oracle prices, the reference prices the spot venue profiles
//! value base assets at.
//!
//! A feed's price lives in a `PriceUpdateV2` account of the Pyth receiver
//! program, at the push oracle's address for the feed and shard 0. Its
//! layout, after the 8-byte Anchor discriminator: the write authority (32),
//! the verification level (Borsh: `0, num_signatures` for partial, `1` for
//! full), then the price message: feed id (32), price (`i64`), confidence
//! (`u64`), exponent (`i32`), publish time (`i64`), previous publish time
//! (`i64`), EMA price (`i64`), EMA confidence (`u64`), and the slot it was
//! posted at (`u64`).

use mtx_core::{Decimal, MeterError};
use solana_pubkey::{Pubkey, pubkey};

/// The Pyth push oracle program, which owns the feed addresses.
pub const PUSH_ORACLE: Pubkey = pubkey!("pythWSnswVUd12oZpeFP8e9CVaEqJg25g1Vtc2biRsT");

/// The SOL/USD feed id.
pub const SOL_USD: [u8; 32] = [
    0xef, 0x0d, 0x8b, 0x6f, 0xda, 0x2c, 0xeb, 0xa4, 0x1d, 0xa1, 0x5d, 0x40, 0x95, 0xd1, 0xda, 0x39,
    0x2a, 0x0d, 0x2f, 0x8e, 0xd0, 0xc6, 0xc7, 0xbc, 0x0f, 0x4c, 0xfa, 0xc8, 0xc2, 0x80, 0xb5, 0x6d,
];

/// The account holding `feed_id`'s price on shard 0.
pub fn feed_address(feed_id: &[u8; 32]) -> Pubkey {
    Pubkey::find_program_address(&[&0u16.to_le_bytes(), feed_id], &PUSH_ORACLE).0
}

/// A feed's latest price.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Price {
    pub feed_id: [u8; 32],
    /// In the quote currency, USD for every feed the profiles name.
    pub price: Decimal,
    pub publish_time: i64,
    pub posted_slot: u64,
}

/// Decodes a `PriceUpdateV2` account.
pub fn decode(data: &[u8]) -> Result<Price, MeterError> {
    let short = || MeterError::Decode {
        what: "Pyth price update",
        detail: format!("{} bytes is too short", data.len()),
    };
    let mut at = 8 + 32;
    at += match data.get(at) {
        Some(0) => 2,
        Some(1) => 1,
        Some(tag) => {
            return Err(MeterError::Decode {
                what: "Pyth price update",
                detail: format!("verification level {tag}"),
            });
        }
        None => return Err(short()),
    };
    let message = data.get(at..at + 84 + 8).ok_or_else(short)?;
    let feed_id: [u8; 32] = message[..32].try_into().expect("32 bytes");
    let i64_at = |o: usize| i64::from_le_bytes(message[o..o + 8].try_into().expect("8 bytes"));
    let price = i64_at(32);
    let exponent = i32::from_le_bytes(message[48..52].try_into().expect("4 bytes"));
    let publish_time = i64_at(52);
    let posted_slot = u64::from_le_bytes(message[84..92].try_into().expect("8 bytes"));
    if price <= 0 {
        return Err(MeterError::Decode {
            what: "Pyth price update",
            detail: format!("price {price} is not positive"),
        });
    }
    let price = match u32::try_from(-i64::from(exponent)) {
        Ok(scale) => Decimal::try_from_i128_with_scale(i128::from(price), scale).ok(),
        Err(_) => u32::try_from(exponent)
            .ok()
            .and_then(|e| 10i128.checked_pow(e))
            .and_then(|f| f.checked_mul(i128::from(price)))
            .and_then(|p| Decimal::try_from_i128_with_scale(p, 0).ok()),
    }
    .ok_or_else(|| MeterError::Decode {
        what: "Pyth price update",
        detail: format!("price {price} at exponent {exponent} doesn't fit a decimal"),
    })?;
    Ok(Price {
        feed_id,
        price,
        publish_time,
        posted_slot,
    })
}

#[cfg(test)]
mod tests {
    use solana_pubkey::pubkey;

    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.trim().len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn decodes_mainnets_sol_usd_update() {
        // Read from mainnet at slot 467582217.
        let data = hex(include_str!("testdata/pyth_sol_usd.hex"));
        let price = decode(&data).unwrap();
        assert_eq!(price.feed_id, SOL_USD);
        assert_eq!(price.price, Decimal::new(8_583_462_200, 8));
        assert_eq!(price.posted_slot, 467582158);
        assert_eq!(
            feed_address(&SOL_USD),
            pubkey!("7UVimffxr9ow1uXYxsr4LHAcV58mLzhmwaeKvJ1pjLiE")
        );
        assert!(decode(&data[..100]).is_err());
    }
}
