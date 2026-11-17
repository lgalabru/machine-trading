//! [`PhoenixEternalMeter`]: what an enforcer measures of an owner's trader
//! accounts on Phoenix Eternal, as the Phoenix venue profile (Perpetual
//! markets) defines it.
//!
//! Everything is read from Phoenix through Hawkeye's views, and the wallet,
//! the owner's trader accounts and the market's fills from the chain,
//! through the meter's own clients, never the bot's.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use futures::future::join_all;
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
use solana_transaction::Signature;

use super::{
    PhoenixError, PhoenixExchange, PhoenixMarket,
    fills::{PerpMarketRecords, TraderRecords},
};
use crate::{
    chain::{ChainClient, token},
    core::{AccountMeter, Holding, InstrumentId, Measurement, MeterError, Position, RecordedFill},
    protocols::USDC_MINT,
};

const USDC_DECIMALS: u32 = 6;

/// One owner's trader accounts, measured on one or more perp markets: the
/// cross-margin account and every other the owner registered (another PDA
/// index, an isolated subaccount), summed, so no position hides in one.
/// Phoenix margins every market of an account together: the meter reads
/// each account's margin once, and its position in each measured market.
pub struct PhoenixEternalMeter {
    exchange: Arc<PhoenixExchange>,
    rpc_url: String,
    chain: Arc<dyn ChainClient>,
    owner: Pubkey,
    trader: Pubkey,
    /// The markets measured, in the order given: never empty, each once,
    /// all counting quote lots alike.
    markets: Vec<PhoenixMarket>,
    unsettled_funding: bool,
}

impl PhoenixEternalMeter {
    /// Measures `owner`'s trader account on `symbol`: Hawkeye's views at
    /// `rpc_url`, the wallet through `chain`. The same as
    /// [`PhoenixEternalMeter::for_markets`] on that one market.
    pub fn new(
        exchange: Arc<PhoenixExchange>,
        rpc_url: impl Into<String>,
        chain: Arc<dyn ChainClient>,
        owner: Pubkey,
        symbol: &str,
    ) -> Result<Self, PhoenixError> {
        Self::for_markets(exchange, rpc_url, chain, owner, [symbol])
    }

    /// Measures `owner`'s trader account on each of `symbols`, one holding
    /// each, in that order; a position or order in any other market counts
    /// as outside the universe. Fails on no market, an unknown one, one
    /// given twice, and markets whose quote lots differ (the account's
    /// collateral is one number, in one quote lot).
    pub fn for_markets(
        exchange: Arc<PhoenixExchange>,
        rpc_url: impl Into<String>,
        chain: Arc<dyn ChainClient>,
        owner: Pubkey,
        symbols: impl IntoIterator<Item: AsRef<str>>,
    ) -> Result<Self, PhoenixError> {
        let markets = measured(
            symbols
                .into_iter()
                .map(|symbol| exchange.market(symbol.as_ref()))
                .collect::<Result<_, _>>()?,
        )?;
        Ok(Self {
            markets,
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

    /// The first market this meter measures, as the mandate names it.
    #[deprecated(note = "a meter may measure several markets: use `instruments`")]
    pub fn instrument(&self) -> &InstrumentId {
        &self.markets[0].instrument.id
    }

    /// Every market this meter measures, as the mandate names them, in the
    /// order given: one holding each in [`AccountMeter::measure`].
    pub fn instruments(&self) -> impl ExactSizeIterator<Item = &InstrumentId> {
        self.markets.iter().map(|market| &market.instrument.id)
    }

    /// Every fill, order placed to rest and position change the measured
    /// markets recorded in slots after `after_slot`, between any trader
    /// accounts, in order (see [`PerpMarketRecords::sort`]): the events of
    /// every landed transaction that named a market's order book, each
    /// transaction fetched and decoded once, whichever markets it traded. A
    /// position change names only the trader accounts it moves, so this
    /// holds those of transactions that also named a book;
    /// [`PhoenixEternalMeter::records`] reads the owner's own.
    pub async fn market_records(&self, after_slot: u64) -> Result<PerpMarketRecords, MeterError> {
        self.read_records(&self.order_books()?, after_slot).await
    }

    /// What the owner's trader accounts did on the measured markets in
    /// slots after `after_slot`: their fills, as a taker or a maker, the
    /// orders they placed to rest, and their position changes that were
    /// not fills, read from the transactions that named a market's order
    /// book or one of the accounts. On each market, the fills and the
    /// position changes sum to the accounts' change in position.
    pub async fn records(&self, after_slot: u64) -> Result<TraderRecords, MeterError> {
        let traders = traders(self.chain.as_ref(), &self.owner, self.trader).await?;
        let mut addresses = self.order_books()?;
        addresses.extend(&traders);
        Ok(self
            .read_records(&addresses, after_slot)
            .await?
            .for_traders(&traders))
    }

    /// Each measured market's order book account, which every trade on it
    /// names.
    fn order_books(&self) -> Result<Vec<Pubkey>, MeterError> {
        self.markets
            .iter()
            .map(|market| {
                self.exchange
                    .market_config(market.symbol())
                    .map_err(read)?
                    .market_pubkey
                    .parse::<Pubkey>()
                    .map_err(|e| MeterError::Decode {
                        what: "market address",
                        detail: format!("{}: {e}", market.id()),
                    })
            })
            .collect()
    }

    /// The records of the transactions that named any of `addresses` in
    /// slots after `after_slot`: the addresses' lists merged into one, in
    /// the order the transactions landed, and each transaction fetched and
    /// decoded once, on every measured market.
    async fn read_records(
        &self,
        addresses: &[Pubkey],
        after_slot: u64,
    ) -> Result<PerpMarketRecords, MeterError> {
        let chain = self.chain.as_ref();
        let mut listed = Vec::with_capacity(addresses.len());
        for address in addresses {
            listed.push(
                chain
                    .signatures_for_address(address, after_slot)
                    .await
                    .map_err(read)?,
            );
        }
        let mut records = PerpMarketRecords::default();
        for signature in landed_order(listed) {
            let summary = chain
                .get_transaction(&signature)
                .await
                .map_err(read)?
                .ok_or_else(|| {
                    MeterError::Read(format!("transaction {signature} is listed but not found"))
                })?;
            records.add_transaction(&summary, &self.markets)?;
        }
        records.sort();
        Ok(records)
    }

    fn hawkeye(&self) -> PhoenixHawkeyeClient<'_> {
        self.exchange.hawkeye(&self.rpc_url, self.owner)
    }

    /// Each trader account's position in each measured market and its
    /// margin, which is the account's across every market. The views are
    /// read together; within an account, each market's position before
    /// its margin, as the reads are issued.
    async fn readings(&self, traders: &[Pubkey]) -> Result<Vec<Reading>, MeterError> {
        let hawkeye = self.hawkeye();
        let hawkeye = &hawkeye;
        join_all(traders.iter().map(|&trader| async move {
            let assets = join_all(self.markets.iter().map(|market| async move {
                hawkeye
                    .view_margin_for_asset_for_trader(trader, market.asset_id)
                    .await
                    .map(|view| view.value)
                    .map_err(read)
            }))
            .await
            .into_iter()
            .collect::<Result<Vec<_>, _>>()?;
            let margin = hawkeye
                .view_margin_for_trader(trader)
                .await
                .map_err(read)?
                .value;
            Ok(Reading { assets, margin })
        }))
        .await
        .into_iter()
        .collect()
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
        // Every measured market counts quote lots alike (`for_markets`).
        Ok(
            Decimal::new(quote_lots, u32::from(self.markets[0].quote_lot_decimals))
                + Decimal::from_i128_with_scale(i128::from(wallet), USDC_DECIMALS),
        )
    }
}

#[async_trait::async_trait]
impl AccountMeter for PhoenixEternalMeter {
    /// Each account's positions, read first, then its margin. A count of
    /// markets held outside the universe is only kept if a second round of
    /// reads agrees: the market views and the account's are separate reads,
    /// and an order the bot places or cancels between them shows up in one
    /// and not the other, which looks like a position elsewhere. A position
    /// that really is elsewhere shows up on every read. The second round
    /// re-reads only the accounts that showed one.
    async fn measure(&self, universe: &[InstrumentId]) -> Result<Measurement, MeterError> {
        let traders = traders(self.chain.as_ref(), &self.owner, self.trader).await?;
        let readings = self.readings(&traders).await?;
        let value_usd = self.value_usd(&readings).await?;
        let seen: Vec<u32> = readings
            .iter()
            .map(|r| outside(&self.markets, r, universe))
            .collect();
        let showed: Vec<Pubkey> = traders
            .iter()
            .zip(&seen)
            .filter(|&(_, &n)| n > 0)
            .map(|(&trader, _)| trader)
            .collect();
        let outside_universe = if showed.is_empty() {
            0
        } else {
            let again: u32 = self
                .readings(&showed)
                .await?
                .iter()
                .map(|r| outside(&self.markets, r, universe))
                .sum();
            seen.iter().sum::<u32>().min(again)
        };
        Ok(Measurement {
            holdings: self
                .markets
                .iter()
                .enumerate()
                .map(|(i, market)| {
                    (
                        market.instrument.id.clone(),
                        Holding::Perp(position(market, i, &readings)),
                    )
                })
                .collect(),
            value_usd,
            outside_universe,
        })
    }

    /// The owner's fills on every measured market, as a taker or a maker,
    /// from Phoenix's fill events (see [`PhoenixEternalMeter::records`]). A
    /// liquidation transfer or auto-deleveraging moves a position without a
    /// fill, so these alone need not sum to the position: `records` has
    /// those too.
    async fn fills(&self, after_slot: u64) -> Result<Vec<RecordedFill>, MeterError> {
        Ok(self.records(after_slot).await?.fills)
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

/// `markets`, if a meter can measure them together: at least one, each
/// once, all counting quote lots alike.
fn measured(markets: Vec<PhoenixMarket>) -> Result<Vec<PhoenixMarket>, PhoenixError> {
    let Some(first) = markets.first() else {
        return Err(PhoenixError::NoMarkets);
    };
    for (i, market) in markets.iter().enumerate() {
        if markets[..i].iter().any(|m| m.asset_id == market.asset_id) {
            return Err(PhoenixError::DuplicateMarket(market.symbol().to_string()));
        }
        if market.quote_lot_decimals != first.quote_lot_decimals {
            return Err(PhoenixError::MixedQuoteLots {
                first: first.symbol().to_string(),
                other: market.symbol().to_string(),
            });
        }
    }
    Ok(markets)
}

/// The transactions in `listed` (each address's list, oldest first), once
/// each, in the order they landed: a merge that keeps every list's order,
/// taking at each turn the earliest slot among the transactions no list
/// still has later, then the first list's. Lists of one chain agree, so
/// that is always one of the heads; if they ever didn't, the earliest head
/// is taken, so the merge always ends.
fn landed_order(listed: Vec<Vec<(Signature, u64)>>) -> Vec<Signature> {
    let places: Vec<HashMap<Signature, usize>> = listed
        .iter()
        .map(|list| {
            list.iter()
                .enumerate()
                .map(|(i, (signature, _))| (*signature, i))
                .collect()
        })
        .collect();
    let mut next = vec![0usize; listed.len()];
    let mut taken = HashSet::new();
    let mut order = Vec::new();
    loop {
        for (list, at) in listed.iter().zip(next.iter_mut()) {
            while list.get(*at).is_some_and(|(s, _)| taken.contains(s)) {
                *at += 1;
            }
        }
        let heads = listed
            .iter()
            .zip(&next)
            .enumerate()
            .filter_map(|(i, (list, &at))| list.get(at).map(|&(s, slot)| (slot, i, s)));
        let free = |&(_, _, s): &(u64, usize, Signature)| {
            places
                .iter()
                .zip(&next)
                .all(|(place, &at)| place.get(&s).is_none_or(|&p| p <= at))
        };
        let Some((_, _, signature)) = heads
            .clone()
            .filter(free)
            .min_by_key(|&(slot, i, _)| (slot, i))
            .or_else(|| heads.min_by_key(|&(slot, i, _)| (slot, i)))
        else {
            return order;
        };
        taken.insert(signature);
        order.push(signature);
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

/// The position in `market`, the meter's `index`th, netted across the
/// trader accounts.
fn position(market: &PhoenixMarket, index: usize, readings: &[Reading]) -> Position {
    let mut legs = readings.iter().map(|r| {
        let asset = &r.assets[index];
        market.position(asset.base_lots, asset.entry_price_quote_lots_per_base_lot)
    });
    let first = legs
        .next()
        .expect("the cross-margin account is always read");
    legs.fold(first, net)
}

/// Markets one trader account holds other than the universe's: its count,
/// less each measured market (`markets`, read in that order) it holds that
/// is in the universe.
fn outside(markets: &[PhoenixMarket], reading: &Reading, universe: &[InstrumentId]) -> u32 {
    let held = markets
        .iter()
        .zip(&reading.assets)
        .filter(|(market, asset)| {
            asset.has_position_or_orders != 0 && universe.contains(&market.instrument.id)
        })
        .count();
    let held = u16::try_from(held).unwrap_or(u16::MAX);
    u32::from(reading.margin.position_count.saturating_sub(held))
}

/// One trader account's reads.
#[derive(Clone)]
struct Reading {
    /// Its position or orders in each of the meter's markets, in order.
    assets: Vec<ViewAssetReturn>,
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
    use phoenix_rise::{
        api::TraderKey,
        events::market_events::{
            CloseMatchedPositionsEvent, LiquidationTransferEvent, MarketEvent, MarketEventHeader,
            OptionalNonZeroU64, OrderFilledEvent, OrderFlags, OrderPlacedEvent, SlotContextEvent,
            SplineFilledEvent, Symbol, TradeSummaryEvent,
        },
        math::{
            BaseLots, QuoteLots, Side as MathSide, SignedBaseLots, SignedFeeRateMicro,
            SignedQuoteLots, SignedQuoteLotsPerBaseLot, Ticks,
        },
    };
    use rust_decimal_macros::dec;
    use solana_account::Account as ChainAccount;

    use super::*;
    use crate::{
        chain::{InnerInstruction, TransactionSummary, mock::MockChainClient},
        core::{Asset, FeeSchedule, Side},
        protocols::phoenix::eternal::{
            PerpPositionChange, PositionChangeKind, fills::testing::log_encoded,
        },
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

    fn asset(base_lots: i64, entry: u64) -> ViewAssetReturn {
        ViewAssetReturn {
            base_lots,
            entry_price_quote_lots_per_base_lot: entry,
            has_position_or_orders: u8::from(base_lots != 0),
            ..ViewAssetReturn::default()
        }
    }

    /// Across `readings`, the markets held outside `universe`.
    fn outside_all(
        markets: &[PhoenixMarket],
        readings: &[Reading],
        universe: &[InstrumentId],
    ) -> u32 {
        readings.iter().map(|r| outside(markets, r, universe)).sum()
    }

    fn reading(base_lots: i64, entry: u64, collateral: i64, pnl: i64, count: u16) -> Reading {
        Reading {
            assets: vec![asset(base_lots, entry)],
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
        let markets = std::slice::from_ref(&sol);
        let universe = [sol.instrument.id.clone()];
        // The cross-margin account: 1.50 SOL long at 150, 2 markets (SOL
        // and one more); an isolated subaccount: 4.00 SOL long at 160.
        let main = reading(150, 1_500_000, 900_000_000, 3_000_000, 2);
        let isolated = reading(400, 1_600_000, 100_000_000, -1_000_000, 1);
        let both = [main.clone(), isolated];

        let p = position(&sol, 0, &both);
        assert_eq!(p.quantity, dec!(5.5));
        assert_eq!(p.entry_notional, dec!(865));
        assert_eq!(quote_lots(&both, false), 1_002_000_000);
        assert_eq!(quote_lots(&both, true), 1_001_999_990);
        assert_eq!(outside_all(markets, &both, &universe), 1);
        // Outside the universe, every account's SOL counts too.
        assert_eq!(outside_all(markets, &both, &[]), 3);

        // A short leg nets against the long one.
        let short = reading(-400, 1_600_000, 100_000_000, 0, 1);
        let p = position(&sol, 0, &[main.clone(), short]);
        assert_eq!(p.quantity, dec!(-2.5));
        assert_eq!(p.entry_notional, dec!(415));

        // One account alone is that account, as Phoenix reports it.
        let p = position(&sol, 0, std::slice::from_ref(&main));
        assert_eq!(p, sol.position(150, 1_500_000));
        assert_eq!(outside(markets, &main, &universe), 1);
        // An isolated subaccount in a market outside the universe.
        let elsewhere = reading(0, 0, 100_000_000, 0, 1);
        assert_eq!(
            outside_all(markets, &[reading(0, 0, 1, 0, 0), elsewhere], &universe),
            1
        );
    }

    #[test]
    fn measures_each_market_of_one_account() {
        let sol = sol();
        let mut eth = sol.clone();
        eth.asset_id += 1;
        eth.instrument.id = InstrumentId::new("phoenix:perp:ETH");
        let markets = [sol.clone(), eth.clone()];
        let both = [sol.instrument.id.clone(), eth.instrument.id.clone()];
        // One account: long 1.50 SOL, short 0.20 ETH, and a third market:
        // three markets held, the margin read once for them all.
        let account = Reading {
            assets: vec![asset(150, 1_500_000), asset(-20, 3_000_000)],
            margin: ViewMarginReturn {
                position_count: 3,
                ..ViewMarginReturn::default()
            },
        };
        let readings = std::slice::from_ref(&account);
        assert_eq!(position(&sol, 0, readings), sol.position(150, 1_500_000));
        assert_eq!(position(&eth, 1, readings), eth.position(-20, 3_000_000));
        // The third market is the only one outside the universe.
        assert_eq!(outside(&markets, &account, &both), 1);
        // A measured market outside the universe counts as outside.
        assert_eq!(outside(&markets, &account, &both[..1]), 2);
        // Measuring one of them is the one-market meter: the other two count.
        let one = Reading {
            assets: vec![asset(150, 1_500_000)],
            ..account.clone()
        };
        assert_eq!(outside(&markets[..1], &one, &both), 2);
    }

    fn exchange(snapshot: &str) -> Arc<PhoenixExchange> {
        Arc::new(
            PhoenixExchange::load(std::path::Path::new(&format!(
                "{}/../../snapshots/{snapshot}/exchange.json",
                env!("CARGO_MANIFEST_DIR")
            )))
            .unwrap(),
        )
    }

    #[test]
    fn a_meter_on_one_market_is_the_one_market_meter() {
        let exchange = exchange("phoenix-perps-2026-11-13");
        let chain: Arc<dyn ChainClient> = Arc::new(MockChainClient::new());
        let owner = Pubkey::new_unique();
        let url = "http://127.0.0.1:1";
        let meter = |symbols: &[&str]| {
            PhoenixEternalMeter::for_markets(exchange.clone(), url, chain.clone(), owner, symbols)
        };
        let one =
            PhoenixEternalMeter::new(exchange.clone(), url, chain.clone(), owner, "NVDA").unwrap();
        let same = meter(&["NVDA"]).unwrap();
        assert_eq!(one.markets, same.markets);
        assert_eq!(one.trader(), same.trader());
        // Any iterable of names: owned strings too.
        let two = PhoenixEternalMeter::for_markets(
            exchange.clone(),
            url,
            chain.clone(),
            owner,
            vec!["SPCX".to_string(), "NVDA".to_string()],
        )
        .unwrap();
        let ids: Vec<String> = two.instruments().map(ToString::to_string).collect();
        assert_eq!(ids, ["phoenix:perp:SPCX", "phoenix:perp:NVDA"]);
        assert!(matches!(meter(&[]), Err(PhoenixError::NoMarkets)));
        assert!(matches!(
            meter(&["NVDA", "NOPE"]),
            Err(PhoenixError::UnknownMarket(symbol)) if symbol == "NOPE"
        ));
        // A market measured twice would count its position twice.
        assert!(matches!(
            meter(&["NVDA", "SPCX", "NVDA"]),
            Err(PhoenixError::DuplicateMarket(symbol)) if symbol == "NVDA"
        ));
    }

    #[test]
    fn markets_whose_quote_lots_differ_are_refused() {
        let usdc = |decimals| {
            Asset::new(
                "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
                "USDC",
                decimals,
            )
            .unwrap()
        };
        let market = |symbol, asset_id, decimals| {
            PhoenixMarket::from_parts(symbol, asset_id, 100, 2, usdc(decimals), FeeSchedule::ZERO)
                .unwrap()
        };
        let (sol, eth) = (market("SOL", 7, 6), market("ETH", 8, 6));
        assert_eq!(
            measured(vec![sol.clone(), eth.clone()]).unwrap(),
            [sol.clone(), eth]
        );
        let mixed = measured(vec![sol.clone(), market("BTC", 9, 4)]);
        assert!(
            matches!(
                &mixed,
                Err(PhoenixError::MixedQuoteLots { first, other })
                    if first == "SOL" && other == "BTC"
            ),
            "{mixed:?}"
        );
        // The same asset under two names is one market given twice.
        assert!(matches!(
            measured(vec![sol.clone(), market("SOL2", 7, 6)]),
            Err(PhoenixError::DuplicateMarket(symbol)) if symbol == "SOL2"
        ));
    }

    /// A transaction at `slot` whose instruction logged `events`, already
    /// encoded.
    fn logged_encoded(
        slot: u64,
        signature: u8,
        encoded: &[Vec<u8>],
        failed: bool,
    ) -> TransactionSummary {
        let [lengths, log] = log_encoded(0, encoded);
        TransactionSummary {
            signature: solana_transaction::Signature::from([signature; 64]),
            slot,
            block_time: chrono::DateTime::from_timestamp(1_700_000_000 + slot as i64, 0),
            fee_lamports: 5_000,
            error: failed.then_some(solana_transaction::TransactionError::AccountInUse),
            logs: Vec::new(),
            account_keys: Vec::new(),
            pre_balances: Vec::new(),
            post_balances: Vec::new(),
            token_balance_changes: Vec::new(),
            inner_instructions: [lengths, log]
                .into_iter()
                .map(|data| InnerInstruction {
                    outer: 0,
                    program: *PHOENIX_PROGRAM_ID,
                    data,
                })
                .collect(),
        }
    }

    /// A transaction at `slot` whose instruction logged `events`.
    fn logged(
        slot: u64,
        signature: u8,
        events: &[MarketEvent],
        failed: bool,
    ) -> TransactionSummary {
        let encoded: Vec<Vec<u8>> = events
            .iter()
            .map(|e| borsh::to_vec(e).expect("events encode"))
            .collect();
        logged_encoded(slot, signature, &encoded, failed)
    }

    fn header(market: &PhoenixMarket, sequence: u64, trader: Pubkey) -> MarketEvent {
        MarketEvent::Header(MarketEventHeader {
            sequence_number: sequence,
            prev_sequence_number_slot: 0,
            asset_symbol: Symbol::new(market.symbol()).unwrap(),
            asset_id: market.asset_id,
            tick_size: market.tick_size_quote_lots as u32,
            base_lot_decimals: market.base_lots_decimals,
            quote_lot_decimals: market.quote_lot_decimals,
            signer: [0; 32],
            trader_account: trader.to_bytes(),
        })
    }

    /// A resting order of `maker` filled: `maker_side` is the maker's.
    fn order_filled(maker: Pubkey, maker_side: MathSide, ticks: u64, lots: u64) -> MarketEvent {
        MarketEvent::OrderFilled(OrderFilledEvent {
            order_sequence_number: 1,
            side: maker_side,
            price: Ticks::new(ticks),
            base_lots_filled: BaseLots::new(lots),
            quote_lots_filled: QuoteLots::ZERO,
            quantity_remaining: BaseLots::ZERO,
            maker: maker.to_bytes(),
            maker_fee_rate: SignedFeeRateMicro::ZERO,
            maker_base_lot_position: SignedBaseLots::ZERO,
            maker_virtual_quote_lot_position: SignedQuoteLots::ZERO,
            maker_quote_lot_collateral: SignedQuoteLots::ZERO,
            maker_cumulative_funding_snapshot: SignedQuoteLotsPerBaseLot::ZERO,
        })
    }

    fn spline_filled(maker: Pubkey, maker_side: MathSide, ticks: u64, lots: u64) -> MarketEvent {
        MarketEvent::SplineFilled(SplineFilledEvent {
            spline_sequence_number: 1,
            side: maker_side,
            price: Ticks::new(ticks),
            base_lots_filled: BaseLots::new(lots),
            quote_lots_filled: QuoteLots::ZERO,
            maker: maker.to_bytes(),
            maker_fee_rate: SignedFeeRateMicro::ZERO,
            maker_base_lot_position: SignedBaseLots::ZERO,
            maker_virtual_quote_lot_position: SignedQuoteLots::ZERO,
            maker_quote_lot_collateral: SignedQuoteLots::ZERO,
            maker_cumulative_funding_snapshot: SignedQuoteLotsPerBaseLot::ZERO,
        })
    }

    fn placed(ticks: u64, lots: i64) -> MarketEvent {
        MarketEvent::OrderPlaced(OrderPlacedEvent {
            order_id: 0,
            order_flags: OrderFlags::from_bits(0),
            order_sequence_number: 2,
            prev_order_sequence_number_slot: 0,
            client_order_id: [0; 16],
            price: Ticks::new(ticks),
            quantity: SignedBaseLots::new(lots),
            last_valid_slot: OptionalNonZeroU64::null(),
            initial_slot: 0,
        })
    }

    /// The SPCX market's meter for `owner`, on `chain`, and its order book.
    fn spcx_meter(chain: &Arc<MockChainClient>, owner: Pubkey) -> (PhoenixEternalMeter, Pubkey) {
        let exchange = Arc::new(
            PhoenixExchange::load(std::path::Path::new(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../snapshots/phoenix-spcx-2026-10-29/exchange.json"
            )))
            .unwrap(),
        );
        let book: Pubkey = exchange
            .market_config("SPCX")
            .unwrap()
            .market_pubkey
            .parse()
            .unwrap();
        let meter =
            PhoenixEternalMeter::new(exchange, "http://127.0.0.1:1", chain.clone(), owner, "SPCX")
                .unwrap();
        (meter, book)
    }

    /// `market` as another market: its own asset and instrument.
    fn another(market: &PhoenixMarket) -> PhoenixMarket {
        let mut other = market.clone();
        other.asset_id += 1;
        other.instrument.id = InstrumentId("phoenix:perp:OTHER".into());
        other
    }

    #[tokio::test]
    async fn fills_come_from_the_markets_events_as_taker_and_maker() {
        let chain = Arc::new(MockChainClient::new());
        let owner = Pubkey::new_unique();
        let main = TraderKey::derive_pda(&owner, 0, 0);
        let isolated = TraderKey::derive_pda(&owner, 0, 1);
        for key in [main, isolated] {
            chain.set_account(key, account(PhoenixAccount::Trader, &owner));
        }
        let other = TraderKey::derive_pda(&Pubkey::new_unique(), 0, 0);
        let (meter, book) = spcx_meter(&chain, owner);
        let spcx = meter.markets[0].clone();
        let elsewhere = another(&spcx);

        // The owner buys as the taker, from another trader's resting ask and
        // a spline, and rests the rest of its bid.
        let bought = logged(
            20,
            1,
            &[
                header(&spcx, 10, main),
                order_filled(other, MathSide::Ask, 15_000, 300),
                spline_filled(other, MathSide::Ask, 15_010, 200),
                placed(14_990, 500),
            ],
            false,
        );
        // Another taker sells into a bid resting for the owner's isolated
        // account: the owner buys as the maker. Then it rests an ask.
        let hit = logged(
            30,
            2,
            &[
                header(&spcx, 11, other),
                order_filled(isolated, MathSide::Bid, 14_990, 100),
                header(&spcx, 12, isolated),
                placed(15_100, -50),
            ],
            false,
        );
        // A failed transaction's events never happened, and another
        // market's fills aren't this one's.
        let failed = logged(
            31,
            3,
            &[
                header(&spcx, 13, main),
                order_filled(other, MathSide::Bid, 1, 1),
            ],
            true,
        );
        let other_market = logged(
            32,
            4,
            &[
                header(&elsewhere, 14, main),
                order_filled(other, MathSide::Bid, 1, 1),
            ],
            false,
        );
        for summary in [hit, bought, failed, other_market] {
            chain.record_transaction(summary, &[book]);
        }

        let all = meter.market_records(0).await.unwrap();
        assert_eq!(all.fills.len(), 3);
        assert_eq!(all.fills[0].taker_side, Side::Buy);
        assert_eq!(all.fills[1].price, spcx.ticks_to_price(15_010));
        assert_eq!(all.fills[2].maker, isolated);

        let mine = meter.records(0).await.unwrap();
        let fills: Vec<(u64, Side, Decimal, Decimal)> = mine
            .fills
            .iter()
            .map(|f| (f.slot, f.fill.side, f.fill.price, f.fill.quantity))
            .collect();
        let price = |ticks: u64| spcx.ticks_to_price(ticks);
        let lots = |lots: i64| spcx.base_lots_to_quantity(lots);
        assert_eq!(
            fills,
            vec![
                (20, Side::Buy, price(15_000), lots(300)),
                (20, Side::Buy, price(15_010), lots(200)),
                (30, Side::Buy, price(14_990), lots(100)),
            ]
        );
        let placed: Vec<(Side, Decimal)> =
            mine.placed.iter().map(|p| (p.side, p.quantity)).collect();
        assert_eq!(placed, vec![(Side::Buy, lots(500)), (Side::Sell, lots(50))]);
        assert!(mine.position_changes.is_empty());
        // Re-reading gives the same records, ids and order ids included, and
        // the trait reads the same fills.
        assert_eq!(meter.records(0).await.unwrap(), mine);
        let again = meter.fills(20).await.unwrap();
        assert_eq!(again, mine.fills[2..].to_vec());
        // The other trader's side of the same fills.
        let theirs = all.for_traders(&[other]).fills;
        let sides: Vec<Side> = theirs.iter().map(|f| f.fill.side).collect();
        assert_eq!(sides, vec![Side::Sell, Side::Sell, Side::Sell]);
    }

    /// The NVDA and SPCX markets' meter for `owner`, on `chain`, and their
    /// order books, in that order.
    fn two_markets(
        chain: &Arc<MockChainClient>,
        owner: Pubkey,
    ) -> (PhoenixEternalMeter, [Pubkey; 2]) {
        let exchange = exchange("phoenix-perps-2026-11-13");
        let book = |symbol| -> Pubkey {
            exchange
                .market_config(symbol)
                .unwrap()
                .market_pubkey
                .parse()
                .unwrap()
        };
        let books = [book("NVDA"), book("SPCX")];
        let meter = PhoenixEternalMeter::for_markets(
            exchange,
            "http://127.0.0.1:1",
            chain.clone(),
            owner,
            ["NVDA", "SPCX"],
        )
        .unwrap();
        (meter, books)
    }

    #[test]
    fn fills_on_two_markets_never_share_an_id() {
        // Fills of the same sequence and place on two markets, in one
        // transaction: the id comes from the market, the sequence and the
        // side (`MarketFill::recorded`), so the four ids are apart.
        let chain = Arc::new(MockChainClient::new());
        let (meter, _) = two_markets(&chain, Pubkey::new_unique());
        let (nvda, spcx) = (meter.markets[0].clone(), meter.markets[1].clone());
        assert_eq!(nvda.quote_lot_decimals, spcx.quote_lot_decimals);
        let (taker, maker) = (Pubkey::new_unique(), Pubkey::new_unique());
        let mut records = PerpMarketRecords::default();
        let events = [
            header(&nvda, 7, taker),
            order_filled(maker, MathSide::Ask, 15_000, 1),
            header(&spcx, 7, taker),
            order_filled(maker, MathSide::Ask, 15_000, 1),
        ];
        records
            .add_transaction(&logged(5, 1, &events, false), &meter.markets)
            .unwrap();
        // Event places differ in one transaction; give SPCX's fill the same
        // place as NVDA's, as another transaction would.
        let mut same_place = records.fills[1].clone();
        same_place.sequence = records.fills[0].sequence;
        let both = [records.fills[0].clone(), same_place];
        let mut ids = std::collections::HashSet::new();
        for fill in &both {
            for side in [Side::Buy, Side::Sell] {
                assert!(ids.insert(fill.recorded(side).fill.id));
            }
        }
        // And reading again gives the same ids.
        assert!(ids.contains(&both[1].recorded(Side::Sell).fill.id));
        let instruments: Vec<String> = records
            .for_traders(&[taker])
            .fills
            .iter()
            .map(|f| f.fill.instrument.to_string())
            .collect();
        assert_eq!(instruments, ["phoenix:perp:NVDA", "phoenix:perp:SPCX"]);
        // The same orders' ids differ by market too.
        assert_ne!(
            both[0].recorded(Side::Sell).fill.order_id,
            both[1].recorded(Side::Sell).fill.order_id
        );
    }

    #[tokio::test]
    async fn a_transaction_on_two_markets_is_fetched_and_decoded_once() {
        let chain = Arc::new(MockChainClient::new());
        let owner = Pubkey::new_unique();
        let main = TraderKey::derive_pda(&owner, 0, 0);
        chain.set_account(main, account(PhoenixAccount::Trader, &owner));
        let (meter, books) = two_markets(&chain, owner);
        let (nvda, spcx) = (meter.markets[0].clone(), meter.markets[1].clone());
        let maker = Pubkey::new_unique();
        // One transaction trades both markets: it names both books and the
        // owner's account.
        chain.record_transaction(
            logged(
                10,
                1,
                &[
                    header(&nvda, 40, main),
                    order_filled(maker, MathSide::Ask, 15_000, 300),
                    header(&spcx, 9, main),
                    order_filled(maker, MathSide::Bid, 20_000, 200),
                ],
                false,
            ),
            &[books[0], books[1], main],
        );
        let before = chain.receipt_requests();
        let mine = meter.records(0).await.unwrap();
        assert_eq!(chain.receipt_requests() - before, 1);
        let fills: Vec<(String, Side, Decimal)> = mine
            .fills
            .iter()
            .map(|f| (f.fill.instrument.to_string(), f.fill.side, f.fill.quantity))
            .collect();
        assert_eq!(
            fills,
            vec![
                (
                    "phoenix:perp:NVDA".into(),
                    Side::Buy,
                    nvda.base_lots_to_quantity(300)
                ),
                (
                    "phoenix:perp:SPCX".into(),
                    Side::Sell,
                    spcx.base_lots_to_quantity(200)
                ),
            ]
        );
        let before = chain.receipt_requests();
        assert_eq!(meter.market_records(0).await.unwrap().fills.len(), 2);
        assert_eq!(chain.receipt_requests() - before, 1);
    }

    #[tokio::test]
    async fn fills_on_several_markets_come_in_the_order_they_landed() {
        let chain = Arc::new(MockChainClient::new());
        let owner = Pubkey::new_unique();
        let main = TraderKey::derive_pda(&owner, 0, 0);
        chain.set_account(main, account(PhoenixAccount::Trader, &owner));
        let (meter, books) = two_markets(&chain, owner);
        let (nvda, spcx) = (meter.markets[0].clone(), meter.markets[1].clone());
        let maker = Pubkey::new_unique();
        let buy = |market: &PhoenixMarket, sequence, ticks| {
            [
                header(market, sequence, main),
                order_filled(maker, MathSide::Ask, ticks, 1),
            ]
        };
        // In one slot: SPCX (its sequence 900), NVDA (its sequence 5), then
        // a transaction on both; a slot later, NVDA again. Each names its
        // books and the owner's account, whose list has them in the order
        // they landed. Sequence numbers are each market's own: sorting by
        // them would put NVDA's first.
        chain.record_transaction(logged(10, 1, &buy(&spcx, 900, 1), false), &[books[1], main]);
        chain.record_transaction(logged(10, 2, &buy(&nvda, 5, 2), false), &[books[0], main]);
        chain.record_transaction(
            logged(
                10,
                3,
                &[buy(&nvda, 6, 3), buy(&spcx, 901, 4)].concat(),
                false,
            ),
            &[books[0], books[1], main],
        );
        chain.record_transaction(logged(11, 4, &buy(&nvda, 7, 5), false), &[books[0], main]);
        let order: Vec<Decimal> = meter
            .fills(0)
            .await
            .unwrap()
            .iter()
            .map(|f| f.fill.price)
            .collect();
        let at = |market: &PhoenixMarket, ticks| market.ticks_to_price(ticks);
        assert_eq!(
            order,
            vec![
                at(&spcx, 1),
                at(&nvda, 2),
                at(&nvda, 3),
                at(&spcx, 4),
                at(&nvda, 5)
            ]
        );
    }

    #[test]
    fn transactions_merge_in_the_order_each_list_saw_them() {
        let s = |i: u8| Signature::from([i; 64]);
        // Two books' lists in one slot: a and c on one, b and c on the
        // other. c comes after both a and b.
        let merged = landed_order(vec![
            vec![(s(1), 10), (s(3), 10)],
            vec![(s(2), 10), (s(3), 10), (s(4), 12)],
            vec![(s(5), 11)],
        ]);
        assert_eq!(merged, [s(1), s(2), s(3), s(5), s(4)]);
        // Lists that disagree still give every transaction once.
        let merged = landed_order(vec![vec![(s(1), 1), (s(2), 1)], vec![(s(2), 1), (s(1), 1)]]);
        assert_eq!(merged.len(), 2);
        assert!(landed_order(vec![Vec::new()]).is_empty());
    }

    #[test]
    fn an_event_on_a_market_names_it_when_it_does_not_decode() {
        let chain = Arc::new(MockChainClient::new());
        let (meter, _) = two_markets(&chain, Pubkey::new_unique());
        let spcx = meter.markets[1].clone();
        let (taker, maker) = (Pubkey::new_unique(), Pubkey::new_unique());
        let mut timeless = logged(
            8,
            4,
            &[
                header(&spcx, 4, taker),
                order_filled(maker, MathSide::Ask, 1, 1),
            ],
            false,
        );
        timeless.block_time = None;
        let mut records = PerpMarketRecords::default();
        let read = records.add_transaction(&timeless, &meter.markets);
        assert!(
            matches!(&read, Err(MeterError::Decode { detail, .. }) if detail.contains("phoenix:perp:SPCX")),
            "{read:?}"
        );
        assert_eq!(records, PerpMarketRecords::default());
    }

    #[test]
    fn an_event_that_does_not_decode_is_an_error() {
        let chain = Arc::new(MockChainClient::new());
        let (meter, _) = spcx_meter(&chain, Pubkey::new_unique());
        let a = meter.markets[0].clone();
        let b = another(&a);
        let (taker, maker) = (Pubkey::new_unique(), Pubkey::new_unique());
        let encode = |e: &MarketEvent| borsh::to_vec(e).unwrap();
        let corrupt = |mut bytes: Vec<u8>| {
            bytes[0] = 0xff;
            bytes
        };

        // A corrupted header alone.
        let lone = logged_encoded(5, 1, &[corrupt(encode(&header(&a, 1, taker)))], false);
        let mut records = PerpMarketRecords::default();
        let read = records.add_transaction(&lone, std::slice::from_ref(&a));
        assert!(matches!(read, Err(MeterError::Decode { .. })), "{read:?}");
        assert_eq!(records, PerpMarketRecords::default());

        // A's header, then B's, corrupted, then B's fill: never A's fill.
        let mixed = logged_encoded(
            6,
            2,
            &[
                encode(&header(&a, 2, taker)),
                corrupt(encode(&header(&b, 3, taker))),
                encode(&order_filled(maker, MathSide::Ask, 15_000, 4)),
            ],
            false,
        );
        let read = records.add_transaction(&mixed, std::slice::from_ref(&a));
        assert!(matches!(read, Err(MeterError::Decode { .. })), "{read:?}");
        assert!(records.fills.is_empty());

        // A fill with no header to name its market, or no time.
        let headless = logged(7, 3, &[order_filled(maker, MathSide::Ask, 1, 1)], false);
        assert!(
            records
                .add_transaction(&headless, std::slice::from_ref(&a))
                .is_err()
        );
        let mut timeless = logged(
            8,
            4,
            &[
                header(&a, 4, taker),
                order_filled(maker, MathSide::Ask, 1, 1),
            ],
            false,
        );
        timeless.block_time = None;
        assert!(
            records
                .add_transaction(&timeless, std::slice::from_ref(&a))
                .is_err()
        );
        // A slot context gives it one.
        let mut timed = logged(
            9,
            5,
            &[
                MarketEvent::SlotContext(SlotContextEvent {
                    timestamp: 1_700_000_123,
                    slot: 9,
                }),
                header(&a, 5, taker),
                order_filled(maker, MathSide::Ask, 1, 1),
            ],
            false,
        );
        timed.block_time = None;
        records
            .add_transaction(&timed, std::slice::from_ref(&a))
            .unwrap();
        assert_eq!(records.fills[0].unix_timestamp, 1_700_000_123);
    }

    #[test]
    fn a_transaction_added_twice_counts_once() {
        let chain = Arc::new(MockChainClient::new());
        let (meter, _) = spcx_meter(&chain, Pubkey::new_unique());
        let a = meter.markets[0].clone();
        let (taker, maker) = (Pubkey::new_unique(), Pubkey::new_unique());
        let tx = logged(
            5,
            1,
            &[
                header(&a, 1, taker),
                order_filled(maker, MathSide::Ask, 1, 1),
            ],
            false,
        );
        let mut records = PerpMarketRecords::default();
        records
            .add_transaction(&tx, std::slice::from_ref(&a))
            .unwrap();
        records
            .add_transaction(&tx, std::slice::from_ref(&a))
            .unwrap();
        assert_eq!(records.fills.len(), 1);
        assert!(records.contains(&tx.signature));
    }

    #[tokio::test]
    async fn liquidation_transfers_and_deleveraging_are_position_changes() {
        let chain = Arc::new(MockChainClient::new());
        let owner = Pubkey::new_unique();
        let main = TraderKey::derive_pda(&owner, 0, 0);
        chain.set_account(main, account(PhoenixAccount::Trader, &owner));
        let (keeper, other, maker) = (
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        );
        let (meter, book) = spcx_meter(&chain, owner);
        let spcx = meter.markets[0].clone();
        let asset_id = u64::from(spcx.asset_id);
        let lots = |lots: i64| spcx.base_lots_to_quantity(lots);

        // The owner buys 900 lots.
        chain.record_transaction(
            logged(
                10,
                1,
                &[
                    header(&spcx, 1, main),
                    order_filled(maker, MathSide::Ask, 15_000, 900),
                ],
                false,
            ),
            &[book],
        );
        // A keeper takes 600 of the long into its own account: the
        // transaction names the trader accounts, not the order book, and
        // Phoenix logs it under no header.
        let transfer = MarketEvent::LiquidationTransfer(LiquidationTransferEvent {
            liquidatee: main.to_bytes(),
            liquidator: keeper.to_bytes(),
            asset_id,
            base_lots_transferred: SignedBaseLots::new(600),
            virtual_quote_lots_transferred: SignedQuoteLots::ZERO,
            haircut_rate: 19,
            liquidatee_collateral_change: SignedQuoteLots::ZERO,
            liquidator_collateral_change: SignedQuoteLots::ZERO,
        });
        // Auto-deleveraging closes 100 of the rest against a short.
        let adl = MarketEvent::CloseMatchedPositions(CloseMatchedPositionsEvent {
            caller: keeper.to_bytes(),
            closed_short: other.to_bytes(),
            closed_long: main.to_bytes(),
            in_profit_account: other.to_bytes(),
            asset_id,
            base_lots_closed: SignedBaseLots::new(-100),
            at_loss_close_value: SignedQuoteLots::ZERO,
            in_profit_close_value: SignedQuoteLots::ZERO,
            at_loss_collateral_change: SignedQuoteLots::ZERO,
            in_profit_collateral_change: SignedQuoteLots::ZERO,
        });
        // Another market's transfer isn't this one's.
        let mut elsewhere = transfer;
        if let MarketEvent::LiquidationTransfer(t) = &mut elsewhere {
            t.asset_id += 1;
        }
        chain.record_transaction(logged(20, 2, &[transfer, adl, elsewhere], false), &[main]);

        let mine = meter.records(0).await.unwrap();
        let changes: Vec<(PositionChangeKind, Side, Decimal, Pubkey)> = mine
            .position_changes
            .iter()
            .map(|c| (c.kind, c.side, c.quantity, c.counterparty))
            .collect();
        assert_eq!(
            changes,
            vec![
                (
                    PositionChangeKind::LiquidationTransfer,
                    Side::Sell,
                    lots(600),
                    keeper
                ),
                (
                    PositionChangeKind::AutoDeleverage,
                    Side::Sell,
                    lots(100),
                    other
                ),
            ]
        );
        // Its fills and its position changes sum to its position.
        let filled: Decimal = mine
            .fills
            .iter()
            .map(|f| f.fill.side.sign() * f.fill.quantity)
            .sum();
        let moved: Decimal = mine
            .position_changes
            .iter()
            .map(PerpPositionChange::signed_quantity)
            .sum();
        assert_eq!(filled + moved, lots(200));
        // The market's own records don't see the transfer: it names no
        // order book.
        assert!(
            meter
                .market_records(0)
                .await
                .unwrap()
                .position_changes
                .is_empty()
        );
    }

    #[tokio::test]
    async fn liquidation_and_deleveraging_count_on_their_own_market() {
        let chain = Arc::new(MockChainClient::new());
        let owner = Pubkey::new_unique();
        let main = TraderKey::derive_pda(&owner, 0, 0);
        chain.set_account(main, account(PhoenixAccount::Trader, &owner));
        let (keeper, other, maker) = (
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        );
        let (meter, books) = two_markets(&chain, owner);
        let (nvda, spcx) = (meter.markets[0].clone(), meter.markets[1].clone());
        // The owner buys 900 lots of NVDA and sells 500 of SPCX.
        chain.record_transaction(
            logged(
                10,
                1,
                &[
                    header(&nvda, 1, main),
                    order_filled(maker, MathSide::Ask, 15_000, 900),
                    header(&spcx, 1, main),
                    order_filled(maker, MathSide::Bid, 20_000, 500),
                ],
                false,
            ),
            &books,
        );
        // A keeper takes 600 of the NVDA long; auto-deleveraging closes 200
        // of the SPCX short against a long.
        let transfer = MarketEvent::LiquidationTransfer(LiquidationTransferEvent {
            liquidatee: main.to_bytes(),
            liquidator: keeper.to_bytes(),
            asset_id: u64::from(nvda.asset_id),
            base_lots_transferred: SignedBaseLots::new(600),
            virtual_quote_lots_transferred: SignedQuoteLots::ZERO,
            haircut_rate: 19,
            liquidatee_collateral_change: SignedQuoteLots::ZERO,
            liquidator_collateral_change: SignedQuoteLots::ZERO,
        });
        let adl = MarketEvent::CloseMatchedPositions(CloseMatchedPositionsEvent {
            caller: keeper.to_bytes(),
            closed_short: main.to_bytes(),
            closed_long: other.to_bytes(),
            in_profit_account: other.to_bytes(),
            asset_id: u64::from(spcx.asset_id),
            base_lots_closed: SignedBaseLots::new(200),
            at_loss_close_value: SignedQuoteLots::ZERO,
            in_profit_close_value: SignedQuoteLots::ZERO,
            at_loss_collateral_change: SignedQuoteLots::ZERO,
            in_profit_collateral_change: SignedQuoteLots::ZERO,
        });
        chain.record_transaction(logged(20, 2, &[transfer, adl], false), &[main]);

        let mine = meter.records(0).await.unwrap();
        // On each market, the fills and the position changes sum to the
        // position: NVDA 900 - 600, SPCX -500 + 200.
        let net = |market: &PhoenixMarket| -> Decimal {
            let filled: Decimal = mine
                .fills
                .iter()
                .filter(|f| f.fill.instrument == *market.id())
                .map(|f| f.fill.side.sign() * f.fill.quantity)
                .sum();
            let moved: Decimal = mine
                .position_changes
                .iter()
                .filter(|c| c.instrument == *market.id())
                .map(PerpPositionChange::signed_quantity)
                .sum();
            filled + moved
        };
        assert_eq!(net(&nvda), nvda.base_lots_to_quantity(300));
        assert_eq!(net(&spcx), spcx.base_lots_to_quantity(-300));
        let kinds: Vec<(PositionChangeKind, String)> = mine
            .position_changes
            .iter()
            .map(|c| (c.kind, c.instrument.to_string()))
            .collect();
        assert_eq!(
            kinds,
            [
                (
                    PositionChangeKind::LiquidationTransfer,
                    "phoenix:perp:NVDA".into()
                ),
                (
                    PositionChangeKind::AutoDeleverage,
                    "phoenix:perp:SPCX".into()
                ),
            ]
        );
    }

    #[test]
    fn a_match_between_two_of_the_owners_accounts_is_both_sides() {
        let chain = Arc::new(MockChainClient::new());
        let (meter, _) = spcx_meter(&chain, Pubkey::new_unique());
        let a = meter.markets[0].clone();
        let (main, isolated) = (Pubkey::new_unique(), Pubkey::new_unique());
        let mut records = PerpMarketRecords::default();
        records
            .add_transaction(
                &logged(
                    5,
                    1,
                    &[
                        header(&a, 1, main),
                        order_filled(isolated, MathSide::Ask, 15_000, 40),
                    ],
                    false,
                ),
                std::slice::from_ref(&a),
            )
            .unwrap();
        let own = records.for_traders(&[main, isolated]).fills;
        let sides: Vec<Side> = own.iter().map(|f| f.fill.side).collect();
        assert_eq!(sides, vec![Side::Buy, Side::Sell]);
        assert_ne!(own[0].fill.id, own[1].fill.id);
        let net: Decimal = own
            .iter()
            .map(|f| f.fill.side.sign() * f.fill.quantity)
            .sum();
        assert_eq!(net, Decimal::ZERO);
    }

    /// The run of fills a trade summary closes was taken by its trader.
    fn summary_of(taker: Pubkey, taker_side: MathSide, lots: u64, fee_lots: u64) -> MarketEvent {
        MarketEvent::TradeSummary(TradeSummaryEvent {
            trader: taker.to_bytes(),
            trade_sequence_number: 1,
            prev_trade_sequence_number_slot: 0,
            side: taker_side,
            base_lots_filled: BaseLots::new(lots),
            quote_lots_filled: QuoteLots::ZERO,
            fee_in_quote_lots: QuoteLots::new(fee_lots),
            base_lot_position: SignedBaseLots::ZERO,
            virtual_quote_lot_position: SignedQuoteLots::ZERO,
            quote_lot_collateral: SignedQuoteLots::ZERO,
            cumulative_funding_snapshot: SignedQuoteLotsPerBaseLot::ZERO,
        })
    }

    /// `event`, a fill, for `quote_lots` at the maker's `rate` (millionths).
    fn priced(mut event: MarketEvent, quote_lots: u64, rate: i32) -> MarketEvent {
        match &mut event {
            MarketEvent::OrderFilled(f) => {
                f.quote_lots_filled = QuoteLots::new(quote_lots);
                f.maker_fee_rate = SignedFeeRateMicro::new(rate);
            }
            MarketEvent::SplineFilled(f) => {
                f.quote_lots_filled = QuoteLots::new(quote_lots);
                f.maker_fee_rate = SignedFeeRateMicro::new(rate);
            }
            _ => unreachable!("a fill"),
        }
        event
    }

    #[tokio::test]
    async fn a_cross_match_inside_the_owners_instruction_is_not_the_owners() {
        let chain = Arc::new(MockChainClient::new());
        let owner = Pubkey::new_unique();
        let main = TraderKey::derive_pda(&owner, 0, 0);
        chain.set_account(main, account(PhoenixAccount::Trader, &owner));
        let (spline, stale, maker) = (
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        );
        let (meter, book) = spcx_meter(&chain, owner);
        let spcx = meter.markets[0].clone();
        // The owner's instruction (a cancel, then an order): first a
        // re-centred spline crosses another trader's stale bid, which
        // Phoenix logs under the owner's header and closes with a summary
        // naming that trader; then the owner buys from a resting ask (its
        // maker pays 250 millionths) and a spline (a 50 millionths rebate),
        // and pays 4,200 quote lots of fee, split by quote lots.
        let events = [
            header(&spcx, 20, main),
            priced(
                spline_filled(spline, MathSide::Ask, 14_000, 300),
                4_200_000,
                0,
            ),
            summary_of(stale, MathSide::Bid, 300, 1_000),
            priced(
                order_filled(maker, MathSide::Ask, 15_000, 100),
                1_500_000,
                250,
            ),
            priced(
                spline_filled(spline, MathSide::Ask, 15_000, 200),
                3_000_001,
                -50,
            ),
            summary_of(main, MathSide::Bid, 300, 4_200),
        ];
        chain.record_transaction(logged(40, 9, &events, false), &[book]);

        let all = meter.market_records(0).await.unwrap();
        let takers: Vec<Pubkey> = all.fills.iter().map(|f| f.taker).collect();
        assert_eq!(takers, vec![stale, main, main]);
        let fee = |lots: i64| Decimal::new(lots, u32::from(spcx.quote_lot_decimals));
        assert_eq!(all.fills[0].taker_fee, fee(1_000));
        // 4,200 x 1,500,000 / 4,500,001 rounds down to 1,399; the last
        // fill takes the remainder.
        assert_eq!(all.fills[1].taker_fee, fee(1_399));
        assert_eq!(all.fills[2].taker_fee, fee(2_801));
        // A maker's fee at its rate, and a rebate rounded toward zero
        // (-150.00005 lots), as Phoenix charges them.
        assert_eq!(all.fills[1].maker_fee, fee(375));
        assert_eq!(all.fills[2].maker_fee, fee(-150));
        assert_eq!(all.fills[0].maker_fee, Decimal::ZERO);
        let makers = all.for_traders(&[maker]).fills;
        assert_eq!(makers.len(), 1);
        assert_eq!(makers[0].fill.fees, fee(375));

        // The owner's fills are its own buys, with its fees; the cross-match
        // isn't one, and its position change is what they add up to.
        let mine = meter.records(0).await.unwrap().fills;
        let bought: Decimal = mine.iter().map(|f| f.fill.quantity).sum();
        assert_eq!(mine.len(), 2);
        assert!(mine.iter().all(|f| f.fill.side == Side::Buy));
        assert_eq!(bought, spcx.base_lots_to_quantity(300));
        assert_eq!(mine[0].fill.fees + mine[1].fill.fees, fee(4_200));
        // The stale order's owner took its side of the cross.
        let theirs = all.for_traders(&[stale]).fills;
        assert_eq!(theirs.len(), 1);
        assert_eq!(theirs[0].fill.side, Side::Buy);
        assert_eq!(theirs[0].fill.fees, fee(1_000));
    }
}
