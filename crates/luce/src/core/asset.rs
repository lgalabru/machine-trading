//! Assets: the tokens that balances, deposits and yields are denominated in.
//!
//! An [`Asset`] is a *thing* — SOL, USDC, JitoSOL — independent of where it
//! trades or is lent. It sits below [`Instrument`](crate::core::instrument::Instrument):
//! an instrument is a listing of an asset (or a contract) on a specific venue,
//! whereas an asset is only a unit of account.
//!
//! Amounts come in two forms:
//!
//! - **native** units (`u64`): the integer the chain works with (lamports,
//!   micro-USDC).
//! - **UI** amounts ([`Decimal`]): human-readable, scaled by `10^decimals`.
//!
//! Conversions between them are exact or they fail. Nothing is silently
//! rounded: a UI amount with more fractional digits than the asset supports is
//! an error, not a truncation. This matters because a lending deposit of
//! `1.0000000001 SOL` cannot exist, and the caller must decide how to round.

use rust_decimal::{Decimal, prelude::ToPrimitive};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Largest number of decimals an [`Asset`] may declare.
///
/// `10^18` still fits comfortably in a [`Decimal`] (which holds 28–29
/// significant digits) alongside realistic token supplies. No widely used
/// token exceeds 18 decimals.
pub const MAX_DECIMALS: u8 = 18;

/// Globally unique identifier for an asset.
///
/// On Solana this is the token mint address, base58-encoded. The convention
/// matters only for persistence and display; this crate never interprets it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AssetId(pub String);

impl std::fmt::Display for AssetId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A token and the precision it is denominated in.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Asset {
    /// Unique identifier (token mint on Solana).
    pub id: AssetId,
    /// Human-readable ticker, e.g. `"SOL"`, `"USDC"`.
    pub symbol: String,
    /// Number of fractional digits in one whole unit. `9` for SOL, `6` for USDC.
    pub decimals: u8,
}

/// Errors from amount conversion.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum AssetError {
    /// The asset declares more decimals than this crate supports.
    #[error("asset declares {0} decimals; at most {MAX_DECIMALS} are supported")]
    DecimalsTooLarge(u8),

    /// Native amounts are unsigned; a negative UI amount has no representation.
    #[error("amount {0} is negative")]
    Negative(Decimal),

    /// The UI amount has fractional digits the asset cannot represent.
    #[error("amount {amount} has more than {decimals} fractional digits")]
    TooPrecise { amount: Decimal, decimals: u8 },

    /// The UI amount exceeds `u64::MAX` native units.
    #[error("amount {0} does not fit in a 64-bit native amount")]
    Overflow(Decimal),
}

impl Asset {
    /// Creates an asset, validating the decimal count.
    pub fn new(
        id: impl Into<AssetId>,
        symbol: impl Into<String>,
        decimals: u8,
    ) -> Result<Self, AssetError> {
        if decimals > MAX_DECIMALS {
            return Err(AssetError::DecimalsTooLarge(decimals));
        }
        Ok(Self {
            id: id.into(),
            symbol: symbol.into(),
            decimals,
        })
    }

    /// `10^decimals`, the number of native units in one whole token.
    pub fn scale(&self) -> Decimal {
        Decimal::from_i128_with_scale(10_i128.pow(u32::from(self.decimals)), 0)
    }

    /// Converts a UI amount to native units, exactly.
    ///
    /// Fails if the amount is negative, carries more fractional digits than
    /// the asset has decimals, or exceeds `u64::MAX` native units.
    pub fn to_native(&self, ui: Decimal) -> Result<u64, AssetError> {
        if ui.is_sign_negative() && !ui.is_zero() {
            return Err(AssetError::Negative(ui));
        }
        let scaled = ui
            .checked_mul(self.scale())
            .ok_or(AssetError::Overflow(ui))?;
        if !scaled.fract().is_zero() {
            return Err(AssetError::TooPrecise {
                amount: ui,
                decimals: self.decimals,
            });
        }
        // `scaled` is a non-negative integer here; it only remains to check
        // that it fits in a u64.
        scaled.to_u64().ok_or(AssetError::Overflow(ui))
    }

    /// Converts native units to a UI amount. Always exact.
    ///
    /// The result carries exactly `decimals` fractional digits, so
    /// `1 SOL` renders as `1.000000000` unless normalized by the caller.
    pub fn to_ui(&self, native: u64) -> Decimal {
        Decimal::from_i128_with_scale(i128::from(native), u32::from(self.decimals))
    }
}

impl From<String> for AssetId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for AssetId {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use rust_decimal_macros::dec;

    use super::*;

    fn sol() -> Asset {
        Asset::new("So11111111111111111111111111111111111111112", "SOL", 9).unwrap()
    }

    fn usdc() -> Asset {
        Asset::new("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v", "USDC", 6).unwrap()
    }

    #[test]
    fn rejects_too_many_decimals() {
        assert_eq!(
            Asset::new("x", "X", 19).unwrap_err(),
            AssetError::DecimalsTooLarge(19)
        );
        assert!(Asset::new("x", "X", 18).is_ok());
        assert!(Asset::new("x", "X", 0).is_ok());
    }

    #[test]
    fn scale_is_power_of_ten() {
        assert_eq!(sol().scale(), dec!(1_000_000_000));
        assert_eq!(usdc().scale(), dec!(1_000_000));
        assert_eq!(Asset::new("x", "X", 0).unwrap().scale(), dec!(1));
    }

    #[test]
    fn to_native_exact() {
        assert_eq!(sol().to_native(dec!(1)).unwrap(), 1_000_000_000);
        assert_eq!(sol().to_native(dec!(1.5)).unwrap(), 1_500_000_000);
        assert_eq!(sol().to_native(dec!(0.000000001)).unwrap(), 1);
        assert_eq!(usdc().to_native(dec!(12.345678)).unwrap(), 12_345_678);
        assert_eq!(sol().to_native(dec!(0)).unwrap(), 0);
        // Trailing zeros beyond the asset's precision are fine: value is exact.
        assert_eq!(
            sol().to_native(dec!(1.0000000000000)).unwrap(),
            1_000_000_000
        );
    }

    #[test]
    fn to_native_rejects_sub_unit_precision() {
        let err = sol().to_native(dec!(0.0000000001)).unwrap_err();
        assert_eq!(
            err,
            AssetError::TooPrecise {
                amount: dec!(0.0000000001),
                decimals: 9
            }
        );
        let err = usdc().to_native(dec!(1.0000001)).unwrap_err();
        assert!(matches!(err, AssetError::TooPrecise { decimals: 6, .. }));
    }

    #[test]
    fn to_native_rejects_negative() {
        assert_eq!(
            sol().to_native(dec!(-1)).unwrap_err(),
            AssetError::Negative(dec!(-1))
        );
        // Negative zero is just zero.
        assert_eq!(sol().to_native(dec!(-0)).unwrap(), 0);
    }

    #[test]
    fn to_native_rejects_overflow() {
        // u64::MAX lamports is ~18.4 billion SOL; one more unit must fail.
        let max = sol().to_ui(u64::MAX);
        assert_eq!(sol().to_native(max).unwrap(), u64::MAX);
        let over = max + dec!(0.000000001);
        assert_eq!(
            sol().to_native(over).unwrap_err(),
            AssetError::Overflow(over)
        );
    }

    #[test]
    fn to_ui_exact_and_round_trips() {
        assert_eq!(sol().to_ui(1_500_000_000), dec!(1.500000000));
        assert_eq!(sol().to_ui(1_500_000_000).normalize(), dec!(1.5));
        assert_eq!(usdc().to_ui(1), dec!(0.000001));
        assert_eq!(sol().to_ui(0), dec!(0.000000000));
        for native in [
            0u64,
            1,
            999_999_999,
            1_000_000_000,
            123_456_789_012,
            u64::MAX,
        ] {
            assert_eq!(sol().to_native(sol().to_ui(native)).unwrap(), native);
        }
    }
}
