//! Phoenix v1 instructions: the program's addresses, order packets and the
//! trader instructions.
//!
//! Instruction data is a one-byte tag followed by Borsh-encoded arguments.
//! Every trader instruction starts with the same four accounts: the program,
//! its log authority, the market and the trader (signer). Instructions that
//! move tokens add the trader's two associated token accounts, the market's
//! vaults and the token program.

use solana_pubkey::Pubkey;
use solana_transaction::{AccountMeta, Instruction};

use super::{PROGRAM_ID, SEAT_MANAGER_ID, state::BookSide};
use crate::chain::token::{ata, token_program};

/// What a Phoenix instruction needs to know about its market.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MarketKeys {
    /// The market account.
    pub market: Pubkey,
    /// The base mint.
    pub base_mint: Pubkey,
    /// The quote mint.
    pub quote_mint: Pubkey,
    /// The market's base vault.
    pub base_vault: Pubkey,
    /// The market's quote vault.
    pub quote_vault: Pubkey,
}

/// The program's log authority, which every instruction passes.
pub fn log_authority() -> Pubkey {
    Pubkey::find_program_address(&[b"log"], &PROGRAM_ID).0
}

/// A market's vault for `mint`.
pub fn vault(market: &Pubkey, mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"vault", market.as_ref(), mint.as_ref()], &PROGRAM_ID).0
}

/// A trader's seat account on a market.
pub fn seat(market: &Pubkey, trader: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"seat", market.as_ref(), trader.as_ref()], &PROGRAM_ID).0
}

/// The seat manager's account for a market.
pub fn seat_manager(market: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[market.as_ref()], &SEAT_MANAGER_ID).0
}

/// Where the seat manager keeps the deposits of seats it grants.
pub fn seat_deposit_collector(market: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[market.as_ref(), b"deposit"], &SEAT_MANAGER_ID).0
}

/// What happens when an order would match the trader's own resting order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SelfTrade {
    /// Fail the instruction.
    Abort,
    /// Cancel the resting order.
    CancelProvide,
    /// Reduce the taking order by the resting order's size.
    #[default]
    DecrementTake,
}

impl SelfTrade {
    fn tag(self) -> u8 {
        match self {
            Self::Abort => 0,
            Self::CancelProvide => 1,
            Self::DecrementTake => 2,
        }
    }
}

fn side_tag(side: BookSide) -> u8 {
    match side {
        BookSide::Bid => 0,
        BookSide::Ask => 1,
    }
}

fn opt_u64(out: &mut Vec<u8>, v: Option<u64>) {
    match v {
        None => out.push(0),
        Some(x) => {
            out.push(1);
            out.extend_from_slice(&x.to_le_bytes());
        }
    }
}

/// An order as Phoenix takes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderPacket {
    /// Rests without taking: refused (or moved, see `reject_post_only`) if
    /// it would cross.
    PostOnly {
        /// Book side.
        side: BookSide,
        /// Price, in ticks.
        price_in_ticks: u64,
        /// Size, in base lots.
        num_base_lots: u64,
        /// The trader's own id for the order.
        client_order_id: u128,
        /// Refuse a crossing order; when `false`, Phoenix moves it to the
        /// best price that does not cross.
        reject_post_only: bool,
        /// Trade only with funds already on the trader's seat.
        use_only_deposited_funds: bool,
        /// Expire after this slot.
        last_valid_slot: Option<u64>,
        /// Expire after this Unix second.
        last_valid_unix_timestamp: Option<u64>,
        /// Skip, rather than fail, when funds are short.
        fail_silently_on_insufficient_funds: bool,
    },
    /// Takes what crosses, then rests the rest.
    Limit {
        /// Book side.
        side: BookSide,
        /// Price, in ticks.
        price_in_ticks: u64,
        /// Size, in base lots.
        num_base_lots: u64,
        /// Matching the trader's own orders.
        self_trade: SelfTrade,
        /// The most resting orders to match.
        match_limit: Option<u64>,
        /// The trader's own id for the order.
        client_order_id: u128,
        /// Trade only with funds already on the trader's seat.
        use_only_deposited_funds: bool,
        /// Expire after this slot.
        last_valid_slot: Option<u64>,
        /// Expire after this Unix second.
        last_valid_unix_timestamp: Option<u64>,
        /// Skip, rather than fail, when funds are short.
        fail_silently_on_insufficient_funds: bool,
    },
    /// Takes what crosses now; nothing rests.
    ImmediateOrCancel {
        /// Book side (an ask sells the base asset).
        side: BookSide,
        /// The worst price to trade at, in ticks; `None` for any.
        price_in_ticks: Option<u64>,
        /// Base lots to trade at most (0 when sized in quote lots).
        num_base_lots: u64,
        /// Quote lots to trade at most (0 when sized in base lots).
        num_quote_lots: u64,
        /// Fail unless at least this many base lots trade.
        min_base_lots_to_fill: u64,
        /// Fail unless at least this many quote lots trade.
        min_quote_lots_to_fill: u64,
        /// Matching the trader's own orders.
        self_trade: SelfTrade,
        /// The most resting orders to match.
        match_limit: Option<u64>,
        /// The trader's own id for the order.
        client_order_id: u128,
        /// Trade only with funds already on the trader's seat.
        use_only_deposited_funds: bool,
        /// Expire after this slot.
        last_valid_slot: Option<u64>,
        /// Expire after this Unix second.
        last_valid_unix_timestamp: Option<u64>,
    },
}

impl OrderPacket {
    /// An immediate-or-cancel order of `num_base_lots` at no worse than
    /// `price_in_ticks`, with every other field at its default.
    pub fn ioc(side: BookSide, price_in_ticks: Option<u64>, num_base_lots: u64) -> Self {
        Self::ImmediateOrCancel {
            side,
            price_in_ticks,
            num_base_lots,
            num_quote_lots: 0,
            min_base_lots_to_fill: 0,
            min_quote_lots_to_fill: 0,
            self_trade: SelfTrade::default(),
            match_limit: None,
            client_order_id: 0,
            use_only_deposited_funds: false,
            last_valid_slot: None,
            last_valid_unix_timestamp: None,
        }
    }

    /// A post-only order of `num_base_lots` at `price_in_ticks`, with every
    /// other field at its default (refused if it would cross, no expiry).
    pub fn post_only(side: BookSide, price_in_ticks: u64, num_base_lots: u64) -> Self {
        Self::PostOnly {
            side,
            price_in_ticks,
            num_base_lots,
            client_order_id: 0,
            reject_post_only: true,
            use_only_deposited_funds: false,
            last_valid_slot: None,
            last_valid_unix_timestamp: None,
            fail_silently_on_insufficient_funds: false,
        }
    }

    /// A limit order of `num_base_lots` at `price_in_ticks`, with every
    /// other field at its default.
    pub fn limit(side: BookSide, price_in_ticks: u64, num_base_lots: u64) -> Self {
        Self::Limit {
            side,
            price_in_ticks,
            num_base_lots,
            self_trade: SelfTrade::default(),
            match_limit: None,
            client_order_id: 0,
            use_only_deposited_funds: false,
            last_valid_slot: None,
            last_valid_unix_timestamp: None,
            fail_silently_on_insufficient_funds: false,
        }
    }

    /// The order's book side.
    pub fn side(&self) -> BookSide {
        match *self {
            Self::PostOnly { side, .. }
            | Self::Limit { side, .. }
            | Self::ImmediateOrCancel { side, .. } => side,
        }
    }

    /// The packet's Borsh encoding.
    pub fn encode(&self) -> Vec<u8> {
        let mut o = Vec::with_capacity(80);
        match *self {
            Self::PostOnly {
                side,
                price_in_ticks,
                num_base_lots,
                client_order_id,
                reject_post_only,
                use_only_deposited_funds,
                last_valid_slot,
                last_valid_unix_timestamp,
                fail_silently_on_insufficient_funds,
            } => {
                o.extend_from_slice(&[0, side_tag(side)]);
                o.extend_from_slice(&price_in_ticks.to_le_bytes());
                o.extend_from_slice(&num_base_lots.to_le_bytes());
                o.extend_from_slice(&client_order_id.to_le_bytes());
                o.extend_from_slice(&[
                    u8::from(reject_post_only),
                    u8::from(use_only_deposited_funds),
                ]);
                opt_u64(&mut o, last_valid_slot);
                opt_u64(&mut o, last_valid_unix_timestamp);
                o.push(u8::from(fail_silently_on_insufficient_funds));
            }
            Self::Limit {
                side,
                price_in_ticks,
                num_base_lots,
                self_trade,
                match_limit,
                client_order_id,
                use_only_deposited_funds,
                last_valid_slot,
                last_valid_unix_timestamp,
                fail_silently_on_insufficient_funds,
            } => {
                o.extend_from_slice(&[1, side_tag(side)]);
                o.extend_from_slice(&price_in_ticks.to_le_bytes());
                o.extend_from_slice(&num_base_lots.to_le_bytes());
                o.push(self_trade.tag());
                opt_u64(&mut o, match_limit);
                o.extend_from_slice(&client_order_id.to_le_bytes());
                o.push(u8::from(use_only_deposited_funds));
                opt_u64(&mut o, last_valid_slot);
                opt_u64(&mut o, last_valid_unix_timestamp);
                o.push(u8::from(fail_silently_on_insufficient_funds));
            }
            Self::ImmediateOrCancel {
                side,
                price_in_ticks,
                num_base_lots,
                num_quote_lots,
                min_base_lots_to_fill,
                min_quote_lots_to_fill,
                self_trade,
                match_limit,
                client_order_id,
                use_only_deposited_funds,
                last_valid_slot,
                last_valid_unix_timestamp,
            } => {
                o.extend_from_slice(&[2, side_tag(side)]);
                opt_u64(&mut o, price_in_ticks);
                for v in [
                    num_base_lots,
                    num_quote_lots,
                    min_base_lots_to_fill,
                    min_quote_lots_to_fill,
                ] {
                    o.extend_from_slice(&v.to_le_bytes());
                }
                o.push(self_trade.tag());
                opt_u64(&mut o, match_limit);
                o.extend_from_slice(&client_order_id.to_le_bytes());
                o.push(u8::from(use_only_deposited_funds));
                opt_u64(&mut o, last_valid_slot);
                opt_u64(&mut o, last_valid_unix_timestamp);
            }
        }
        o
    }
}

/// A post-only order in a batch: price in ticks, base lots and expiry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CondensedOrder {
    /// Price, in ticks.
    pub price_in_ticks: u64,
    /// Size, in base lots.
    pub size_in_base_lots: u64,
    /// Expire after this slot.
    pub last_valid_slot: Option<u64>,
    /// Expire after this Unix second.
    pub last_valid_unix_timestamp: Option<u64>,
}

/// What a batch of post-only orders does with an order it cannot place.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FailedMultipleLimitOrderBehavior {
    /// Fail the instruction on any order it cannot fund or that would cross.
    FailOnInsufficientFundsAndAmendOnCross,
    /// Fail on a crossing order, skip one it cannot fund.
    FailOnInsufficientFundsAndFailOnCross,
    /// Skip one it cannot fund, move a crossing one to the best price that
    /// does not cross.
    #[default]
    SkipOnInsufficientFundsAndAmendOnCross,
    /// Skip one it cannot fund, fail on a crossing one.
    SkipOnInsufficientFundsAndFailOnCross,
}

impl FailedMultipleLimitOrderBehavior {
    fn tag(self) -> u8 {
        match self {
            Self::FailOnInsufficientFundsAndAmendOnCross => 0,
            Self::FailOnInsufficientFundsAndFailOnCross => 1,
            Self::SkipOnInsufficientFundsAndAmendOnCross => 2,
            Self::SkipOnInsufficientFundsAndFailOnCross => 3,
        }
    }
}

fn ro(key: Pubkey) -> AccountMeta {
    AccountMeta::new_readonly(key, false)
}

fn rw(key: Pubkey) -> AccountMeta {
    AccountMeta::new(key, false)
}

fn head(m: &MarketKeys, trader: &Pubkey) -> Vec<AccountMeta> {
    vec![
        ro(PROGRAM_ID),
        ro(log_authority()),
        rw(m.market),
        AccountMeta::new_readonly(*trader, true),
    ]
}

fn funds(m: &MarketKeys, trader: &Pubkey) -> [AccountMeta; 5] {
    [
        rw(ata(trader, &m.base_mint)),
        rw(ata(trader, &m.quote_mint)),
        rw(m.base_vault),
        rw(m.quote_vault),
        ro(token_program()),
    ]
}

fn instruction(accounts: Vec<AccountMeta>, tag: u8, body: &[u8]) -> Instruction {
    let mut data = Vec::with_capacity(1 + body.len());
    data.push(tag);
    data.extend_from_slice(body);
    Instruction {
        program_id: PROGRAM_ID,
        accounts,
        data,
    }
}

/// Swap: an immediate-or-cancel order settled straight from and to the
/// trader's associated token accounts. Needs no seat.
pub fn swap(m: &MarketKeys, trader: &Pubkey, packet: &OrderPacket) -> Instruction {
    let mut accounts = head(m, trader);
    accounts.extend(funds(m, trader));
    instruction(accounts, 0, &packet.encode())
}

/// PlaceLimitOrder: an order funded from the trader's token accounts (and
/// its seat's free funds); needs a seat.
pub fn place_limit_order(m: &MarketKeys, trader: &Pubkey, packet: &OrderPacket) -> Instruction {
    let mut accounts = head(m, trader);
    accounts.push(ro(seat(&m.market, trader)));
    accounts.extend(funds(m, trader));
    instruction(accounts, 2, &packet.encode())
}

/// PlaceLimitOrderWithFreeFunds: an order funded only from the trader's
/// seat.
pub fn place_limit_order_with_free_funds(
    m: &MarketKeys,
    trader: &Pubkey,
    packet: &OrderPacket,
) -> Instruction {
    let mut accounts = head(m, trader);
    accounts.push(ro(seat(&m.market, trader)));
    instruction(accounts, 3, &packet.encode())
}

/// CancelAllOrders: cancels every resting order of the trader and sends
/// the freed funds to its token accounts.
pub fn cancel_all_orders(m: &MarketKeys, trader: &Pubkey) -> Instruction {
    let mut accounts = head(m, trader);
    accounts.extend(funds(m, trader));
    instruction(accounts, 6, &[])
}

/// CancelAllOrdersWithFreeFunds: cancels every resting order; the funds
/// stay on the seat.
pub fn cancel_all_orders_with_free_funds(m: &MarketKeys, trader: &Pubkey) -> Instruction {
    instruction(head(m, trader), 7, &[])
}

/// WithdrawFunds: free funds from the seat to the trader's token accounts;
/// `None` withdraws all of that asset.
pub fn withdraw_funds(
    m: &MarketKeys,
    trader: &Pubkey,
    quote_lots: Option<u64>,
    base_lots: Option<u64>,
) -> Instruction {
    let mut accounts = head(m, trader);
    accounts.extend(funds(m, trader));
    let mut body = Vec::with_capacity(18);
    opt_u64(&mut body, quote_lots);
    opt_u64(&mut body, base_lots);
    instruction(accounts, 12, &body)
}

/// DepositFunds: from the trader's token accounts to its seat.
pub fn deposit_funds(
    m: &MarketKeys,
    trader: &Pubkey,
    quote_lots: u64,
    base_lots: u64,
) -> Instruction {
    let mut accounts = head(m, trader);
    accounts.push(ro(seat(&m.market, trader)));
    accounts.extend(funds(m, trader));
    let mut body = quote_lots.to_le_bytes().to_vec();
    body.extend_from_slice(&base_lots.to_le_bytes());
    instruction(accounts, 13, &body)
}

/// PlaceMultiplePostOnlyOrders (or its free-funds form): a batch of bids
/// and asks in one instruction.
pub fn place_multiple_post_only_orders(
    m: &MarketKeys,
    trader: &Pubkey,
    bids: &[CondensedOrder],
    asks: &[CondensedOrder],
    client_order_id: Option<u128>,
    failed: FailedMultipleLimitOrderBehavior,
    with_free_funds: bool,
) -> Instruction {
    let mut body = Vec::new();
    for orders in [bids, asks] {
        body.extend_from_slice(&(orders.len() as u32).to_le_bytes());
        for o in orders {
            body.extend_from_slice(&o.price_in_ticks.to_le_bytes());
            body.extend_from_slice(&o.size_in_base_lots.to_le_bytes());
            opt_u64(&mut body, o.last_valid_slot);
            opt_u64(&mut body, o.last_valid_unix_timestamp);
        }
    }
    match client_order_id {
        None => body.push(0),
        Some(id) => {
            body.push(1);
            body.extend_from_slice(&id.to_le_bytes());
        }
    }
    body.push(failed.tag());
    let mut accounts = head(m, trader);
    accounts.push(ro(seat(&m.market, trader)));
    if with_free_funds {
        instruction(accounts, 17, &body)
    } else {
        accounts.extend(funds(m, trader));
        instruction(accounts, 16, &body)
    }
}

/// The seat manager's ClaimSeat: a seat on the market for `trader`, who
/// pays its deposit. Permissionless.
pub fn claim_seat(m: &MarketKeys, trader: &Pubkey) -> Instruction {
    let market = m.market;
    Instruction {
        program_id: SEAT_MANAGER_ID,
        accounts: vec![
            ro(PROGRAM_ID),
            ro(log_authority()),
            rw(market),
            rw(seat_manager(&market)),
            rw(seat_deposit_collector(&market)),
            AccountMeta::new_readonly(*trader, true),
            AccountMeta::new(*trader, true),
            rw(seat(&market, trader)),
            ro(solana_system_interface::program::id()),
        ],
        data: vec![1],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys() -> MarketKeys {
        MarketKeys {
            market: Pubkey::new_from_array([1; 32]),
            base_mint: Pubkey::new_from_array([2; 32]),
            quote_mint: Pubkey::new_from_array([3; 32]),
            base_vault: Pubkey::new_from_array([4; 32]),
            quote_vault: Pubkey::new_from_array([5; 32]),
        }
    }

    #[test]
    fn packets_encode_as_phoenix_borsh() {
        let ioc = OrderPacket::ioc(BookSide::Ask, Some(150_000), 2_500).encode();
        assert_eq!(&ioc[..3], &[2, 1, 1]);
        assert_eq!(u64::from_le_bytes(ioc[3..11].try_into().unwrap()), 150_000);
        assert_eq!(u64::from_le_bytes(ioc[11..19].try_into().unwrap()), 2_500);
        // four u64 sizes, self trade, match limit (none), client id, deposited funds, two expiries (none)
        assert_eq!(ioc.len(), 3 + 8 + 32 + 1 + 1 + 16 + 1 + 1 + 1);
        assert_eq!(ioc[43], 2, "decrement take");

        let post = OrderPacket::PostOnly {
            side: BookSide::Ask,
            price_in_ticks: 7,
            num_base_lots: 9,
            client_order_id: 0,
            reject_post_only: false,
            use_only_deposited_funds: false,
            last_valid_slot: Some(1_000),
            last_valid_unix_timestamp: None,
            fail_silently_on_insufficient_funds: false,
        }
        .encode();
        assert_eq!(&post[..2], &[0, 1]);
        assert_eq!(post[34], 0, "amend a crossing order");
        assert_eq!(&post[36..45], &[1, 0xe8, 3, 0, 0, 0, 0, 0, 0]);
        assert_eq!(post.len(), 2 + 16 + 16 + 2 + 9 + 1 + 1);

        let limit = OrderPacket::limit(BookSide::Bid, 3, 4).encode();
        assert_eq!(&limit[..2], &[1, 0]);
        assert_eq!(limit.len(), 2 + 16 + 1 + 1 + 16 + 1 + 1 + 1 + 1);
    }

    #[test]
    fn instructions_carry_the_documented_accounts() {
        let m = keys();
        let trader = Pubkey::new_from_array([9; 32]);
        let packet = OrderPacket::ioc(BookSide::Ask, None, 1);
        let swap = swap(&m, &trader, &packet);
        assert_eq!(swap.program_id, PROGRAM_ID);
        assert_eq!(swap.accounts.len(), 9);
        assert_eq!(swap.data[0], 0);
        assert!(swap.accounts[3].is_signer && !swap.accounts[3].is_writable);
        assert_eq!(swap.accounts[4].pubkey, ata(&trader, &m.base_mint));

        let place = place_limit_order(&m, &trader, &OrderPacket::post_only(BookSide::Ask, 1, 1));
        assert_eq!(place.accounts[4].pubkey, seat(&m.market, &trader));
        assert_eq!(place.accounts.len(), 10);
        assert_eq!(cancel_all_orders(&m, &trader).data, vec![6]);
        assert_eq!(
            cancel_all_orders_with_free_funds(&m, &trader)
                .accounts
                .len(),
            4
        );
        assert_eq!(
            withdraw_funds(&m, &trader, None, Some(5)).data,
            vec![12, 0, 1, 5, 0, 0, 0, 0, 0, 0, 0]
        );

        let batch = place_multiple_post_only_orders(
            &m,
            &trader,
            &[],
            &[CondensedOrder {
                price_in_ticks: 10,
                size_in_base_lots: 20,
                last_valid_slot: None,
                last_valid_unix_timestamp: None,
            }],
            None,
            FailedMultipleLimitOrderBehavior::default(),
            true,
        );
        assert_eq!(batch.data[0], 17);
        assert_eq!(&batch.data[1..9], &[0, 0, 0, 0, 1, 0, 0, 0]);
        assert_eq!(*batch.data.last().unwrap(), 2);

        let claim = claim_seat(&m, &trader);
        assert_eq!(claim.program_id, SEAT_MANAGER_ID);
        assert_eq!(claim.data, vec![1]);
        assert!(claim.accounts[6].is_writable && claim.accounts[6].is_signer);
    }
}
