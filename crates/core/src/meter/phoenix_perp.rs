//! The grader's measurements of a Phoenix perp account: [`PhoenixPerpMeter`],
//! the first [`AccountMeter`] (`docs/rfd/0001-instrument-traits.md`, step 2).
//!
//! Everything here is read from Phoenix through Hawkeye's views and from the
//! chain, never from the bot. The session turns what it reports into the
//! grader's `f64`s.

use std::str::FromStr;

use mtx_botkit::{
    Decimal, QUOTE_LOTS_PER_USD,
    luce::{
        core::{AccountMeter, Holding, InstrumentId, Measurement, MeterError, RecordedFill},
        protocols::phoenix::PhoenixMarket,
    },
    phoenix_rise::{
        core::PhoenixHawkeyeClient, ix::hawkeye::ViewAssetReturn,
        types::exchange::ExchangeMarketConfig,
    },
};
use serde_json::json;
use solana_pubkey::Pubkey;
use solana_rpc_client::rpc_client::RpcClient;

use crate::{
    chain,
    phoenix::{Phoenix, USDC_MINT},
};

/// One trader account on one Phoenix perp market.
pub struct PhoenixPerpMeter<'a> {
    rpc: &'a RpcClient,
    hawkeye: PhoenixHawkeyeClient<'a>,
    owner: Pubkey,
    trader: Pubkey,
    market: &'a ExchangeMarketConfig,
    luce_market: PhoenixMarket,
}

impl<'a> PhoenixPerpMeter<'a> {
    /// The meter for `owner`'s trader account `trader` on `market`.
    pub fn new(
        rpc: &'a RpcClient,
        phoenix: &'a Phoenix,
        owner: Pubkey,
        trader: Pubkey,
        market: &'a ExchangeMarketConfig,
    ) -> crate::Result<Self> {
        Ok(Self {
            rpc,
            hawkeye: phoenix.hawkeye(owner),
            owner,
            trader,
            market,
            luce_market: phoenix.luce_market(&market.symbol)?,
        })
    }

    /// The instrument this meter measures, as the universe names it.
    pub fn instrument(&self) -> &InstrumentId {
        &self.luce_market.instrument.id
    }

    async fn asset(&self) -> Result<ViewAssetReturn, MeterError> {
        Ok(self
            .hawkeye
            .view_margin_for_asset(self.owner, 0, 0, self.market.asset_id)
            .await
            .map_err(read)?
            .value)
    }

    /// The trader account, from one margin view, with the wallet's USDC.
    async fn account(&self) -> Result<Account, MeterError> {
        let margin = self
            .hawkeye
            .view_margin_for_trader(self.trader)
            .await
            .map_err(read)?
            .value;
        let quote_lots = margin.collateral_quote_lots + margin.unrealized_pnl_quote_lots;
        Ok(Account {
            value_usd: Decimal::from(quote_lots) / quote_lots_per_usd() + self.wallet_usdc()?,
            position_count: margin.position_count,
        })
    }

    /// USDC withdrawn back to the owner's wallet.
    fn wallet_usdc(&self) -> Result<Decimal, MeterError> {
        let accounts = chain::call(
            self.rpc,
            "getTokenAccountsByOwner",
            json!([self.owner.to_string(), { "mint": USDC_MINT.to_string() }, { "encoding": "jsonParsed" }]),
        )
        .map_err(read)?;
        Ok(accounts["value"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|account| {
                Decimal::from_str(
                    account["account"]["data"]["parsed"]["info"]["tokenAmount"]["uiAmountString"]
                        .as_str()?,
                )
                .ok()
            })
            .sum())
    }

    /// Markets held other than the universe's, from one pair of reads.
    fn outside(
        &self,
        account: &Account,
        asset: &ViewAssetReturn,
        universe: &[InstrumentId],
    ) -> u32 {
        let held = asset.has_position_or_orders != 0 && universe.contains(self.instrument());
        u32::from(account.position_count.saturating_sub(u16::from(held)))
    }
}

#[async_trait::async_trait]
impl AccountMeter for PhoenixPerpMeter<'_> {
    /// The market's position, read first, then the account. A count of
    /// markets held outside the universe is only kept if a second pair of
    /// reads agrees: the market's view and the account's are two reads, and
    /// an order the bot places or cancels between them shows up in one and
    /// not the other, which looks like a position elsewhere. A position that
    /// really is elsewhere shows up on every read.
    async fn measure(&self, universe: &[InstrumentId]) -> Result<Measurement, MeterError> {
        let asset = self.asset().await?;
        let account = self.account().await?;
        let outside_universe = match self.outside(&account, &asset, universe) {
            0 => 0,
            seen => {
                let again = self.asset().await?;
                seen.min(self.outside(&self.account().await?, &again, universe))
            }
        };
        let position = self
            .luce_market
            .position(asset.base_lots, asset.entry_price_quote_lots_per_base_lot);
        Ok(Measurement {
            holdings: vec![(self.instrument().clone(), Holding::Perp(position))],
            value_usd: account.value_usd,
            outside_universe,
        })
    }

    async fn fills(&self, _after_slot: u64) -> Result<Vec<RecordedFill>, MeterError> {
        Err(MeterError::Unsupported(
            "Phoenix perp fills: no task's mandate has a fill rule yet".into(),
        ))
    }

    async fn liquidatable(&self) -> Result<bool, MeterError> {
        let margin = self
            .hawkeye
            .view_margin(self.owner, 0, 0)
            .await
            .map_err(read)?
            .value;
        Ok(margin.is_liquidatable != 0)
    }
}

struct Account {
    /// Phoenix collateral plus unrealized PnL at mark, plus any USDC
    /// withdrawn back to the wallet.
    value_usd: Decimal,
    /// Markets the trader holds a position or orders in.
    position_count: u16,
}

fn quote_lots_per_usd() -> Decimal {
    Decimal::try_from(QUOTE_LOTS_PER_USD).expect("a power of ten")
}

fn read(error: impl std::fmt::Display) -> MeterError {
    MeterError::Read(error.to_string())
}
