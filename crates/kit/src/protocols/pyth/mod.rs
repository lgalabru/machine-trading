//! Pyth push-oracle price feeds (feature `pyth`).
//!
//! A Pyth price feed is named by its feed id, such as SOL/USD's
//! [`SOL_USD_FEED_ID`]. On Solana, the push oracle keeps each feed's latest
//! price in a `PriceUpdateV2` account at an address derived from the feed id
//! and a shard ([`feed_account`]), written by the feed's poster.
//! [`PriceUpdate::decode`] reads one from its account bytes, and
//! [`PriceUpdate::decode_feed`] also checks it is the feed asked for.
//!
//! | Pyth | Core |
//! |------|------|
//! | `price × 10^exponent` | [`PriceUpdate::price`], an exact [`Decimal`] |
//! | `conf × 10^exponent` | [`PriceUpdate::confidence`] |
//! | `ema_price`, `ema_conf` | [`PriceUpdate::ema_price`], [`PriceUpdate::ema_confidence`] |
//!
//! The account carries no market; which feed prices which instrument is the
//! caller's mapping, as each venue profile in the repository's `specs/`
//! names it.

use rust_decimal::Decimal;
use solana_pubkey::Pubkey;

use crate::chain::ChainError;

/// The Pyth push oracle, which owns every feed's price account.
pub const PUSH_ORACLE_ID: Pubkey =
    solana_pubkey::pubkey!("pythWSnswVUd12oZpeFP8e9CVaEqJg25g1Vtc2biRsT");

/// SOL/USD's feed id, the reference price the Phoenix and OpenBook spot
/// profiles name for SOL.
pub const SOL_USD_FEED_ID: [u8; 32] = [
    0xef, 0x0d, 0x8b, 0x6f, 0xda, 0x2c, 0xeb, 0xa4, 0x1d, 0xa1, 0x5d, 0x40, 0x95, 0xd1, 0xda, 0x39,
    0x2a, 0x0d, 0x2f, 0x8e, 0xd0, 0xc6, 0xc7, 0xbc, 0x0f, 0x4c, 0xfa, 0xc8, 0xc2, 0x80, 0xb5, 0x6d,
];

/// The push oracle's price account for `feed_id` in `shard`. Shard 0 is the
/// one Pyth keeps current for every feed it sponsors.
pub fn feed_account(shard: u16, feed_id: &[u8; 32]) -> Pubkey {
    Pubkey::find_program_address(&[&shard.to_le_bytes(), feed_id], &PUSH_ORACLE_ID).0
}

/// The Anchor discriminator of a `PriceUpdateV2` account.
/// SPCX's feed id: `Equity.US.SPCX/USD`, SpaceX's share price, the
/// reference price the Raydium, Meteora and Orca profiles name for SPCX. It
/// publishes in the US regular session only.
pub const SPCX_USD_FEED_ID: [u8; 32] = [
    0x8a, 0x59, 0x3d, 0x6e, 0xdd, 0xe7, 0xa3, 0x09, 0x52, 0x13, 0xc8, 0x81, 0x16, 0xd8, 0x84, 0x0d,
    0x01, 0xe9, 0x3c, 0x2d, 0xde, 0xb8, 0x00, 0xbc, 0x89, 0x17, 0x72, 0xeb, 0x8b, 0x93, 0xbb, 0x94,
];

/// The feed the spot venue profiles value `mint` at, for the mints they
/// name: SOL/USD for wrapped SOL, SPCX/USD for SPCX. USDC is valued at $1
/// and has none.
pub fn feed_for_mint(mint: &Pubkey) -> Option<[u8; 32]> {
    if mint == &solana_pubkey::pubkey!("So11111111111111111111111111111111111111112") {
        Some(SOL_USD_FEED_ID)
    } else if mint == &crate::protocols::SPCX_MINT {
        Some(SPCX_USD_FEED_ID)
    } else {
        None
    }
}

/// `feed_id`'s latest price, read from its shard-0 account.
pub async fn read_price(
    chain: &dyn crate::chain::ChainClient,
    feed_id: &[u8; 32],
) -> Result<Decimal, ChainError> {
    let address = feed_account(0, feed_id);
    let account = chain
        .get_account(&address)
        .await?
        .ok_or(ChainError::AccountNotFound(address))?;
    PriceUpdate::decode_feed(&account.data, feed_id)?
        .price()
        .ok_or_else(|| decode_error("the price doesn't fit a decimal"))
}

pub const PRICE_UPDATE_V2_DISCRIMINATOR: [u8; 8] = [34, 241, 35, 99, 157, 126, 244, 205];

/// How much of the Wormhole guardian set signed the update.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerificationLevel {
    /// Fewer signatures than a quorum: `num_signatures` of them.
    Partial {
        /// Guardian signatures checked.
        num_signatures: u8,
    },
    /// A full quorum.
    Full,
}

/// One feed's latest price, as a `PriceUpdateV2` account holds it.
///
/// Raw fields are in units of `10^exponent`; the methods convert them
/// exactly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PriceUpdate {
    /// Who may write the account.
    pub write_authority: [u8; 32],
    /// How the update was verified.
    pub verification_level: VerificationLevel,
    /// The feed's identifier.
    pub feed_id: [u8; 32],
    /// The price, in units of `10^exponent`.
    pub price_raw: i64,
    /// The confidence interval, in units of `10^exponent`.
    pub conf_raw: u64,
    /// The power of ten the raw fields are counted in.
    pub exponent: i32,
    /// When the price was published, in Unix seconds.
    pub publish_time: i64,
    /// When the previous price was published, in Unix seconds.
    pub prev_publish_time: i64,
    /// The exponentially weighted moving average price, in units of
    /// `10^exponent`.
    pub ema_price_raw: i64,
    /// The moving average's confidence, in units of `10^exponent`.
    pub ema_conf_raw: u64,
    /// The slot the update was posted at.
    pub posted_slot: u64,
}

fn decode_error(detail: impl Into<String>) -> ChainError {
    ChainError::Decode {
        what: "pyth price update",
        detail: detail.into(),
    }
}

fn read<const N: usize>(data: &[u8], at: usize) -> Result<[u8; N], ChainError> {
    data.get(at..at + N)
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| decode_error(format!("account ends before byte {}", at + N)))
}

impl PriceUpdate {
    /// Decodes a `PriceUpdateV2` account's data.
    pub fn decode(data: &[u8]) -> Result<Self, ChainError> {
        if read::<8>(data, 0)? != PRICE_UPDATE_V2_DISCRIMINATOR {
            return Err(decode_error("not a PriceUpdateV2 account"));
        }
        let write_authority = read::<32>(data, 8)?;
        let (verification_level, o) = match read::<1>(data, 40)?[0] {
            0 => (
                VerificationLevel::Partial {
                    num_signatures: read::<1>(data, 41)?[0],
                },
                42,
            ),
            1 => (VerificationLevel::Full, 41),
            tag => return Err(decode_error(format!("unknown verification level {tag}"))),
        };
        Ok(Self {
            write_authority,
            verification_level,
            feed_id: read::<32>(data, o)?,
            price_raw: i64::from_le_bytes(read(data, o + 32)?),
            conf_raw: u64::from_le_bytes(read(data, o + 40)?),
            exponent: i32::from_le_bytes(read(data, o + 48)?),
            publish_time: i64::from_le_bytes(read(data, o + 52)?),
            prev_publish_time: i64::from_le_bytes(read(data, o + 60)?),
            ema_price_raw: i64::from_le_bytes(read(data, o + 68)?),
            ema_conf_raw: u64::from_le_bytes(read(data, o + 76)?),
            posted_slot: u64::from_le_bytes(read(data, o + 84)?),
        })
    }

    /// Decodes a `PriceUpdateV2` account's data, and refuses it unless it
    /// holds `feed_id`'s price: an address alone doesn't say which feed an
    /// account carries.
    pub fn decode_feed(data: &[u8], feed_id: &[u8; 32]) -> Result<Self, ChainError> {
        let update = Self::decode(data)?;
        if &update.feed_id != feed_id {
            return Err(decode_error("the account holds another feed's price"));
        }
        Ok(update)
    }

    /// `raw × 10^exponent`, exactly. `None` if it does not fit a
    /// [`Decimal`] (an exponent beyond ±28).
    fn scaled(&self, raw: i128) -> Option<Decimal> {
        if self.exponent <= 0 {
            Decimal::try_from_i128_with_scale(raw, self.exponent.unsigned_abs()).ok()
        } else {
            10_i128
                .checked_pow(self.exponent.unsigned_abs())
                .and_then(|p| raw.checked_mul(p))
                .and_then(|v| Decimal::try_from_i128_with_scale(v, 0).ok())
        }
    }

    /// The price, exactly.
    pub fn price(&self) -> Option<Decimal> {
        self.scaled(i128::from(self.price_raw))
    }

    /// The confidence interval, exactly.
    pub fn confidence(&self) -> Option<Decimal> {
        self.scaled(i128::from(self.conf_raw))
    }

    /// The moving average price, exactly.
    pub fn ema_price(&self) -> Option<Decimal> {
        self.scaled(i128::from(self.ema_price_raw))
    }

    /// The moving average's confidence, exactly.
    pub fn ema_confidence(&self) -> Option<Decimal> {
        self.scaled(i128::from(self.ema_conf_raw))
    }

    /// The price as `price_raw × 10.0^exponent` in floating point, the
    /// conversion Pyth's own clients make.
    pub fn price_f64(&self) -> f64 {
        self.price_raw as f64 * 10f64.powf(f64::from(self.exponent))
    }

    /// The confidence as `conf_raw × 10.0^exponent` in floating point.
    pub fn confidence_f64(&self) -> f64 {
        self.conf_raw as f64 * 10f64.powf(f64::from(self.exponent))
    }

    /// Seconds between the price's publication and `unix_time`.
    pub fn age(&self, unix_time: i64) -> i64 {
        unix_time - self.publish_time
    }
}

#[cfg(test)]
mod tests {
    use rust_decimal_macros::dec;

    use super::*;

    /// A `PriceUpdateV2` account as the receiver program writes it.
    pub(crate) fn encode(
        level: VerificationLevel,
        price: i64,
        conf: u64,
        exponent: i32,
    ) -> Vec<u8> {
        let mut d = PRICE_UPDATE_V2_DISCRIMINATOR.to_vec();
        d.extend_from_slice(&[7; 32]);
        match level {
            VerificationLevel::Partial { num_signatures } => {
                d.extend_from_slice(&[0, num_signatures])
            }
            VerificationLevel::Full => d.push(1),
        }
        d.extend_from_slice(&[9; 32]);
        d.extend_from_slice(&price.to_le_bytes());
        d.extend_from_slice(&conf.to_le_bytes());
        d.extend_from_slice(&exponent.to_le_bytes());
        d.extend_from_slice(&1_700_000_000_i64.to_le_bytes());
        d.extend_from_slice(&1_699_999_999_i64.to_le_bytes());
        d.extend_from_slice(&(price - 5).to_le_bytes());
        d.extend_from_slice(&(conf + 1).to_le_bytes());
        d.extend_from_slice(&123_456_u64.to_le_bytes());
        d
    }

    #[test]
    fn decodes_both_verification_levels() {
        for level in [
            VerificationLevel::Full,
            VerificationLevel::Partial { num_signatures: 5 },
        ] {
            let u = PriceUpdate::decode(&encode(level, 15_025_000_000, 2_500_000, -8)).unwrap();
            assert_eq!(u.verification_level, level);
            assert_eq!(u.feed_id, [9; 32]);
            assert_eq!(u.price(), Some(dec!(150.25)));
            assert_eq!(u.confidence(), Some(dec!(0.025)));
            assert_eq!(u.ema_price(), Some(dec!(150.24999995)));
            assert_eq!(u.publish_time, 1_700_000_000);
            assert_eq!(u.posted_slot, 123_456);
            assert_eq!(u.age(1_700_000_012), 12);
            assert!((u.price_f64() - 150.25).abs() < 1e-9);
        }
    }

    #[test]
    fn sol_usd_is_at_its_push_oracle_address() {
        assert_eq!(
            feed_account(0, &SOL_USD_FEED_ID),
            solana_pubkey::pubkey!("7UVimffxr9ow1uXYxsr4LHAcV58mLzhmwaeKvJ1pjLiE")
        );
        assert_eq!(
            feed_account(0, &SPCX_USD_FEED_ID),
            solana_pubkey::pubkey!("6WRG9ZLzhqykJ6fdAYZeTHMg79JnnYjjZTs5QDUb9iwv")
        );
        assert_eq!(
            feed_for_mint(&crate::protocols::SPCX_MINT),
            Some(SPCX_USD_FEED_ID)
        );
    }

    #[test]
    fn decode_feed_checks_the_feed() {
        let data = encode(VerificationLevel::Full, 1, 1, -8);
        assert!(PriceUpdate::decode_feed(&data, &[9; 32]).is_ok());
        assert!(PriceUpdate::decode_feed(&data, &SOL_USD_FEED_ID).is_err());
    }

    #[test]
    fn positive_exponents_scale_up() {
        let u = PriceUpdate::decode(&encode(VerificationLevel::Full, 3, 1, 2)).unwrap();
        assert_eq!(u.price(), Some(dec!(300)));
    }

    #[test]
    fn rejects_other_accounts() {
        let mut data = encode(VerificationLevel::Full, 1, 1, -8);
        data[0] ^= 1;
        assert!(PriceUpdate::decode(&data).is_err());
        let data = encode(VerificationLevel::Full, 1, 1, -8);
        assert!(PriceUpdate::decode(&data[..60]).is_err());
        let mut data = encode(VerificationLevel::Full, 1, 1, -8);
        data[40] = 7;
        assert!(PriceUpdate::decode(&data).is_err());
    }

    /// Mainnet's SOL/USD update, read at slot 467582217.
    #[test]
    fn decodes_mainnets_sol_usd_update() {
        let hex = include_str!("testdata/pyth_sol_usd.hex").trim();
        let data: Vec<u8> = (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect();
        let update = PriceUpdate::decode_feed(&data, &SOL_USD_FEED_ID).unwrap();
        assert_eq!(update.verification_level, VerificationLevel::Full);
        assert_eq!(update.price(), Some(dec!(85.83462200)));
        assert_eq!(update.posted_slot, 467582158);
        assert_eq!(
            feed_account(0, &SOL_USD_FEED_ID).to_string(),
            "7UVimffxr9ow1uXYxsr4LHAcV58mLzhmwaeKvJ1pjLiE"
        );
        assert!(PriceUpdate::decode_feed(&data, &[0; 32]).is_err());
    }
}
