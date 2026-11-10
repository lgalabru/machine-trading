//! A trader's handle on one Phoenix v1 market.
//!
//! [`PhoenixV1Trader`] reads the market account (book, seats) and turns
//! orders into [`ExecutionPlan`]s. It never holds a key: the methods that
//! send take a [`TransactionSigner`].

use std::sync::Arc;

use rust_decimal::Decimal;
use solana_pubkey::Pubkey;
use solana_transaction::{Instruction, Signature};

use super::{
    COMPUTE_UNIT_LIMIT, PhoenixV1Error, PhoenixV1Market,
    ix::{self, OrderPacket},
    state::{BookSide, MarketState},
};
use crate::{
    chain::{
        self, ChainClient, ChainError, ExecutionPlan, InstructionGroup, RpcChainClient, SendMode,
        TransactionSigner, token,
    },
    core::{OrderBook, Side, SpotBalances},
    protocols::holdings_from_atoms,
};

/// An order in the form Phoenix v1 takes it, in decimal units.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhoenixV1Order {
    /// Takes what crosses at `worst_price` or better, from and to the
    /// trader's token accounts (no seat needed); nothing rests.
    ImmediateOrCancel {
        /// Buy or sell.
        side: Side,
        /// Base units, on the lot grid.
        quantity: Decimal,
        /// The worst price, on the tick grid; `None` for any.
        worst_price: Option<Decimal>,
        /// Fill all of `quantity` or nothing.
        all_or_none: bool,
    },
    /// Rests at `price` without taking; needs a seat.
    PostOnly {
        /// Buy or sell.
        side: Side,
        /// Price, on the tick grid.
        price: Decimal,
        /// Base units, on the lot grid.
        quantity: Decimal,
        /// Move a crossing order to the best price that does not cross,
        /// rather than refuse it.
        amend_if_crossing: bool,
        /// Expire after this slot.
        last_valid_slot: Option<u64>,
    },
    /// Takes what crosses at `price`, rests the rest; needs a seat.
    Limit {
        /// Buy or sell.
        side: Side,
        /// Price, on the tick grid.
        price: Decimal,
        /// Base units, on the lot grid.
        quantity: Decimal,
    },
}

fn book_side(side: Side) -> BookSide {
    match side {
        Side::Buy => BookSide::Bid,
        Side::Sell => BookSide::Ask,
    }
}

/// One trader in one Phoenix v1 market.
pub struct PhoenixV1Trader {
    market: PhoenixV1Market,
    authority: Pubkey,
    chain: Arc<dyn ChainClient>,
    mode: SendMode,
}

impl PhoenixV1Trader {
    /// A trader for `authority` in `market`, talking to `rpc_url`.
    pub fn new(market: PhoenixV1Market, authority: Pubkey, rpc_url: impl Into<String>) -> Self {
        Self::with_chain(market, authority, Arc::new(RpcChainClient::new(rpc_url)))
    }

    /// A trader that talks through `chain`.
    pub fn with_chain(
        market: PhoenixV1Market,
        authority: Pubkey,
        chain: Arc<dyn ChainClient>,
    ) -> Self {
        Self {
            market,
            authority,
            chain,
            mode: SendMode::default(),
        }
    }

    /// Reads the market account at `address` and returns a trader for it.
    pub async fn connect(
        chain: Arc<dyn ChainClient>,
        address: Pubkey,
        base_symbol: &str,
        quote_symbol: &str,
        authority: Pubkey,
    ) -> Result<Self, PhoenixV1Error> {
        let account = chain
            .get_account(&address)
            .await?
            .ok_or(ChainError::AccountNotFound(address))?;
        let market = PhoenixV1Market::decode(address, &account.data, base_symbol, quote_symbol)?;
        Ok(Self::with_chain(market, authority, chain))
    }

    /// How plans are sent from now on.
    pub fn with_send_mode(mut self, mode: SendMode) -> Self {
        self.mode = mode;
        self
    }

    /// The market traded.
    pub fn market(&self) -> &PhoenixV1Market {
        &self.market
    }

    /// The wallet that trades and signs.
    pub fn authority(&self) -> Pubkey {
        self.authority
    }

    /// The trader's seat account (it may not exist yet: see
    /// [`plan_claim_seat`](Self::plan_claim_seat)).
    pub fn seat_address(&self) -> Pubkey {
        ix::seat(&self.market.keys.market, &self.authority)
    }

    /// The chain client plans are sent through.
    pub fn chain(&self) -> &dyn ChainClient {
        &*self.chain
    }

    /// The market account, decoded.
    pub async fn state(&self) -> Result<MarketState, PhoenixV1Error> {
        let address = self.market.keys.market;
        let account = self
            .chain
            .get_account(&address)
            .await?
            .ok_or(ChainError::AccountNotFound(address))?;
        Ok(MarketState::decode(&account.data)?)
    }

    /// The book by price level at the current slot.
    pub async fn order_book(&self) -> Result<OrderBook, PhoenixV1Error> {
        let slot = self.chain.get_slot().await?;
        Ok(self.market.order_book(&self.state().await?, slot, 0))
    }

    /// What the trader holds in the market's two assets: its wallet's
    /// associated token accounts and its seat.
    /// What the trader holds on this market, as the Phoenix venue profile
    /// measures it: its wallet's token accounts for the market's two mints,
    /// and its seat, free and locked. For SOL, the wallet's holding is its
    /// wrapped-SOL token account; the wallet's native SOL pays fees and is
    /// not counted.
    pub async fn holdings(&self) -> Result<SpotBalances, PhoenixV1Error> {
        let k = &self.market.keys;
        let wallets = [
            token::ata(&self.authority, &k.base_mint),
            token::ata(&self.authority, &k.quote_mint),
        ];
        let accounts = self.chain.get_multiple_accounts(&wallets).await?;
        let balance = |a: &Option<solana_account::Account>| -> Result<u64, ChainError> {
            a.as_ref().map_or(Ok(0), token::token_balance)
        };
        let state = self.state().await?;
        let h = &state.header;
        let seat = state.seat(&self.authority).copied().unwrap_or_default();
        let atoms = |lots: u64, lot_size: u64| i128::from(lots) * i128::from(lot_size);
        let i = &self.market.instrument;
        Ok(SpotBalances {
            base: holdings_from_atoms(
                &i.base,
                balance(&accounts[0])?.into(),
                atoms(seat.base_lots_free, h.base_lot_size),
                atoms(seat.base_lots_locked, h.base_lot_size),
            )?,
            quote: holdings_from_atoms(
                &i.quote,
                balance(&accounts[1])?.into(),
                atoms(seat.quote_lots_free, h.quote_lot_size),
                atoms(seat.quote_lots_locked, h.quote_lot_size),
            )?,
        })
    }

    fn lots(&self, quantity: Decimal) -> Result<u64, PhoenixV1Error> {
        let lots = self.market.quantity_to_base_lots(quantity)?;
        if lots == 0 {
            return Err(PhoenixV1Error::InvalidOrder(format!(
                "quantity {quantity} is zero lots"
            )));
        }
        Ok(lots)
    }

    fn ticks(&self, price: Decimal) -> Result<u64, PhoenixV1Error> {
        let ticks = self.market.price_to_ticks(price)?;
        if ticks == 0 {
            return Err(PhoenixV1Error::InvalidOrder(format!(
                "price {price} is zero ticks"
            )));
        }
        Ok(ticks)
    }

    /// The instruction for one order, validated against the market's grid.
    pub fn instruction(&self, order: &PhoenixV1Order) -> Result<Instruction, PhoenixV1Error> {
        let k = &self.market.keys;
        Ok(match *order {
            PhoenixV1Order::ImmediateOrCancel {
                side,
                quantity,
                worst_price,
                all_or_none,
            } => {
                let lots = self.lots(quantity)?;
                let mut packet = OrderPacket::ioc(
                    book_side(side),
                    worst_price.map(|p| self.ticks(p)).transpose()?,
                    lots,
                );
                if all_or_none
                    && let OrderPacket::ImmediateOrCancel {
                        min_base_lots_to_fill,
                        ..
                    } = &mut packet
                {
                    *min_base_lots_to_fill = lots;
                }
                ix::swap(k, &self.authority, &packet)
            }
            PhoenixV1Order::PostOnly {
                side,
                price,
                quantity,
                amend_if_crossing,
                last_valid_slot,
            } => {
                let mut packet = OrderPacket::post_only(
                    book_side(side),
                    self.ticks(price)?,
                    self.lots(quantity)?,
                );
                if let OrderPacket::PostOnly {
                    reject_post_only,
                    last_valid_slot: expiry,
                    ..
                } = &mut packet
                {
                    *reject_post_only = !amend_if_crossing;
                    *expiry = last_valid_slot;
                }
                ix::place_limit_order(k, &self.authority, &packet)
            }
            PhoenixV1Order::Limit {
                side,
                price,
                quantity,
            } => ix::place_limit_order(
                k,
                &self.authority,
                &OrderPacket::limit(book_side(side), self.ticks(price)?, self.lots(quantity)?),
            ),
        })
    }

    /// The plan for one order: one transaction.
    pub fn plan(&self, order: &PhoenixV1Order) -> Result<ExecutionPlan, PhoenixV1Error> {
        let label = match order {
            PhoenixV1Order::ImmediateOrCancel { .. } => "phoenix v1 swap",
            PhoenixV1Order::PostOnly { .. } => "phoenix v1 post-only order",
            PhoenixV1Order::Limit { .. } => "phoenix v1 limit order",
        };
        self.pack(label, vec![self.instruction(order)?])
    }

    /// The plan to cancel every resting order and withdraw the seat's free
    /// funds to the wallet.
    pub fn plan_cancel_all(&self) -> Result<ExecutionPlan, PhoenixV1Error> {
        let k = &self.market.keys;
        self.pack(
            "phoenix v1 cancel all",
            vec![
                ix::cancel_all_orders(k, &self.authority),
                ix::withdraw_funds(k, &self.authority, None, None),
            ],
        )
    }

    /// The plan to claim a seat, which resting orders need.
    pub fn plan_claim_seat(&self) -> Result<ExecutionPlan, PhoenixV1Error> {
        self.pack(
            "phoenix v1 claim seat",
            vec![ix::claim_seat(&self.market.keys, &self.authority)],
        )
    }

    fn pack(
        &self,
        label: &str,
        instructions: Vec<Instruction>,
    ) -> Result<ExecutionPlan, PhoenixV1Error> {
        Ok(chain::pack(
            &self.authority,
            &[InstructionGroup::new(label, instructions)],
            COMPUTE_UNIT_LIMIT,
        )?)
    }

    /// Sends a plan as the trader's [`SendMode`] says. Returns the last
    /// step's signature.
    pub async fn send(
        &self,
        plan: &ExecutionPlan,
        signer: &dyn TransactionSigner,
    ) -> Result<Signature, PhoenixV1Error> {
        let signatures = self.mode.send(plan, signer, &*self.chain).await?;
        Ok(*signatures
            .last()
            .expect("a non-empty plan sends at least one transaction"))
    }

    /// Plans and sends one order.
    pub async fn submit(
        &self,
        order: &PhoenixV1Order,
        signer: &dyn TransactionSigner,
    ) -> Result<Signature, PhoenixV1Error> {
        self.send(&self.plan(order)?, signer).await
    }
}

#[cfg(test)]
mod tests {
    use rust_decimal_macros::dec;
    use solana_account::Account;
    use solana_keypair::Keypair;
    use solana_signer::Signer;

    use super::*;
    use crate::{
        chain::{SubmitOptions, mock::MockChainClient},
        protocols::phoenix_v1::{
            PROGRAM_ID,
            state::testing::{MarketSpec, market_bytes},
        },
    };

    fn trader(chain: Arc<MockChainClient>, authority: Pubkey) -> PhoenixV1Trader {
        let address = Pubkey::new_unique();
        let data = market_bytes(&MarketSpec {
            seats: vec![(authority, 1_000, 2_000, 5, 0)],
            ..MarketSpec::default()
        });
        let market = PhoenixV1Market::decode(address, &data, "SOL", "USDC").unwrap();
        chain.set_account(
            address,
            Account {
                lamports: 1,
                data,
                owner: PROGRAM_ID,
                executable: false,
                rent_epoch: 0,
            },
        );
        PhoenixV1Trader::with_chain(market, authority, chain)
    }

    #[test]
    fn orders_become_validated_instructions() {
        let chain = Arc::new(MockChainClient::new());
        let me = Pubkey::new_unique();
        let t = trader(chain, me);
        let ioc = t
            .instruction(&PhoenixV1Order::ImmediateOrCancel {
                side: Side::Sell,
                quantity: dec!(2.5),
                worst_price: Some(dec!(149.9)),
                all_or_none: true,
            })
            .unwrap();
        assert_eq!(ioc.data[0], 0, "a swap");
        assert_eq!(ioc.data[2], 1, "an ask");
        let packet = &ioc.data[1..];
        assert_eq!(
            u64::from_le_bytes(packet[3..11].try_into().unwrap()),
            149_900
        );
        assert_eq!(
            u64::from_le_bytes(packet[11..19].try_into().unwrap()),
            2_500
        );
        assert_eq!(
            u64::from_le_bytes(packet[27..35].try_into().unwrap()),
            2_500,
            "all or none"
        );

        let post = t
            .instruction(&PhoenixV1Order::PostOnly {
                side: Side::Sell,
                price: dec!(150.5),
                quantity: dec!(1),
                amend_if_crossing: true,
                last_valid_slot: Some(99),
            })
            .unwrap();
        assert_eq!(post.data[0], 2);
        assert_eq!(post.accounts[4].pubkey, t.seat_address());
        assert_eq!(post.data[35], 0, "amended, not refused");

        assert!(matches!(
            t.instruction(&PhoenixV1Order::Limit {
                side: Side::Buy,
                price: dec!(150.0001),
                quantity: dec!(1)
            }),
            Err(PhoenixV1Error::Instrument(_))
        ));
        assert!(matches!(
            t.instruction(&PhoenixV1Order::Limit {
                side: Side::Buy,
                price: dec!(150),
                quantity: dec!(0)
            }),
            Err(PhoenixV1Error::InvalidOrder(_))
        ));
    }

    #[tokio::test]
    async fn submit_mode_sends_without_waiting() {
        let chain = Arc::new(MockChainClient::new());
        let signer = Keypair::new();
        let t = trader(chain.clone(), signer.pubkey());
        let t = t.with_send_mode(SendMode::Submit(SubmitOptions { simulate: false }));
        let plan = t.plan_cancel_all().unwrap();
        assert_eq!(plan.steps[0].instructions.len(), 2);
        let sig = t.send(&plan, &signer).await.unwrap();
        assert_eq!(chain.sent()[0].signatures[0], sig);
        assert!(chain.simulated().is_empty());

        let holdings = t.holdings().await.unwrap();
        assert_eq!(
            holdings.base.wallet,
            dec!(0),
            "no token accounts in the mock"
        );
        assert_eq!(holdings.base.free + holdings.base.locked, dec!(3));
        assert_eq!(holdings.base.total(), dec!(3));
        assert_eq!(holdings.holding().position(), dec!(3));
        assert_eq!(holdings.quote.total(), dec!(0.000005));
        assert_eq!(t.order_book().await.unwrap().bids.len(), 0);
    }
}
