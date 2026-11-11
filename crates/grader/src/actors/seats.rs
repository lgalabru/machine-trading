//! The actors' accounts: a funded seat on one venue, trading through the
//! grader's own RPC with the same orders any trader sends.
//!
//! A spot seat is a wallet with wrapped SOL and USDC, a Phoenix v1 seat or
//! an OpenBook v2 open orders account, and the kit's trader for that market.
//! A perp seat is a Phoenix cross-margin account, onboarded as the takers
//! are. Seats are onboarded at setup, while the cheatcodes are unlocked.
//! Spot seats sign from memory; a perp seat's keypair sits in a directory
//! only root can read, which goes with the last seat.

use std::{path::PathBuf, str::FromStr, sync::Arc};

use mtx_core::{Decimal, Side, instrument::Rounding};
use mtx_kit::{
    BotEnv, Trader,
    chain::{self, ChainClient, ExecutionPlan, InstructionGroup, RpcChainClient, SendMode},
    protocols::{
        openbook::v2::{
            self as openbook_v2, OpenBookV2Order, OpenBookV2Trader,
            ix::{NO_QUOTE_LIMIT, OrderSide, PlaceOrderArgs, PlaceOrderType, SelfTradeBehavior},
        },
        phoenix::v1::{
            self as phoenix_v1, BookSide, PhoenixV1Order, PhoenixV1Trader,
            ix::{OrderPacket, SelfTrade},
        },
    },
};
use phoenix_rise::{
    core::MarketOrderTicket,
    ix::types::{SelfTradeBehavior as PerpSelfTrade, Side as TicketSide},
};
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use surfpool_sdk::Surfnet;

use crate::{
    Error, Result,
    error::io_error,
    phoenix::{self, Phoenix},
    scenario::Scenario,
    snapshot::OfflineWorld,
};

/// What a spot seat starts with: far more than its orders need, so none
/// fails for funds.
const SPOT_BASE: f64 = 50_000.0;
const SPOT_QUOTE: f64 = 5_000_000.0;
/// A perp seat's collateral, for the same reason.
const PERP_DEPOSIT_USDC: f64 = 1_000_000.0;
/// The most heap events one crank takes (OpenBook consumes at most eight
/// an instruction and skips the rest), and the most open orders accounts.
const CRANK_EVENTS: usize = 8;
const CRANK_ACCOUNTS: usize = 8;

/// Resting orders on a spot venue a few to a transaction: Phoenix runs out
/// of compute on twelve post-only orders in one.
pub const ORDERS_PER_TRANSACTION: usize = 4;

/// What a [`Seat::rest`] sent, how many of those failed, and the last error.
#[derive(Debug, Default)]
pub struct Rested {
    pub sent: usize,
    pub failed: usize,
    pub error: Option<Error>,
}

/// One actor's account on one venue.
pub struct Seat {
    /// The venue, as the mandate names it.
    pub venue: String,
    keypair: Keypair,
    chain: Arc<dyn ChainClient>,
    kind: Kind,
    _dir: Option<Arc<SeatDir>>,
}

enum Kind {
    PhoenixV1(Box<PhoenixV1Trader>),
    OpenBookV2(Box<OpenBookV2Trader>),
    Perp(Box<Trader>),
}

/// A directory only root can read, removed with the last seat that uses it.
pub struct SeatDir(PathBuf);

impl SeatDir {
    pub fn create() -> Result<Arc<Self>> {
        let dir = std::env::temp_dir().join(format!("mtx-seats-{}", uuid::Uuid::new_v4()));
        // Bots run in the same container, and these keypairs sign for
        // accounts with funds of their own.
        std::os::unix::fs::DirBuilderExt::mode(std::fs::DirBuilder::new().recursive(true), 0o700)
            .create(&dir)
            .map_err(io_error(&dir))?;
        Ok(Arc::new(Self(dir)))
    }
}

impl Drop for SeatDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn failed<E: std::fmt::Display>(what: &'static str) -> impl Fn(E) -> Error {
    move |error| Error::Transaction {
        label: what.into(),
        reason: error.to_string(),
    }
}

fn decimal(value: f64) -> Decimal {
    Decimal::from_str(&format!("{value}")).unwrap_or_default()
}

fn f64_of(value: Decimal) -> f64 {
    value.to_string().parse().unwrap_or(f64::NAN)
}

/// A bid rounds down onto the grid, an ask up: never better than asked.
fn rounding(side: Side) -> Rounding {
    match side {
        Side::Buy => Rounding::Down,
        Side::Sell => Rounding::Up,
    }
}

impl Seat {
    /// A funded seat on the spot `venue` (`phoenix` or `openbook`), trading
    /// through `rpc_url`, with `deposit` (SOL, USDC) in its wallet: by
    /// default far more than its orders need. Setup only: funding uses
    /// cheatcodes.
    pub async fn spot(
        surfnet: &Surfnet,
        venue: &str,
        rpc_url: &str,
        deposit: Option<(f64, f64)>,
    ) -> Result<Self> {
        let keypair = Keypair::new();
        let (base, quote) = deposit.unwrap_or((SPOT_BASE, SPOT_QUOTE));
        crate::spot::fund(surfnet, &keypair, base, quote)?;
        let chain: Arc<dyn ChainClient> = Arc::new(RpcChainClient::new(rpc_url));
        let owner = keypair.pubkey();
        let kind = match venue {
            "phoenix" => {
                let named = phoenix_v1::MARKETS[0];
                let trader =
                    PhoenixV1Trader::connect(chain.clone(), named.address, "SOL", "USDC", owner)
                        .await
                        .map_err(failed("phoenix v1 market"))?;
                let plan = trader.plan_claim_seat().map_err(failed("claim seat"))?;
                trader
                    .send(&plan, &keypair)
                    .await
                    .map_err(failed("claim seat"))?;
                Kind::PhoenixV1(Box::new(trader))
            }
            _ => {
                let named = openbook_v2::MARKETS[0];
                let trader =
                    OpenBookV2Trader::connect(chain.clone(), named.address, "SOL", "USDC", owner)
                        .await
                        .map_err(failed("openbook v2 market"))?;
                let plan = trader
                    .plan_open_account()
                    .map_err(failed("open orders account"))?;
                trader
                    .send(&plan, &keypair)
                    .await
                    .map_err(failed("open orders account"))?;
                Kind::OpenBookV2(Box::new(trader))
            }
        };
        Ok(Self {
            venue: venue.into(),
            keypair,
            chain,
            kind,
            _dir: None,
        })
    }

    /// A funded cross-margin account on the scenario's Phoenix market,
    /// onboarded as the takers are, its keypair in `dir`.
    pub async fn perp(
        world: &OfflineWorld,
        phoenix: &Phoenix,
        scenario: &Scenario,
        rpc_url: &str,
        dir: &Arc<SeatDir>,
    ) -> Result<Self> {
        let keypair = Keypair::new();
        phoenix::onboard_trader(&world.surfnet, phoenix, &keypair, PERP_DEPOSIT_USDC).await?;
        let path = dir.0.join(format!("seat-{}.json", keypair.pubkey()));
        let bytes = serde_json::to_string(&keypair.to_bytes().to_vec())?;
        std::fs::write(&path, bytes).map_err(io_error(&path))?;
        let trader = Trader::connect(&BotEnv {
            rpc_url: rpc_url.to_string(),
            keypair_path: path,
            exchange_path: world.exchange_path.clone(),
            symbol: scenario.symbol.clone(),
            end_slot: u64::MAX,
            max_position: f64::MAX,
            max_drawdown_usd: f64::MAX,
            mandate_path: None,
        })?;
        Ok(Self {
            venue: crate::scenario::VENUE.into(),
            keypair,
            chain: Arc::new(RpcChainClient::new(rpc_url)),
            kind: Kind::Perp(Box::new(trader)),
            _dir: Some(dir.clone()),
        })
    }

    /// Who the seat is in a spot market's fills: its wallet, and on OpenBook
    /// its open orders account.
    pub fn ids(&self) -> Vec<Pubkey> {
        match &self.kind {
            Kind::OpenBookV2(trader) => vec![self.keypair.pubkey(), trader.open_orders_account()],
            _ => vec![self.keypair.pubkey()],
        }
    }

    /// The market's tick, in USD.
    pub fn tick_usd(&self) -> f64 {
        match &self.kind {
            Kind::PhoenixV1(t) => f64_of(t.market().instrument.tick_size),
            Kind::OpenBookV2(t) => f64_of(t.market().instrument.tick_size),
            Kind::Perp(t) => t
                .phoenix
                .market(&t.env.symbol)
                .map_or(f64::NAN, |m| t.phoenix.ticks_to_usd(m, 1)),
        }
    }

    async fn send(&self, plan: &ExecutionPlan, what: &'static str) -> Result<()> {
        SendMode::default()
            .send(plan, &self.keypair, self.chain.as_ref())
            .await
            .map(|_| ())
            .map_err(failed(what))
    }

    /// Cancel every resting order of the seat's, and take its funds back to
    /// the wallet.
    pub async fn cancel(&self) -> Result<()> {
        match &self.kind {
            Kind::PhoenixV1(t) => {
                let plan = t.plan_cancel_all().map_err(failed("cancel"))?;
                self.send(&plan, "cancel").await
            }
            Kind::OpenBookV2(t) => {
                let plan = t.plan_cancel_all().map_err(failed("cancel"))?;
                self.send(&plan, "cancel").await
            }
            Kind::Perp(t) => t.cancel_all().map(|_| ()).map_err(Error::from),
        }
    }

    /// Every price level resting on `side`, best first: price and size.
    pub async fn levels(&self, side: Side) -> Result<Vec<(f64, f64)>> {
        let book = match &self.kind {
            Kind::PhoenixV1(t) => t.order_book().await.map_err(failed("book"))?,
            Kind::OpenBookV2(t) => t.order_book(0).await.map_err(failed("book"))?,
            Kind::Perp(t) => {
                let bbo = t.inner.bbo().await.map_err(failed("book"))?;
                let market = t.phoenix.market(&t.env.symbol)?;
                let ticks = match side {
                    Side::Buy => bbo.best_bid_ticks(),
                    Side::Sell => bbo.best_ask_ticks(),
                };
                return Ok(ticks
                    .map(|t2| (t.phoenix.ticks_to_usd(market, t2), f64::NAN))
                    .into_iter()
                    .collect());
            }
        };
        let levels = match side {
            Side::Buy => book.bids,
            Side::Sell => book.asks,
        };
        Ok(levels
            .into_iter()
            .map(|l| (f64_of(l.price), l.quantity.map_or(f64::NAN, f64_of)))
            .collect())
    }

    /// The best price resting on `side`, if any.
    pub async fn best(&self, side: Side) -> Result<Option<f64>> {
        Ok(self.levels(side).await?.first().map(|(price, _)| *price))
    }

    /// Rest `orders` (side, price, size), a few to a transaction on spot
    /// venues. Prices go onto the grid away from the other side. On spot
    /// venues an order that would cross rests at the best price that
    /// doesn't instead.
    pub async fn rest(&self, orders: &[(Side, f64, f64)]) -> Rested {
        match &self.kind {
            Kind::PhoenixV1(t) => {
                let instrument = &t.market().instrument;
                let mut groups = Vec::new();
                for &(side, price, base) in orders {
                    let quantity = instrument.round_quantity(decimal(base), Rounding::Down);
                    if quantity <= Decimal::ZERO {
                        continue;
                    }
                    let order = PhoenixV1Order::PostOnly {
                        side,
                        price: instrument.round_price(decimal(price), rounding(side)),
                        quantity,
                        amend_if_crossing: true,
                        last_valid_slot: None,
                    };
                    match t.instruction(&order) {
                        Ok(ix) => groups.push(InstructionGroup::new("rest", vec![ix])),
                        Err(error) => {
                            return Rested {
                                sent: orders.len(),
                                failed: orders.len(),
                                error: Some(failed("rest")(error)),
                            };
                        }
                    }
                }
                self.rest_batched(groups, phoenix_v1::COMPUTE_UNIT_LIMIT)
                    .await
            }
            Kind::OpenBookV2(t) => {
                let market = t.market();
                let instrument = &market.instrument;
                let mut groups = Vec::new();
                for &(side, price, base) in orders {
                    let quantity = instrument.round_quantity(decimal(base), Rounding::Down);
                    let price = instrument.round_price(decimal(price), rounding(side));
                    let (Ok(price_lots), Ok(max_base_lots)) = (
                        market.price_to_price_lots(price),
                        market.quantity_to_base_lots(quantity),
                    ) else {
                        return Rested {
                            sent: orders.len(),
                            failed: orders.len(),
                            error: Some(failed("rest")("off the grid")),
                        };
                    };
                    if max_base_lots <= 0 {
                        continue;
                    }
                    let args = PlaceOrderArgs {
                        side: match side {
                            Side::Buy => OrderSide::Bid,
                            Side::Sell => OrderSide::Ask,
                        },
                        price_lots,
                        max_base_lots,
                        max_quote_lots_including_fees: NO_QUOTE_LIMIT,
                        client_order_id: 0,
                        order_type: PlaceOrderType::PostOnlySlide,
                        expiry_timestamp: 0,
                        self_trade_behavior: SelfTradeBehavior::CancelProvide,
                        limit: openbook_v2::MATCH_LIMIT,
                    };
                    let ix = openbook_v2::ix::place_order(
                        &self.keypair.pubkey(),
                        &market.keys,
                        &args,
                        openbook_v2::ACCOUNT_NUMBER,
                    );
                    groups.push(InstructionGroup::new("rest", vec![ix]));
                }
                self.rest_batched(groups, openbook_v2::COMPUTE_UNIT_LIMIT)
                    .await
            }
            Kind::Perp(t) => {
                let mut rested = Rested::default();
                for &(side, price, base) in orders {
                    let side = match side {
                        Side::Buy => mtx_kit::Side::Bid,
                        Side::Sell => mtx_kit::Side::Ask,
                    };
                    rested.sent += 1;
                    if let Err(error) = t.place_limit(side, price, base).await {
                        rested.failed += 1;
                        rested.error = Some(Error::from(error));
                    }
                }
                rested
            }
        }
    }

    /// Send `groups`, [`ORDERS_PER_TRANSACTION`] to a transaction: a
    /// transaction that fits more by size can still run out of compute.
    async fn rest_batched(&self, groups: Vec<InstructionGroup>, limit: u32) -> Rested {
        let mut rested = Rested::default();
        for chunk in groups.chunks(ORDERS_PER_TRANSACTION) {
            let (sent, result) = self.send_groups(chunk.to_vec(), limit).await;
            rested.sent += sent;
            if let Err(error) = result {
                rested.failed += sent;
                rested.error = Some(error);
            }
        }
        rested
    }

    async fn send_groups(&self, groups: Vec<InstructionGroup>, limit: u32) -> (usize, Result<()>) {
        let n = groups.len();
        if n == 0 {
            return (0, Ok(()));
        }
        let plan = match chain::pack(&self.keypair.pubkey(), &groups, limit) {
            Ok(plan) => plan,
            Err(error) => return (n, Err(failed("rest")(error))),
        };
        (n, self.send(&plan, "rest").await)
    }

    /// Take up to `base` on `side`, at no worse than `limit_usd`. A seat
    /// with resting orders cancels its own rather than trade with them.
    pub async fn take(&self, side: Side, base: f64, limit_usd: f64) -> Result<()> {
        match &self.kind {
            Kind::PhoenixV1(t) => {
                let market = t.market();
                let instrument = &market.instrument;
                let quantity = instrument.round_quantity(decimal(base), Rounding::Down);
                let price = instrument.round_price(decimal(limit_usd), rounding(side.opposite()));
                let lots = market
                    .quantity_to_base_lots(quantity)
                    .map_err(failed("take"))?;
                let ticks = market.price_to_ticks(price).map_err(failed("take"))?;
                if lots == 0 {
                    return Ok(());
                }
                let mut packet = OrderPacket::ioc(
                    match side {
                        Side::Buy => BookSide::Bid,
                        Side::Sell => BookSide::Ask,
                    },
                    Some(ticks),
                    lots,
                );
                if let OrderPacket::ImmediateOrCancel { self_trade, .. } = &mut packet {
                    *self_trade = SelfTrade::CancelProvide;
                }
                let ix = phoenix_v1::ix::swap(&market.keys, &self.keypair.pubkey(), &packet);
                let (_, sent) = self
                    .send_groups(
                        vec![InstructionGroup::new("take", vec![ix])],
                        phoenix_v1::COMPUTE_UNIT_LIMIT,
                    )
                    .await;
                sent
            }
            Kind::OpenBookV2(t) => {
                let instrument = &t.market().instrument;
                let order = OpenBookV2Order::Take {
                    side,
                    quantity: instrument.round_quantity(decimal(base), Rounding::Down),
                    worst_price: instrument
                        .round_price(decimal(limit_usd), rounding(side.opposite())),
                };
                // A take-only order trades from the wallet, never with the
                // seat's resting orders, which belong to its open orders
                // account.
                t.submit(&order, &self.keypair)
                    .await
                    .map(|_| ())
                    .map_err(failed("take"))
            }
            Kind::Perp(t) => {
                let lots = t.lots(base)?;
                if lots == 0 {
                    return Ok(());
                }
                let ticket = MarketOrderTicket::builder()
                    .authority(t.authority())
                    .trader_account(t.account)
                    .symbol(t.env.symbol.clone())
                    .side(match side {
                        Side::Buy => TicketSide::Bid,
                        Side::Sell => TicketSide::Ask,
                    })
                    .price((limit_usd * 100.0).round() / 100.0)
                    .num_base_lots(lots)
                    .self_trade_behavior(PerpSelfTrade::CancelProvide)
                    .build()
                    .map_err(failed("take"))?;
                let ixs = t
                    .phoenix
                    .tx()
                    .place_market_order(ticket)
                    .await
                    .map_err(failed("take"))?;
                mtx_kit::send(&t.rpc, &t.keypair, ixs, "take")
                    .map(|_| ())
                    .map_err(Error::from)
            }
        }
    }

    /// The seat's position on a perp market, in base units.
    pub async fn position(&self) -> Option<f64> {
        match &self.kind {
            Kind::Perp(t) => t.position().await.ok().map(|p| p.size),
            _ => None,
        }
    }

    /// Crank an OpenBook market: settle every event waiting in its heap
    /// whose open orders account exists, which frees the slots filled
    /// orders hold in their makers' accounts. A no-op elsewhere, and on a
    /// market only an admin may crank.
    pub async fn crank(&self) -> Result<()> {
        let Kind::OpenBookV2(t) = &self.kind else {
            return Ok(());
        };
        let market = t.market();
        let account = self
            .chain
            .get_account(&market.keys.market)
            .await
            .map_err(failed("crank"))?
            .ok_or_else(|| failed("crank")("no market"))?;
        let state = openbook_v2::MarketAccount::decode(&account.data).map_err(failed("crank"))?;
        if state.consume_events_admin.is_some() {
            return Ok(());
        }
        let heap = self
            .chain
            .get_account(&market.keys.event_heap)
            .await
            .map_err(failed("crank"))?
            .ok_or_else(|| failed("crank")("no event heap"))?;
        let events = openbook_v2::fills::heap_events(&heap.data).map_err(failed("crank"))?;
        let mut named: Vec<Pubkey> = Vec::new();
        for (_, owner) in &events {
            if !named.contains(owner) {
                named.push(*owner);
            }
        }
        // Only accounts that still exist can take their events: the
        // snapshot's own makers may not be in it.
        let mut live = Vec::new();
        for chunk in named.chunks(100) {
            let found = self
                .chain
                .get_multiple_accounts(chunk)
                .await
                .map_err(failed("crank"))?;
            live.extend(
                chunk
                    .iter()
                    .zip(found)
                    .filter(|(_, a)| {
                        a.as_ref()
                            .is_some_and(|a| a.owner == openbook_v2::PROGRAM_ID)
                    })
                    .map(|(k, _)| *k),
            );
        }
        // Every event a live account can take, oldest first, a few accounts
        // and events to a transaction.
        let mut batches: Vec<(Vec<Pubkey>, Vec<u64>)> = Vec::new();
        for (slot, owner) in events.into_iter().filter(|(_, o)| live.contains(o)) {
            let full = batches.last().is_none_or(|(owners, slots)| {
                slots.len() >= CRANK_EVENTS
                    || (!owners.contains(&owner) && owners.len() >= CRANK_ACCOUNTS)
            });
            if full {
                batches.push((Vec::new(), Vec::new()));
            }
            let (owners, slots) = batches.last_mut().expect("a batch");
            if !owners.contains(&owner) {
                owners.push(owner);
            }
            slots.push(slot);
        }
        for (owners, slots) in batches {
            let ix = openbook_v2::ix::consume_given_events(&market.keys, &owners, &slots);
            let (_, sent) = self
                .send_groups(
                    vec![InstructionGroup::new("crank", vec![ix])],
                    openbook_v2::COMPUTE_UNIT_LIMIT,
                )
                .await;
            sent?;
        }
        Ok(())
    }
}
