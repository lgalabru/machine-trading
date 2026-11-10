//! Protocol adapters, each behind a Cargo feature.
//!
//! An adapter maps one protocol onto the [`core`](crate::core) model: its
//! markets become [`Instrument`](crate::core::Instrument)s, its books
//! [`OrderBook`](crate::core::OrderBook)s, its accounts
//! [`Position`](crate::core::Position)s, and its order flow an
//! [`ExecutionDriver`](crate::core::ExecutionDriver). Transactions go through
//! [`chain`](crate::chain).

#[cfg(feature = "openbook-v2")]
pub mod openbook_v2;
#[cfg(feature = "phoenix")]
pub mod phoenix;
#[cfg(feature = "phoenix-v1")]
pub mod phoenix_v1;
#[cfg(feature = "pyth")]
pub mod pyth;

/// A spot market a venue profile in the repository's `specs/venues/` names
/// by symbol, such as `SOL-USDC`. Any other market on the venue is named by
/// its address.
#[cfg(any(feature = "openbook-v2", feature = "phoenix-v1"))]
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

#[cfg(any(feature = "openbook-v2", feature = "phoenix-v1"))]
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

/// Wrapped SOL's mint. A profile counts SOL held in its token account, not
/// the wallet's native SOL, which pays the transaction fees.
#[cfg(any(feature = "openbook-v2", feature = "phoenix-v1"))]
pub(crate) const WRAPPED_SOL_MINT: solana_pubkey::Pubkey =
    solana_pubkey::pubkey!("So11111111111111111111111111111111111111112");

/// USDC's mint.
#[cfg(any(feature = "openbook-v2", feature = "phoenix-v1"))]
pub(crate) const USDC_MINT: solana_pubkey::Pubkey =
    solana_pubkey::pubkey!("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");
