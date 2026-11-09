//! A trader's handle on one OpenBook v2 market.
//!
//! [`OpenBookV2Trader`] reads the market's book sides and the trader's open
//! orders account, and turns orders into [`ExecutionPlan`]s. Take orders
//! settle from and to the trader's token accounts; resting orders live on
//! its first open orders account, which [`plan_open_account`] creates.
//!
//! [`plan_open_account`]: OpenBookV2Trader::plan_open_account

use std::sync::Arc;

use rust_decimal::Decimal;
use solana_pubkey::Pubkey;
use solana_transaction::{Instruction, Signature};

use super::{
    COMPUTE_UNIT_LIMIT, OpenBookV2Error, OpenBookV2Market,
    ix::{self, NO_QUOTE_LIMIT, OrderSide, PlaceOrderArgs, PlaceOrderType, SelfTradeBehavior},
    state::{LeafOrder, OpenOrdersIndexer, OpenOrdersPosition, book_side_orders},
};
use crate::{
    chain::{
        self, ChainClient, ChainError, ExecutionPlan, InstructionGroup, RpcChainClient, SendMode,
        TransactionSigner, token,
    },
    core::{OrderBook, Side},
};

/// What a trader holds on one market: the account the OpenBook venue
/// profile measures, its wallet's token accounts for the market's two mints
/// and every open orders account it owns on the market.
///
/// For SOL, the wallet's holding is its wrapped-SOL token account; the
/// wallet's native SOL pays fees and is not counted. A maker's fills count
/// once `consume_events` has settled them into its open orders account.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Holdings {
    /// Base atoms in the trader's associated base token account.
    pub base_wallet: u64,
    /// Quote atoms in its associated quote token account.
    pub quote_wallet: u64,
    /// Each open orders account the trader owns on the market, as its
    /// indexer lists them.
    pub open_orders: Vec<(Pubkey, OpenOrdersPosition)>,
}

impl Holdings {
    /// The position the spot profile defines: base held in the wallet and
    /// in every open orders account, in base units.
    pub fn base(&self, market: &OpenBookV2Market) -> Decimal {
        let a = &market.account;
        let atoms = i128::from(self.base_wallet)
            + self
                .open_orders
                .iter()
                .map(|(_, p)| p.base_atoms(a.base_lot_size))
                .sum::<i128>();
        Decimal::from_i128_with_scale(atoms, u32::from(a.base_decimals))
    }

    /// Quote held in the wallet and in every open orders account, in quote
    /// units.
    pub fn quote(&self, market: &OpenBookV2Market) -> Decimal {
        let a = &market.account;
        let atoms = i128::from(self.quote_wallet)
            + self
                .open_orders
                .iter()
                .map(|(_, p)| p.quote_atoms(a.quote_lot_size))
                .sum::<i128>();
        Decimal::from_i128_with_scale(atoms, u32::from(a.quote_decimals))
    }
}

/// The open orders account this adapter trades from: the trader's first.
pub const ACCOUNT_NUMBER: u32 = 1;

/// The most resting orders an order matches before it stops.
pub const MATCH_LIMIT: u8 = 20;

/// An order in the form OpenBook v2 takes it, in decimal units.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenBookV2Order {
    /// Takes what crosses at `worst_price` or better, from and to the
    /// trader's token accounts; nothing rests.
    Take {
        /// Buy or sell.
        side: Side,
        /// Base units, on the lot grid.
        quantity: Decimal,
        /// The worst price, on the price-lot grid.
        worst_price: Decimal,
        /// Fill all of `quantity` or nothing.
        all_or_none: bool,
    },
    /// Rests at `price` on the trader's open orders account; refused if it
    /// would cross.
    PostOnly {
        /// Buy or sell.
        side: Side,
        /// Price, on the price-lot grid.
        price: Decimal,
        /// Base units, on the lot grid.
        quantity: Decimal,
        /// Unix second it expires at (0: never).
        expiry_timestamp: u64,
    },
    /// Takes what crosses at `price`, rests the rest.
    Limit {
        /// Buy or sell.
        side: Side,
        /// Price, on the price-lot grid.
        price: Decimal,
        /// Base units, on the lot grid.
        quantity: Decimal,
    },
}

fn order_side(side: Side) -> OrderSide {
    match side {
        Side::Buy => OrderSide::Bid,
        Side::Sell => OrderSide::Ask,
    }
}

/// The two book sides' resting orders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Book {
    /// Resting bids, in the tree's order.
    pub bids: Vec<LeafOrder>,
    /// Resting asks, in the tree's order.
    pub asks: Vec<LeafOrder>,
}

/// One trader in one OpenBook v2 market.
pub struct OpenBookV2Trader {
    market: OpenBookV2Market,
    authority: Pubkey,
    chain: Arc<dyn ChainClient>,
    mode: SendMode,
}

impl OpenBookV2Trader {
    /// A trader for `authority` in `market`, talking to `rpc_url`.
    pub fn new(market: OpenBookV2Market, authority: Pubkey, rpc_url: impl Into<String>) -> Self {
        Self::with_chain(market, authority, Arc::new(RpcChainClient::new(rpc_url)))
    }

    /// A trader that talks through `chain`.
    pub fn with_chain(
        market: OpenBookV2Market,
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
    ) -> Result<Self, OpenBookV2Error> {
        let account = chain
            .get_account(&address)
            .await?
            .ok_or(ChainError::AccountNotFound(address))?;
        let market = OpenBookV2Market::decode(address, &account.data, base_symbol, quote_symbol)?;
        Ok(Self::with_chain(market, authority, chain))
    }

    /// How plans are sent from now on.
    pub fn with_send_mode(mut self, mode: SendMode) -> Self {
        self.mode = mode;
        self
    }

    /// The market traded.
    pub fn market(&self) -> &OpenBookV2Market {
        &self.market
    }

    /// The wallet that trades and signs.
    pub fn authority(&self) -> Pubkey {
        self.authority
    }

    /// The trader's open orders account (it may not exist yet).
    pub fn open_orders_account(&self) -> Pubkey {
        ix::open_orders_account(&self.authority, ACCOUNT_NUMBER)
    }

    /// Both book sides, in one read.
    pub async fn book(&self) -> Result<Book, OpenBookV2Error> {
        let k = &self.market.keys;
        let accounts = self.chain.get_multiple_accounts(&[k.bids, k.asks]).await?;
        let side = |i: usize, key: Pubkey| -> Result<Vec<LeafOrder>, OpenBookV2Error> {
            let account = accounts[i]
                .as_ref()
                .ok_or(ChainError::AccountNotFound(key))?;
            Ok(book_side_orders(&account.data)?)
        };
        Ok(Book {
            bids: side(0, k.bids)?,
            asks: side(1, k.asks)?,
        })
    }

    /// The book by price level. Orders expired at `unix_timestamp` are left
    /// out.
    pub async fn order_book(&self, unix_timestamp: u64) -> Result<OrderBook, OpenBookV2Error> {
        let book = self.book().await?;
        Ok(self
            .market
            .order_book(&book.bids, &book.asks, unix_timestamp, None))
    }

    /// The position of the open orders account this trader trades from,
    /// if the account exists.
    pub async fn open_orders_position(
        &self,
    ) -> Result<Option<OpenOrdersPosition>, OpenBookV2Error> {
        match self.chain.get_account(&self.open_orders_account()).await? {
            Some(account) => Ok(Some(OpenOrdersPosition::decode(&account.data)?)),
            None => Ok(None),
        }
    }

    /// What the trader holds on the market: its wallet's token accounts and
    /// every open orders account its indexer lists on this market.
    pub async fn holdings(&self) -> Result<Holdings, OpenBookV2Error> {
        let k = &self.market.keys;
        let indexer = ix::open_orders_indexer(&self.authority);
        let wallets = [
            token::ata(&self.authority, &k.base_mint),
            token::ata(&self.authority, &k.quote_mint),
            indexer,
        ];
        let accounts = self.chain.get_multiple_accounts(&wallets).await?;
        let balance = |a: &Option<solana_account::Account>| -> Result<u64, ChainError> {
            a.as_ref().map_or(Ok(0), token::token_balance)
        };
        let listed = match &accounts[2] {
            Some(account) => OpenOrdersIndexer::decode(&account.data)?.addresses,
            None => Vec::new(),
        };
        let open_orders = if listed.is_empty() {
            Vec::new()
        } else {
            self.chain.get_multiple_accounts(&listed).await?
        };
        let mut held = Vec::new();
        for (address, account) in listed.into_iter().zip(open_orders) {
            let Some(account) = account else { continue };
            let position = OpenOrdersPosition::decode(&account.data)?;
            if position.market == k.market && position.owner == self.authority {
                held.push((address, position));
            }
        }
        Ok(Holdings {
            base_wallet: balance(&accounts[0])?,
            quote_wallet: balance(&accounts[1])?,
            open_orders: held,
        })
    }

    fn lots(&self, quantity: Decimal) -> Result<i64, OpenBookV2Error> {
        let lots = self.market.quantity_to_base_lots(quantity)?;
        if lots == 0 {
            return Err(OpenBookV2Error::InvalidOrder(format!(
                "quantity {quantity} is zero lots"
            )));
        }
        Ok(lots)
    }

    fn price_lots(&self, price: Decimal) -> Result<i64, OpenBookV2Error> {
        let lots = self.market.price_to_price_lots(price)?;
        if lots == 0 {
            return Err(OpenBookV2Error::InvalidOrder(format!(
                "price {price} is zero lots"
            )));
        }
        Ok(lots)
    }

    /// The instruction for one order, validated against the market's grid.
    pub fn instruction(&self, order: &OpenBookV2Order) -> Result<Instruction, OpenBookV2Error> {
        let k = &self.market.keys;
        let resting = |side,
                       price,
                       quantity,
                       order_type,
                       expiry_timestamp|
         -> Result<Instruction, OpenBookV2Error> {
            Ok(ix::place_order(
                &self.authority,
                k,
                &PlaceOrderArgs {
                    side: order_side(side),
                    price_lots: self.price_lots(price)?,
                    max_base_lots: self.lots(quantity)?,
                    max_quote_lots_including_fees: NO_QUOTE_LIMIT,
                    client_order_id: 0,
                    order_type,
                    expiry_timestamp,
                    self_trade_behavior: SelfTradeBehavior::default(),
                    limit: MATCH_LIMIT,
                },
                ACCOUNT_NUMBER,
            ))
        };
        match *order {
            OpenBookV2Order::Take {
                side,
                quantity,
                worst_price,
                all_or_none,
            } => Ok(ix::place_take_order(
                &self.authority,
                k,
                order_side(side),
                self.price_lots(worst_price)?,
                self.lots(quantity)?,
                NO_QUOTE_LIMIT,
                if all_or_none {
                    PlaceOrderType::FillOrKill
                } else {
                    PlaceOrderType::ImmediateOrCancel
                },
                MATCH_LIMIT,
            )),
            OpenBookV2Order::PostOnly {
                side,
                price,
                quantity,
                expiry_timestamp,
            } => resting(
                side,
                price,
                quantity,
                PlaceOrderType::PostOnly,
                expiry_timestamp,
            ),
            OpenBookV2Order::Limit {
                side,
                price,
                quantity,
            } => resting(side, price, quantity, PlaceOrderType::Limit, 0),
        }
    }

    /// The plan for one order: one transaction.
    pub fn plan(&self, order: &OpenBookV2Order) -> Result<ExecutionPlan, OpenBookV2Error> {
        let label = match order {
            OpenBookV2Order::Take { .. } => "openbook v2 take order",
            OpenBookV2Order::PostOnly { .. } => "openbook v2 post-only order",
            OpenBookV2Order::Limit { .. } => "openbook v2 limit order",
        };
        self.pack(label, vec![self.instruction(order)?])
    }

    /// The plan to create the trader's indexer and first open orders
    /// account, which resting orders need.
    pub fn plan_open_account(&self) -> Result<ExecutionPlan, OpenBookV2Error> {
        self.pack(
            "openbook v2 open orders account",
            vec![
                ix::create_open_orders_indexer(&self.authority),
                ix::create_open_orders_account(
                    &self.authority,
                    &self.market.keys.market,
                    ACCOUNT_NUMBER,
                    "",
                ),
            ],
        )
    }

    /// The plan to cancel every resting order and settle the open orders
    /// account's free funds to the wallet.
    pub fn plan_cancel_all(&self) -> Result<ExecutionPlan, OpenBookV2Error> {
        let k = &self.market.keys;
        self.pack(
            "openbook v2 cancel all",
            vec![
                ix::cancel_all_orders(&self.authority, k, ACCOUNT_NUMBER),
                ix::settle_funds(&self.authority, k, ACCOUNT_NUMBER),
            ],
        )
    }

    fn pack(
        &self,
        label: &str,
        instructions: Vec<Instruction>,
    ) -> Result<ExecutionPlan, OpenBookV2Error> {
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
    ) -> Result<Signature, OpenBookV2Error> {
        let signatures = self.mode.send(plan, signer, &*self.chain).await?;
        Ok(*signatures
            .last()
            .expect("a non-empty plan sends at least one transaction"))
    }

    /// Plans and sends one order.
    pub async fn submit(
        &self,
        order: &OpenBookV2Order,
        signer: &dyn TransactionSigner,
    ) -> Result<Signature, OpenBookV2Error> {
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
        protocols::openbook_v2::{
            PROGRAM_ID,
            state::testing::{book_side_bytes, market_bytes},
        },
    };

    fn trader(chain: Arc<MockChainClient>, authority: Pubkey) -> OpenBookV2Trader {
        let keys: [Pubkey; 8] = std::array::from_fn(|i| Pubkey::new_from_array([i as u8 + 1; 32]));
        let market =
            OpenBookV2Market::decode(Pubkey::new_unique(), &market_bytes(&keys), "SOL", "USDC")
                .unwrap();
        let account = |data| Account {
            lamports: 1,
            data,
            owner: PROGRAM_ID,
            executable: false,
            rent_epoch: 0,
        };
        let owner = Pubkey::new_unique();
        chain.set_account(
            keys[1],
            account(book_side_bytes(&[(150_000, 1, 1_000, owner, 0)])),
        );
        chain.set_account(keys[2], account(book_side_bytes(&[])));
        OpenBookV2Trader::with_chain(market, authority, chain)
    }

    #[test]
    fn orders_become_validated_instructions() {
        let t = trader(Arc::new(MockChainClient::new()), Pubkey::new_unique());
        let take = t
            .instruction(&OpenBookV2Order::Take {
                side: Side::Sell,
                quantity: dec!(2.5),
                worst_price: dec!(149.9),
                all_or_none: false,
            })
            .unwrap();
        assert_eq!(take.data[8], 1, "an ask");
        assert_eq!(
            i64::from_le_bytes(take.data[9..17].try_into().unwrap()),
            149_900
        );
        assert_eq!(take.data[33], 1, "immediate or cancel");
        let post = t
            .instruction(&OpenBookV2Order::PostOnly {
                side: Side::Sell,
                price: dec!(150.5),
                quantity: dec!(1),
                expiry_timestamp: 0,
            })
            .unwrap();
        assert_eq!(post.accounts[1].pubkey, t.open_orders_account());
        assert!(
            t.instruction(&OpenBookV2Order::Limit {
                side: Side::Buy,
                price: dec!(150.0001),
                quantity: dec!(1)
            })
            .is_err()
        );
    }

    #[tokio::test]
    async fn reads_the_book_and_submits() {
        let chain = Arc::new(MockChainClient::new());
        let signer = Keypair::new();
        let t = trader(chain.clone(), signer.pubkey())
            .with_send_mode(SendMode::Submit(SubmitOptions { simulate: false }));
        let book = t.order_book(0).await.unwrap();
        assert_eq!(book.bids.len(), 1);
        assert_eq!(book.bids[0].price, dec!(150));
        assert!(book.asks.is_empty());
        assert_eq!(t.open_orders_position().await.unwrap(), None);
        assert_eq!(t.holdings().await.unwrap(), Holdings::default());
        let plan = t.plan_open_account().unwrap();
        assert_eq!(plan.steps[0].instructions.len(), 2);
        t.send(&plan, &signer).await.unwrap();
        assert_eq!(chain.sent().len(), 1);
    }

    #[tokio::test]
    async fn holdings_count_the_wallet_and_every_open_orders_account_on_the_market() {
        use solana_program_pack::Pack;
        use spl_token_interface::state::{Account as TokenState, AccountState};

        use crate::protocols::openbook_v2::state::testing::{indexer_bytes, open_orders_bytes};

        let chain = Arc::new(MockChainClient::new());
        let authority = Pubkey::new_unique();
        let t = trader(chain.clone(), authority);
        let k = t.market().keys;
        let owned = |owner: Pubkey, data| Account {
            lamports: 1,
            data,
            owner,
            executable: false,
            rent_epoch: 0,
        };
        let token = |mint: Pubkey, amount: u64| {
            let mut data = vec![0u8; TokenState::LEN];
            TokenState {
                mint,
                owner: authority,
                amount,
                state: AccountState::Initialized,
                ..Default::default()
            }
            .pack_into_slice(&mut data);
            owned(token::token_program(), data)
        };
        chain.set_account(
            token::ata(&authority, &k.base_mint),
            token(k.base_mint, 1_000_000_000),
        );
        chain.set_account(
            token::ata(&authority, &k.quote_mint),
            token(k.quote_mint, 5_000_000),
        );
        let (first, second, elsewhere) = (
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        );
        chain.set_account(
            ix::open_orders_indexer(&authority),
            owned(PROGRAM_ID, indexer_bytes(&[first, second, elsewhere])),
        );
        // 2 lots (0.002 SOL) resting in asks, 0.5 SOL free; 3 USDC in bids.
        chain.set_account(
            first,
            owned(
                PROGRAM_ID,
                open_orders_bytes(&authority, &k.market, 2, 500_000_000, 3_000_000, 0),
            ),
        );
        chain.set_account(
            second,
            owned(
                PROGRAM_ID,
                open_orders_bytes(&authority, &k.market, 0, 0, 0, 1_000_000),
            ),
        );
        chain.set_account(
            elsewhere,
            owned(
                PROGRAM_ID,
                open_orders_bytes(&authority, &Pubkey::new_unique(), 0, 9, 0, 9),
            ),
        );
        let holdings = t.holdings().await.unwrap();
        assert_eq!(holdings.open_orders.len(), 2, "only this market's accounts");
        assert_eq!(holdings.base(t.market()), dec!(1.502));
        assert_eq!(holdings.quote(t.market()), dec!(9));
    }
}
