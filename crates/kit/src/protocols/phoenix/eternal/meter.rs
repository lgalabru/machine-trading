//! [`PhoenixEternalMeter`]: what an enforcer measures of an owner's trader
//! accounts on Phoenix Eternal, as the Phoenix venue profile (Perpetual
//! markets) defines it.
//!
//! Everything is read from Phoenix through Hawkeye's views, and the wallet
//! and the owner's trader accounts from the chain, through the meter's own
//! clients, never the bot's.

use std::sync::Arc;

use phoenix_rise::{
    accounts::{PhoenixAccount, trader::TraderHeader},
    core::PhoenixHawkeyeClient,
    ix::{
        PHOENIX_PROGRAM_ID,
        hawkeye::{ViewAssetReturn, ViewMarginReturn},
    },
};
use rust_decimal::Decimal;
use solana_pubkey::Pubkey;
use solana_rpc_client_api::filter::{Memcmp, RpcFilterType};

use super::{PhoenixError, PhoenixExchange, PhoenixMarket};
use crate::{
    chain::{ChainClient, token},
    core::{
        AccountMeter, Holding, InstrumentId, Measurement, MeterError, Position, RecordedFill,
    },
    protocols::USDC_MINT,
};

const USDC_DECIMALS: u32 = 6;

/// One owner's trader accounts, measured on one perp market: the
/// cross-margin account and every other the owner registered (another PDA
/// index, an isolated subaccount), summed, so no position hides in one.
pub struct PhoenixEternalMeter {
    exchange: Arc<PhoenixExchange>,
    rpc_url: String,
    chain: Arc<dyn ChainClient>,
    owner: Pubkey,
    trader: Pubkey,
    market: PhoenixMarket,
    unsettled_funding: bool,
}

impl PhoenixEternalMeter {
    /// Measures `owner`'s trader account on `symbol`: Hawkeye's views at
    /// `rpc_url`, the wallet through `chain`.
    pub fn new(
        exchange: Arc<PhoenixExchange>,
        rpc_url: impl Into<String>,
        chain: Arc<dyn ChainClient>,
        owner: Pubkey,
        symbol: &str,
    ) -> Result<Self, PhoenixError> {
        Ok(Self {
            market: exchange.market(symbol)?,
            trader: exchange.trader_account(&owner),
            exchange,
            rpc_url: rpc_url.into(),
            chain,
            owner,
            unsettled_funding: false,
        })
    }

    /// Whether the account's value counts the funding it has accrued and not
    /// settled yet. Phoenix settles funding into the collateral only when the
    /// account next trades, so a position held through funding intervals is
    /// worth its collateral, its unrealized PnL and that funding. Off by
    /// default: the account is valued as the venue profile defines it.
    pub fn with_unsettled_funding(mut self, unsettled_funding: bool) -> Self {
        self.unsettled_funding = unsettled_funding;
        self
    }

    /// The owner's cross-margin account, which the meter always measures.
    pub fn trader(&self) -> Pubkey {
        self.trader
    }

    /// The instrument this meter measures, as the mandate names it.
    pub fn instrument(&self) -> &InstrumentId {
        &self.market.instrument.id
    }

    fn hawkeye(&self) -> PhoenixHawkeyeClient<'_> {
        self.exchange.hawkeye(&self.rpc_url, self.owner)
    }

    /// Each trader account's position in the market, read first, then its
    /// margin.
    async fn readings(&self, traders: &[Pubkey]) -> Result<Vec<Reading>, MeterError> {
        let hawkeye = self.hawkeye();
        let mut readings = Vec::with_capacity(traders.len());
        for &trader in traders {
            let asset = hawkeye
                .view_margin_for_asset_for_trader(trader, self.market.asset_id)
                .await
                .map_err(read)?
                .value;
            let margin = hawkeye
                .view_margin_for_trader(trader)
                .await
                .map_err(read)?
                .value;
            readings.push(Reading { asset, margin });
        }
        Ok(readings)
    }

    /// The trader accounts, with the wallet's collateral: what Phoenix holds
    /// across them (collateral plus unrealized PnL at the mark, plus, when
    /// the meter counts it, funding accrued and not settled yet) and USDC
    /// withdrawn back to the wallet, which keeps its value so a withdrawal
    /// can't hide a drawdown.
    async fn value_usd(&self, readings: &[Reading]) -> Result<Decimal, MeterError> {
        let quote_lots = quote_lots(readings, self.unsettled_funding);
        // Real USDC, which deposits take from: the canonical mint
        // (`collateral_mint`) lives only inside Phoenix.
        let wallet = token::owned_amount(self.chain.as_ref(), &self.owner, &USDC_MINT)
            .await
            .map_err(read)?;
        Ok(
            Decimal::new(quote_lots, u32::from(self.market.quote_lot_decimals))
                + Decimal::from_i128_with_scale(i128::from(wallet), USDC_DECIMALS),
        )
    }
}

#[async_trait::async_trait]
impl AccountMeter for PhoenixEternalMeter {
    /// Each account's market position, read first, then its margin. A
    /// count of markets held outside the universe is only kept if a second
    /// round of reads agrees: the market's view and the account's are two
    /// reads, and an order the bot places or cancels between them shows up
    /// in one and not the other, which looks like a position elsewhere. A
    /// position that really is elsewhere shows up on every read.
    async fn measure(&self, universe: &[InstrumentId]) -> Result<Measurement, MeterError> {
        let traders = traders(self.chain.as_ref(), &self.owner, self.trader).await?;
        let readings = self.readings(&traders).await?;
        let value_usd = self.value_usd(&readings).await?;
        let outside_universe = match outside(&self.market, &readings, universe) {
            0 => 0,
            seen => seen.min(outside(
                &self.market,
                &self.readings(&traders).await?,
                universe,
            )),
        };
        Ok(Measurement {
            holdings: vec![(
                self.instrument().clone(),
                Holding::Perp(position(&self.market, &readings)),
            )],
            value_usd,
            outside_universe,
        })
    }

    async fn fills(&self, _after_slot: u64) -> Result<Vec<RecordedFill>, MeterError> {
        Err(MeterError::Unsupported(
            "Phoenix Eternal fills: no task's mandate has a fill rule yet".into(),
        ))
    }

    /// Whether any of the owner's trader accounts is liquidatable.
    async fn liquidatable(&self) -> Result<bool, MeterError> {
        let hawkeye = self.hawkeye();
        for trader in traders(self.chain.as_ref(), &self.owner, self.trader).await? {
            let margin = hawkeye
                .view_margin_for_trader(trader)
                .await
                .map_err(read)?
                .value;
            if margin.is_liquidatable != 0 {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

/// Every trader account `owner` holds, found on chain: `main`, its
/// cross-margin account, first, then the rest in address order.
async fn traders(
    chain: &dyn ChainClient,
    owner: &Pubkey,
    main: Pubkey,
) -> Result<Vec<Pubkey>, MeterError> {
    let filters = vec![
        RpcFilterType::Memcmp(Memcmp::new_raw_bytes(
            0,
            PhoenixAccount::Trader.discriminant().to_vec(),
        )),
        RpcFilterType::Memcmp(Memcmp::new_raw_bytes(
            std::mem::offset_of!(TraderHeader, authority),
            owner.to_bytes().to_vec(),
        )),
    ];
    let mut others: Vec<Pubkey> = chain
        .get_program_accounts(&PHOENIX_PROGRAM_ID, filters)
        .await
        .map_err(read)?
        .into_iter()
        .map(|(key, _)| key)
        .filter(|key| *key != main)
        .collect();
    others.sort();
    others.dedup();
    Ok([main].into_iter().chain(others).collect())
}

/// What Phoenix holds across the trader accounts: collateral plus
/// unrealized PnL, plus unsettled funding if `unsettled_funding`.
fn quote_lots(readings: &[Reading], unsettled_funding: bool) -> i64 {
    readings
        .iter()
        .map(|r| {
            let margin = &r.margin;
            let mut lots = margin.collateral_quote_lots + margin.unrealized_pnl_quote_lots;
            if unsettled_funding {
                lots += margin.unsettled_funding_quote_lots;
            }
            lots
        })
        .sum()
}

/// The position in `market`, netted across the trader accounts.
fn position(market: &PhoenixMarket, readings: &[Reading]) -> Position {
    let mut legs = readings
        .iter()
        .map(|r| market.position(r.asset.base_lots, r.asset.entry_price_quote_lots_per_base_lot));
    let first = legs
        .next()
        .expect("the cross-margin account is always read");
    legs.fold(first, net)
}

/// Markets held other than the universe's, across the trader accounts:
/// each account's count, less `market` where it is in the universe.
fn outside(market: &PhoenixMarket, readings: &[Reading], universe: &[InstrumentId]) -> u32 {
    let counted = universe.contains(&market.instrument.id);
    readings
        .iter()
        .map(|r| {
            let held = r.asset.has_position_or_orders != 0 && counted;
            u32::from(r.margin.position_count.saturating_sub(u16::from(held)))
        })
        .sum()
}

/// One trader account's reads.
#[derive(Clone, Copy)]
struct Reading {
    /// Its position or orders in the meter's market.
    asset: ViewAssetReturn,
    /// Its collateral, PnL and the markets it holds a position or orders in.
    margin: ViewMarginReturn,
}

/// Two legs of one market's position, held in different accounts, as one:
/// their quantities summed, their entry notionals netted by side.
fn net(a: Position, b: Position) -> Position {
    let signed = |p: &Position| {
        if p.quantity.is_sign_negative() {
            -p.entry_notional
        } else {
            p.entry_notional
        }
    };
    Position {
        quantity: a.quantity + b.quantity,
        entry_notional: (signed(&a) + signed(&b)).abs(),
        ..a
    }
}

fn read(error: impl std::fmt::Display) -> MeterError {
    MeterError::Read(error.to_string())
}

#[cfg(test)]
mod tests {
    use phoenix_rise::api::TraderKey;
    use rust_decimal_macros::dec;
    use solana_account::Account as ChainAccount;

    use super::*;
    use crate::{
        chain::mock::MockChainClient,
        core::{Asset, FeeSchedule},
    };

    fn sol() -> PhoenixMarket {
        let usdc = Asset::new("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v", "USDC", 6).unwrap();
        PhoenixMarket::from_parts("SOL", 7, 100, 2, usdc, FeeSchedule::ZERO).unwrap()
    }

    /// A Phoenix account of `kind` whose header names `authority`.
    fn account(kind: PhoenixAccount, authority: &Pubkey) -> ChainAccount {
        let mut data = vec![0; size_of::<TraderHeader>()];
        data[..8].copy_from_slice(&kind.discriminant());
        let at = std::mem::offset_of!(TraderHeader, authority);
        data[at..at + 32].copy_from_slice(authority.as_ref());
        ChainAccount {
            lamports: 1,
            data,
            owner: *PHOENIX_PROGRAM_ID,
            executable: false,
            rent_epoch: 0,
        }
    }

    fn reading(base_lots: i64, entry: u64, collateral: i64, pnl: i64, count: u16) -> Reading {
        Reading {
            asset: ViewAssetReturn {
                base_lots,
                entry_price_quote_lots_per_base_lot: entry,
                has_position_or_orders: u8::from(base_lots != 0),
                ..ViewAssetReturn::default()
            },
            margin: ViewMarginReturn {
                collateral_quote_lots: collateral,
                unrealized_pnl_quote_lots: pnl,
                unsettled_funding_quote_lots: -5,
                position_count: count,
                ..ViewMarginReturn::default()
            },
        }
    }

    #[tokio::test]
    async fn finds_every_trader_account_the_owner_holds() {
        let owner = Pubkey::new_unique();
        let main = TraderKey::derive_pda(&owner, 0, 0);
        let isolated = TraderKey::derive_pda(&owner, 0, 1);
        let second = TraderKey::derive_pda(&owner, 3, 0);
        let chain = MockChainClient::new();
        for key in [main, isolated, second] {
            chain.set_account(key, account(PhoenixAccount::Trader, &owner));
        }
        // Another owner's trader account, and a Phoenix account of another
        // kind that happens to carry the owner's key at the same offset.
        let other = Pubkey::new_unique();
        chain.set_account(
            TraderKey::derive_pda(&other, 0, 0),
            account(PhoenixAccount::Trader, &other),
        );
        chain.set_account(
            Pubkey::new_unique(),
            account(PhoenixAccount::Market, &owner),
        );

        let found = traders(&chain, &owner, main).await.unwrap();
        let mut rest = vec![isolated, second];
        rest.sort();
        assert_eq!(found, [vec![main], rest].concat());

        // An owner with only the cross-margin account: just that one.
        let alone = Pubkey::new_unique();
        let alone_main = TraderKey::derive_pda(&alone, 0, 0);
        chain.set_account(alone_main, account(PhoenixAccount::Trader, &alone));
        assert_eq!(
            traders(&chain, &alone, alone_main).await.unwrap(),
            [alone_main]
        );
    }

    #[test]
    fn sums_the_accounts() {
        let sol = sol();
        let universe = [sol.instrument.id.clone()];
        // The cross-margin account: 1.50 SOL long at 150, 2 markets (SOL
        // and one more); an isolated subaccount: 4.00 SOL long at 160.
        let main = reading(150, 1_500_000, 900_000_000, 3_000_000, 2);
        let isolated = reading(400, 1_600_000, 100_000_000, -1_000_000, 1);
        let both = [main, isolated];

        let p = position(&sol, &both);
        assert_eq!(p.quantity, dec!(5.5));
        assert_eq!(p.entry_notional, dec!(865));
        assert_eq!(quote_lots(&both, false), 1_002_000_000);
        assert_eq!(quote_lots(&both, true), 1_001_999_990);
        assert_eq!(outside(&sol, &both, &universe), 1);
        // Outside the universe, every account's SOL counts too.
        assert_eq!(outside(&sol, &both, &[]), 3);

        // A short leg nets against the long one.
        let short = reading(-400, 1_600_000, 100_000_000, 0, 1);
        let p = position(&sol, &[main, short]);
        assert_eq!(p.quantity, dec!(-2.5));
        assert_eq!(p.entry_notional, dec!(415));

        // One account alone is that account, as Phoenix reports it.
        let p = position(&sol, &[main]);
        assert_eq!(p, sol.position(150, 1_500_000));
        assert_eq!(outside(&sol, &[main], &universe), 1);
        // An isolated subaccount in a market outside the universe.
        let elsewhere = reading(0, 0, 100_000_000, 0, 1);
        assert_eq!(outside(&sol, &[reading(0, 0, 1, 0, 0), elsewhere], &universe), 1);
    }
}
