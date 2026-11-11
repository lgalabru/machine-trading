//! OpenBook v2 instructions: the program's addresses and the trader
//! instructions, as the program's IDL defines them.
//!
//! Instruction data is the 8-byte Anchor discriminator followed by the
//! Borsh-encoded arguments. Optional accounts that are absent (an oracle,
//! the open orders admin, a referrer) are passed as the program's own
//! address, Anchor's convention for `None`.

use solana_pubkey::Pubkey;
use solana_transaction::{AccountMeta, Instruction};

use super::{PROGRAM_ID, market::MarketKeys};
use crate::chain::token::{ata, token_program};

const PLACE_TAKE_ORDER: [u8; 8] = [0x03, 0x2c, 0x47, 0x03, 0x1a, 0xc7, 0xcb, 0x55];
const PLACE_ORDER: [u8; 8] = [0x33, 0xc2, 0x9b, 0xaf, 0x6d, 0x82, 0x60, 0x6a];
const CANCEL_ALL_ORDERS: [u8; 8] = [0xc4, 0x53, 0xf3, 0xab, 0x11, 0x64, 0xa0, 0x8f];
const SETTLE_FUNDS: [u8; 8] = [0xee, 0x40, 0xa3, 0x60, 0x4b, 0xab, 0x10, 0x21];
const CREATE_OPEN_ORDERS_INDEXER: [u8; 8] = [0x40, 0x40, 0x99, 0xff, 0xd9, 0x47, 0xf9, 0x85];
const CREATE_OPEN_ORDERS_ACCOUNT: [u8; 8] = [0xcc, 0xb5, 0xaf, 0xde, 0x28, 0x7d, 0xbc, 0x47];
const CONSUME_GIVEN_EVENTS: [u8; 8] = [0xd1, 0xe3, 0x36, 0x04, 0x6d, 0xac, 0x29, 0x47];

/// The PDA that signs for a market's vaults.
pub fn market_authority(market: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"Market", market.as_ref()], &PROGRAM_ID).0
}

/// The program's event authority (Anchor's event CPI).
pub fn event_authority() -> Pubkey {
    Pubkey::find_program_address(&[b"__event_authority"], &PROGRAM_ID).0
}

/// A wallet's open orders indexer, which numbers its open orders accounts.
pub fn open_orders_indexer(owner: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"OpenOrdersIndexer", owner.as_ref()], &PROGRAM_ID).0
}

/// A wallet's `n`-th open orders account (numbered from 1).
pub fn open_orders_account(owner: &Pubkey, n: u32) -> Pubkey {
    Pubkey::find_program_address(
        &[b"OpenOrders", owner.as_ref(), &n.to_le_bytes()],
        &PROGRAM_ID,
    )
    .0
}

/// Which side an order is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderSide {
    /// Buys the base asset.
    Bid,
    /// Sells it.
    Ask,
}

impl OrderSide {
    fn tag(self) -> u8 {
        match self {
            Self::Bid => 0,
            Self::Ask => 1,
        }
    }
}

/// How an order trades. The deployed program has no fill-or-kill: an order
/// type tagged 5 fails to deserialize (Anchor error 102).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaceOrderType {
    /// Takes what crosses, rests the rest.
    Limit,
    /// Takes what crosses; nothing rests.
    ImmediateOrCancel,
    /// Rests; refused if it would cross.
    PostOnly,
    /// Takes at any price; nothing rests.
    Market,
    /// Rests at the best price that does not cross.
    PostOnlySlide,
}

impl PlaceOrderType {
    fn tag(self) -> u8 {
        match self {
            Self::Limit => 0,
            Self::ImmediateOrCancel => 1,
            Self::PostOnly => 2,
            Self::Market => 3,
            Self::PostOnlySlide => 4,
        }
    }
}

/// What happens when an order would match the owner's own resting order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SelfTradeBehavior {
    /// Reduce the taking order by the resting order's size.
    #[default]
    DecrementTake,
    /// Cancel the resting order.
    CancelProvide,
    /// Fail the instruction.
    AbortTransaction,
}

impl SelfTradeBehavior {
    fn tag(self) -> u8 {
        match self {
            Self::DecrementTake => 0,
            Self::CancelProvide => 1,
            Self::AbortTransaction => 2,
        }
    }
}

/// The most quote lots, fees included, an order may spend when only its
/// base size should bind.
pub const NO_QUOTE_LIMIT: i64 = i64::MAX;

fn ro(key: Pubkey) -> AccountMeta {
    AccountMeta::new_readonly(key, false)
}

fn rw(key: Pubkey) -> AccountMeta {
    AccountMeta::new(key, false)
}

fn data(discriminator: [u8; 8], body: &[u8]) -> Vec<u8> {
    let mut d = discriminator.to_vec();
    d.extend_from_slice(body);
    d
}

/// PlaceTakeOrder: takes liquidity straight from and to the owner's
/// associated token accounts; needs no open orders account. `limit` is the
/// most resting orders to match.
#[allow(clippy::too_many_arguments)]
pub fn place_take_order(
    owner: &Pubkey,
    m: &MarketKeys,
    side: OrderSide,
    price_lots: i64,
    max_base_lots: i64,
    max_quote_lots_including_fees: i64,
    order_type: PlaceOrderType,
    limit: u8,
) -> Instruction {
    let mut body = vec![side.tag()];
    body.extend_from_slice(&price_lots.to_le_bytes());
    body.extend_from_slice(&max_base_lots.to_le_bytes());
    body.extend_from_slice(&max_quote_lots_including_fees.to_le_bytes());
    body.extend_from_slice(&[order_type.tag(), limit]);
    Instruction {
        program_id: PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*owner, true),
            AccountMeta::new(*owner, true),
            rw(m.market),
            ro(m.authority),
            rw(m.bids),
            rw(m.asks),
            rw(m.base_vault),
            rw(m.quote_vault),
            rw(m.event_heap),
            rw(ata(owner, &m.base_mint)),
            rw(ata(owner, &m.quote_mint)),
            ro(PROGRAM_ID),
            ro(PROGRAM_ID),
            ro(token_program()),
            ro(solana_system_interface::program::id()),
            ro(PROGRAM_ID),
        ],
        data: data(PLACE_TAKE_ORDER, &body),
    }
}

/// An order for [`place_order`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlaceOrderArgs {
    /// Bid or ask.
    pub side: OrderSide,
    /// Price, in price lots.
    pub price_lots: i64,
    /// Size, in base lots.
    pub max_base_lots: i64,
    /// The most quote lots, fees included, to spend.
    pub max_quote_lots_including_fees: i64,
    /// The owner's own id for the order.
    pub client_order_id: u64,
    /// How it trades.
    pub order_type: PlaceOrderType,
    /// Unix second it expires at (0: never).
    pub expiry_timestamp: u64,
    /// Matching the owner's own orders.
    pub self_trade_behavior: SelfTradeBehavior,
    /// The most resting orders to match.
    pub limit: u8,
}

/// PlaceOrder: an order on the owner's `n`-th open orders account, funded
/// from its associated token account for the side's asset.
pub fn place_order(owner: &Pubkey, m: &MarketKeys, args: &PlaceOrderArgs, n: u32) -> Instruction {
    let (vault, token) = match args.side {
        OrderSide::Bid => (m.quote_vault, ata(owner, &m.quote_mint)),
        OrderSide::Ask => (m.base_vault, ata(owner, &m.base_mint)),
    };
    let mut body = vec![args.side.tag()];
    body.extend_from_slice(&args.price_lots.to_le_bytes());
    body.extend_from_slice(&args.max_base_lots.to_le_bytes());
    body.extend_from_slice(&args.max_quote_lots_including_fees.to_le_bytes());
    body.extend_from_slice(&args.client_order_id.to_le_bytes());
    body.push(args.order_type.tag());
    body.extend_from_slice(&args.expiry_timestamp.to_le_bytes());
    body.extend_from_slice(&[args.self_trade_behavior.tag(), args.limit]);
    Instruction {
        program_id: PROGRAM_ID,
        accounts: vec![
            AccountMeta::new_readonly(*owner, true),
            rw(open_orders_account(owner, n)),
            ro(PROGRAM_ID),
            rw(token),
            rw(m.market),
            rw(m.bids),
            rw(m.asks),
            rw(m.event_heap),
            rw(vault),
            ro(PROGRAM_ID),
            ro(PROGRAM_ID),
            ro(token_program()),
        ],
        data: data(PLACE_ORDER, &body),
    }
}

/// CancelAllOrders: every resting order of the owner's `n`-th open orders
/// account, both sides, up to 255 of them.
pub fn cancel_all_orders(owner: &Pubkey, m: &MarketKeys, n: u32) -> Instruction {
    Instruction {
        program_id: PROGRAM_ID,
        accounts: vec![
            AccountMeta::new_readonly(*owner, true),
            rw(open_orders_account(owner, n)),
            ro(m.market),
            rw(m.bids),
            rw(m.asks),
        ],
        data: data(CANCEL_ALL_ORDERS, &[0, 255]),
    }
}

/// SettleFunds: the free funds of the owner's `n`-th open orders account to
/// its associated token accounts.
pub fn settle_funds(owner: &Pubkey, m: &MarketKeys, n: u32) -> Instruction {
    Instruction {
        program_id: PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*owner, true),
            AccountMeta::new(*owner, true),
            rw(open_orders_account(owner, n)),
            rw(m.market),
            ro(m.authority),
            rw(m.base_vault),
            rw(m.quote_vault),
            rw(ata(owner, &m.base_mint)),
            rw(ata(owner, &m.quote_mint)),
            ro(PROGRAM_ID),
            ro(token_program()),
            ro(solana_system_interface::program::id()),
        ],
        data: SETTLE_FUNDS.to_vec(),
    }
}

/// CreateOpenOrdersIndexer: the owner's indexer, needed once before its
/// first open orders account.
pub fn create_open_orders_indexer(owner: &Pubkey) -> Instruction {
    Instruction {
        program_id: PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*owner, true),
            AccountMeta::new_readonly(*owner, true),
            rw(open_orders_indexer(owner)),
            ro(solana_system_interface::program::id()),
        ],
        data: CREATE_OPEN_ORDERS_INDEXER.to_vec(),
    }
}

/// CreateOpenOrdersAccount: the owner's `n`-th open orders account on
/// `market`.
pub fn create_open_orders_account(
    owner: &Pubkey,
    market: &Pubkey,
    n: u32,
    name: &str,
) -> Instruction {
    let mut body = (name.len() as u32).to_le_bytes().to_vec();
    body.extend_from_slice(name.as_bytes());
    Instruction {
        program_id: PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*owner, true),
            AccountMeta::new_readonly(*owner, true),
            ro(PROGRAM_ID),
            rw(open_orders_indexer(owner)),
            rw(open_orders_account(owner, n)),
            ro(*market),
            ro(solana_system_interface::program::id()),
        ],
        data: data(CREATE_OPEN_ORDERS_ACCOUNT, &body),
    }
}

/// ConsumeGivenEvents: the crank, for the events at `slots` of the market's
/// heap: the program takes the first eight and leaves the rest. Each settles into the open orders account it names, which `owners`
/// must list; one whose account isn't listed stays in the heap. Unlike
/// ConsumeEvents, which walks the heap from its oldest event, events no one
/// can settle (their accounts gone) never hold up the rest. Permissionless
/// on a market without a consume-events admin.
pub fn consume_given_events(m: &MarketKeys, owners: &[Pubkey], slots: &[u64]) -> Instruction {
    let mut accounts = vec![ro(PROGRAM_ID), rw(m.market), rw(m.event_heap)];
    accounts.extend(owners.iter().copied().map(rw));
    let mut body = (slots.len() as u32).to_le_bytes().to_vec();
    for slot in slots {
        body.extend_from_slice(&slot.to_le_bytes());
    }
    Instruction {
        program_id: PROGRAM_ID,
        accounts,
        data: data(CONSUME_GIVEN_EVENTS, &body),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys() -> MarketKeys {
        let k = |b: u8| Pubkey::new_from_array([b; 32]);
        MarketKeys {
            market: k(1),
            authority: k(2),
            bids: k(3),
            asks: k(4),
            event_heap: k(5),
            base_vault: k(6),
            quote_vault: k(7),
            base_mint: k(8),
            quote_mint: k(9),
        }
    }

    #[test]
    fn take_orders_encode_their_arguments() {
        let owner = Pubkey::new_from_array([42; 32]);
        let ix = place_take_order(
            &owner,
            &keys(),
            OrderSide::Ask,
            149_900,
            2_500,
            NO_QUOTE_LIMIT,
            PlaceOrderType::ImmediateOrCancel,
            20,
        );
        assert_eq!(&ix.data[..8], &PLACE_TAKE_ORDER);
        assert_eq!(ix.data[8], 1);
        assert_eq!(
            i64::from_le_bytes(ix.data[9..17].try_into().unwrap()),
            149_900
        );
        assert_eq!(
            i64::from_le_bytes(ix.data[17..25].try_into().unwrap()),
            2_500
        );
        assert_eq!(&ix.data[33..], &[1, 20]);
        assert_eq!(ix.accounts.len(), 16);
        assert_eq!(ix.accounts[9].pubkey, ata(&owner, &keys().base_mint));
        assert_eq!(ix.accounts[11].pubkey, PROGRAM_ID, "no oracle");
    }

    #[test]
    fn resting_orders_use_the_owners_account() {
        let owner = Pubkey::new_from_array([42; 32]);
        let ix = place_order(
            &owner,
            &keys(),
            &PlaceOrderArgs {
                side: OrderSide::Ask,
                price_lots: 1,
                max_base_lots: 2,
                max_quote_lots_including_fees: NO_QUOTE_LIMIT,
                client_order_id: 3,
                order_type: PlaceOrderType::PostOnly,
                expiry_timestamp: 0,
                self_trade_behavior: SelfTradeBehavior::default(),
                limit: 10,
            },
            1,
        );
        assert_eq!(ix.accounts[1].pubkey, open_orders_account(&owner, 1));
        assert_eq!(ix.accounts[3].pubkey, ata(&owner, &keys().base_mint));
        assert_eq!(ix.accounts[8].pubkey, keys().base_vault);
        assert_eq!(ix.data.len(), 8 + 1 + 24 + 8 + 1 + 8 + 2);
        assert_eq!(cancel_all_orders(&owner, &keys(), 1).data[8..], [0, 255]);
        assert_eq!(settle_funds(&owner, &keys(), 1).accounts.len(), 12);
        let create = create_open_orders_account(&owner, &keys().market, 1, "");
        assert_eq!(create.data[8..], [0, 0, 0, 0]);
        assert_eq!(
            create_open_orders_indexer(&owner).accounts[2].pubkey,
            open_orders_indexer(&owner)
        );
        let crank = consume_given_events(&keys(), &[owner], &[3, 7]);
        assert_eq!(crank.data[..8], CONSUME_GIVEN_EVENTS);
        assert_eq!(
            crank.data[8..],
            [2, 0, 0, 0, 3, 0, 0, 0, 0, 0, 0, 0, 7, 0, 0, 0, 0, 0, 0, 0]
        );
        assert_eq!(crank.accounts[0].pubkey, PROGRAM_ID, "no admin");
        assert_eq!(crank.accounts[2].pubkey, keys().event_heap);
        assert!(crank.accounts[3].is_writable);
    }
}
