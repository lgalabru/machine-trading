use phoenix_rise::{
    core::{LimitOrderTicket, MarketOrderTicket},
    math::Side,
};
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_rpc_client::rpc_client::RpcClient;
use solana_signer::Signer;
use solana_transaction::Signature;

use crate::{BotEnv, Phoenix, Result, rpc_client, send};

/// A bot's handle on its Phoenix account in one market.
pub struct Trader {
    pub env: BotEnv,
    pub keypair: Keypair,
    /// The cross-margin trader account (PDA) holding the collateral.
    pub account: Pubkey,
    pub phoenix: Phoenix,
    pub rpc: RpcClient,
}

/// A position in base units (positive is long) and its average entry in USD.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Position {
    pub size: f64,
    pub entry_usd: f64,
}

impl Trader {
    pub fn connect(env: &BotEnv) -> Result<Self> {
        let keypair = env.keypair()?;
        let phoenix = Phoenix::new(env.exchange()?, &env.rpc_url);
        Ok(Self {
            account: phoenix.trader_account(&keypair.pubkey()),
            rpc: rpc_client(&env.rpc_url),
            env: env.clone(),
            keypair,
            phoenix,
        })
    }

    pub fn authority(&self) -> Pubkey {
        self.keypair.pubkey()
    }

    pub fn slot(&self) -> Result<u64> {
        Ok(self.rpc.get_slot()?)
    }

    pub fn lots(&self, base_units: f64) -> Result<u64> {
        let market = self.phoenix.market(&self.env.symbol)?;
        Ok(self.phoenix.base_units_to_lots(market, base_units))
    }

    pub async fn mark_usd(&self) -> Result<f64> {
        let market = self.phoenix.market(&self.env.symbol)?;
        let bbo = self
            .phoenix
            .hawkeye(self.authority())
            .view_bbo(&self.env.symbol)
            .await?
            .value;
        Ok(self.phoenix.ticks_to_usd(market, bbo.mark_price_ticks))
    }

    pub async fn position(&self) -> Result<Position> {
        let market = self.phoenix.market(&self.env.symbol)?;
        let asset = self
            .phoenix
            .hawkeye(self.authority())
            .view_margin_for_asset(self.authority(), 0, 0, market.asset_id)
            .await?
            .value;
        Ok(Position {
            size: self.phoenix.lots_to_base_units(market, asset.base_lots),
            entry_usd: self
                .phoenix
                .entry_price_to_usd(market, asset.entry_price_quote_lots_per_base_lot),
        })
    }

    /// Rest an order on the book at `price_usd`, or fill against it if the
    /// price crosses.
    pub async fn place_limit(
        &self,
        side: Side,
        price_usd: f64,
        base_units: f64,
    ) -> Result<Signature> {
        let ticket = LimitOrderTicket::builder()
            .authority(self.authority())
            .trader_account(self.account)
            .symbol(&self.env.symbol)
            .side(side)
            .price(price_usd)
            .num_base_lots(self.lots(base_units)?)
            .build()?;
        let ixs = self.phoenix.tx().place_limit_order(ticket).await?;
        send(&self.rpc, &self.keypair, ixs, "limit order")
    }

    /// Fill against the book, at no worse than `limit_usd`.
    pub async fn place_market(
        &self,
        side: Side,
        base_units: f64,
        limit_usd: f64,
    ) -> Result<Signature> {
        let ticket = MarketOrderTicket::builder()
            .authority(self.authority())
            .trader_account(self.account)
            .symbol(&self.env.symbol)
            .side(side)
            .price(limit_usd)
            .num_base_lots(self.lots(base_units)?)
            .build()?;
        let ixs = self.phoenix.tx().place_market_order(ticket).await?;
        send(&self.rpc, &self.keypair, ixs, "market order")
    }

    pub fn cancel_all(&self) -> Result<Signature> {
        let ixs = self.phoenix.tx().build_cancel_all_orders(
            self.authority(),
            self.account,
            &self.env.symbol,
        )?;
        send(&self.rpc, &self.keypair, ixs, "cancel all")
    }
}
