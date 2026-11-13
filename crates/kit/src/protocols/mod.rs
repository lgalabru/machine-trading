//! Protocol adapters: one module per venue, one submodule per program, each
//! behind a Cargo feature.
//!
//! | Venue | Program | Trades through | Holds | Measured by |
//! |-------|---------|----------------|-------|-------------|
//! | [`phoenix`] | [`phoenix::eternal`] | `PhoenixDriver` | perp `Position` | `PhoenixEternalMeter` |
//! | [`phoenix`] | [`phoenix::v1`] | `PhoenixV1Driver` | `SpotBalances` | `PhoenixV1Meter` |
//! | [`openbook`] | [`openbook::v2`] | `OpenBookV2Driver` | `SpotBalances` | `OpenBookV2Meter` |
//!
//! An adapter maps its program onto the [`core`](crate::core) model, as
//! `docs/rfd/0001-instrument-traits.md` lays out: its markets become
//! [`Instrument`](crate::core::Instrument)s named by the mandate's reference
//! (`venue:kind:symbol`), its books [`OrderBook`](crate::core::OrderBook)s,
//! its order flow an [`ExecutionDriver`](crate::core::ExecutionDriver), what
//! an account holds a [`Holding`](crate::core::Holding), and what an
//! enforcer reads an [`AccountMeter`](crate::core::AccountMeter), through the
//! enforcer's own [`ChainClient`](crate::chain::ChainClient). A program's
//! trader and its meter read the account with the same function.
//! Transactions go through [`chain`](crate::chain).

#[cfg(feature = "openbook-v2-venue")]
pub mod openbook;
#[cfg(any(feature = "phoenix", feature = "phoenix-v1-venue"))]
pub mod phoenix;
#[cfg(feature = "pyth")]
pub mod pyth;
#[cfg(any(feature = "openbook-v2", feature = "phoenix-v1"))]
pub mod spot;

/// Account data that didn't have the shape its program writes: what a
/// decoder that needs no chain reports. The chain's
/// [`ChainError::Decode`](crate::chain::ChainError::Decode) carries the same.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("decode {what}: {detail}")]
pub struct DecodeError {
    pub what: &'static str,
    pub detail: String,
}

impl DecodeError {
    pub fn new(what: &'static str, detail: impl Into<String>) -> Self {
        Self {
            what,
            detail: detail.into(),
        }
    }
}

/// A spot market a venue profile in the repository's `specs/venues/` names
/// by symbol, such as `SOL-USDC`. Any other market on the venue is named by
/// its address.
#[cfg(feature = "spot-venue")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NamedMarket {
    /// The profile's symbol, `BASE-QUOTE`.
    pub symbol: &'static str,
    /// The market account.
    pub address: solana_pubkey::Pubkey,
    /// The base asset's mint.
    pub base_mint: solana_pubkey::Pubkey,
    /// The quote asset's mint.
    pub quote_mint: solana_pubkey::Pubkey,
}

#[cfg(feature = "spot-venue")]
impl NamedMarket {
    /// The market `symbol` names among `markets`, or the one at `symbol`
    /// read as a base58 address.
    pub fn find<'a>(markets: &'a [NamedMarket], symbol: &str) -> Option<&'a NamedMarket> {
        markets
            .iter()
            .find(|m| m.symbol == symbol || m.address.to_string() == symbol)
    }

    /// The symbol a market's instrument reference uses: its name when
    /// `markets` names it, otherwise its address.
    pub fn symbol_of(markets: &[NamedMarket], address: &solana_pubkey::Pubkey) -> String {
        markets
            .iter()
            .find(|m| &m.address == address)
            .map_or_else(|| address.to_string(), |m| m.symbol.to_string())
    }
}

/// One asset's [`Holdings`](crate::core::Holdings) from its amounts in atoms:
/// in the wallet, free on the venue, and locked behind orders.
#[cfg(any(feature = "openbook-v2", feature = "phoenix-v1"))]
pub(crate) fn holdings_from_atoms(
    asset: &crate::core::Asset,
    wallet: i128,
    free: i128,
    locked: i128,
) -> Result<crate::core::Holdings, crate::chain::ChainError> {
    let units = |atoms: i128| {
        rust_decimal::Decimal::try_from_i128_with_scale(atoms, u32::from(asset.decimals)).map_err(
            |_| crate::chain::ChainError::Decode {
                what: "holdings",
                detail: format!("{atoms} atoms of {} don't fit a decimal", asset.symbol),
            },
        )
    };
    Ok(crate::core::Holdings {
        base: asset.clone(),
        wallet: units(wallet)?,
        free: units(free)?,
        locked: units(locked)?,
    })
}

/// Wrapped SOL's mint. A profile counts SOL held in its token account, not
/// the wallet's native SOL, which pays the transaction fees.
#[cfg(feature = "spot-venue")]
pub(crate) const WRAPPED_SOL_MINT: solana_pubkey::Pubkey =
    solana_pubkey::pubkey!("So11111111111111111111111111111111111111112");

/// USDC's mint: the quote of every spot market the profiles name, and what
/// Phoenix Eternal's deposits are made from (Phoenix wraps it into its own
/// canonical mint).
pub const USDC_MINT: solana_pubkey::Pubkey =
    solana_pubkey::pubkey!("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");
