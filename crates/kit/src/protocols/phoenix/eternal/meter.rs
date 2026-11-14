//! [`PhoenixEternalMeter`]: what an enforcer measures of an owner's trader
//! accounts on Phoenix Eternal, as the Phoenix venue profile (Perpetual
//! markets) defines it.
//!
//! Everything is read from Phoenix through Hawkeye's views, and the wallet,
//! the owner's trader accounts and the market's fills from the chain,
//! through the meter's own clients, never the bot's.

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

    /// Every fill and resting order the market recorded in slots after
    /// `after_slot`, between any trader accounts, oldest first: the events
    /// of every landed transaction that named the market's order book.
    pub async fn market_records(&self, after_slot: u64) -> Result<PerpMarketRecords, MeterError> {
        let book = self
            .exchange
            .market_config(self.market.symbol())
            .map_err(read)?
            .market_pubkey
            .parse::<Pubkey>()
            .map_err(|e| MeterError::Decode {
                what: "market address",
                detail: e.to_string(),
            })?;
        let chain = self.chain.as_ref();
        let mut records = PerpMarketRecords::default();
        for (signature, _) in chain
            .signatures_for_address(&book, after_slot)
            .await
            .map_err(read)?
        {
            let summary = chain
                .get_transaction(&signature)
                .await
                .map_err(read)?
                .ok_or_else(|| {
                    MeterError::Read(format!("transaction {signature} is listed but not found"))
                })?;
            records.add_transaction(&summary, &self.market);
        }
        records.sort();
        Ok(records)
    }

    /// What the owner's trader accounts did on the market in slots after
    /// `after_slot`: their fills, as a taker or a maker, and the orders
    /// they left resting.
    pub async fn records(&self, after_slot: u64) -> Result<TraderRecords, MeterError> {
        let traders = traders(self.chain.as_ref(), &self.owner, self.trader).await?;
        let records = self.market_records(after_slot).await?;
        Ok(TraderRecords {
            fills: records.fills_for(&traders),
            placed: records.placed_by(&traders),
        })
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

    /// The owner's fills, as a taker or a maker, from Phoenix's fill
    /// events (see [`PhoenixEternalMeter::records`]).
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
    let mut legs = readings.iter().map(|r| {
        market.position(
            r.asset.base_lots,
            r.asset.entry_price_quote_lots_per_base_lot,
        )
    });
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
    use phoenix_rise::{
        api::TraderKey,
        events::market_events::{
            MarketEvent, MarketEventHeader, OptionalNonZeroU64, OrderFilledEvent, OrderFlags,
            OrderPlacedEvent, SplineFilledEvent, Symbol, TradeSummaryEvent,
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
        protocols::phoenix::eternal::fills::testing::log_instructions,
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
        assert_eq!(
            outside(&sol, &[reading(0, 0, 1, 0, 0), elsewhere], &universe),
            1
        );
    }

    /// A transaction at `slot` whose instruction logged `events`.
    fn logged(
        slot: u64,
        signature: u8,
        events: &[MarketEvent],
        failed: bool,
    ) -> TransactionSummary {
        let [lengths, log] = log_instructions(0, events);
        TransactionSummary {
            signature: solana_transaction::Signature::from([signature; 64]),
            slot,
            block_time: None,
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

    #[tokio::test]
    async fn fills_come_from_the_markets_events_as_taker_and_maker() {
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
        let chain = Arc::new(MockChainClient::new());
        let owner = Pubkey::new_unique();
        let main = TraderKey::derive_pda(&owner, 0, 0);
        let isolated = TraderKey::derive_pda(&owner, 0, 1);
        for key in [main, isolated] {
            chain.set_account(key, account(PhoenixAccount::Trader, &owner));
        }
        let other = TraderKey::derive_pda(&Pubkey::new_unique(), 0, 0);
        let meter =
            PhoenixEternalMeter::new(exchange, "http://127.0.0.1:1", chain.clone(), owner, "SPCX")
                .unwrap();
        let spcx = meter.market.clone();
        let mut elsewhere = spcx.clone();
        elsewhere.asset_id += 1;

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
        // The same fill read again has the same id, and the trait reads the
        // same fills.
        let again = meter.fills(20).await.unwrap();
        assert_eq!(again.len(), 1);
        assert_eq!(again[0].fill.id, mine.fills[2].fill.id);
        // The other trader's side of the same fills.
        let theirs = meter.market_records(0).await.unwrap().fills_for(&[other]);
        let sides: Vec<Side> = theirs.iter().map(|f| f.fill.side).collect();
        assert_eq!(sides, vec![Side::Sell, Side::Sell, Side::Sell]);
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

    #[tokio::test]
    async fn a_cross_match_inside_the_owners_instruction_is_not_the_owners() {
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
        let chain = Arc::new(MockChainClient::new());
        let owner = Pubkey::new_unique();
        let main = TraderKey::derive_pda(&owner, 0, 0);
        chain.set_account(main, account(PhoenixAccount::Trader, &owner));
        let (spline, stale, maker) = (
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        );
        let meter =
            PhoenixEternalMeter::new(exchange, "http://127.0.0.1:1", chain.clone(), owner, "SPCX")
                .unwrap();
        let spcx = meter.market.clone();
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
        // A maker's fee at its rate, and a rebate rounded down (-150.00005
        // lots), as Phoenix charges them.
        assert_eq!(all.fills[1].maker_fee, fee(375));
        assert_eq!(all.fills[2].maker_fee, fee(-150));
        assert_eq!(all.fills[0].maker_fee, Decimal::ZERO);
        let makers = all.fills_for(&[maker]);
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
        let theirs = all.fills_for(&[stale]);
        assert_eq!(theirs.len(), 1);
        assert_eq!(theirs[0].fill.side, Side::Buy);
        assert_eq!(theirs[0].fill.fees, fee(1_000));
    }
}
