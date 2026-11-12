//! The Phoenix v1 market account: its header, its two order trees and its
//! seats.
//!
//! One account holds a whole market:
//!
//! ```text
//!  MarketHeader (576 bytes)  status, tree sizes, base and quote mints, vaults, lot and tick sizes
//!  padding (256)
//!  FIFOMarket                base lots per base unit, tick size in quote lots, order sequence
//!                            number, taker fee, then three red-black trees:
//!    bids   (32 + 64 × bids_size)     price-time ordered resting bids
//!    asks   (32 + 64 × asks_size)     resting asks
//!    seats  (32 + 144 × num_seats)    each trader's free and locked funds
//! ```
//!
//! A tree is a 32-byte header (its root's node index first) and an array of
//! nodes, indexed from 1; each node starts with its left and right
//! children's indices. An order's trader is the index of the trader's node
//! in the seat tree.

use solana_pubkey::Pubkey;

use crate::protocols::DecodeError;

/// Size of the market header.
pub const HEADER_SIZE: usize = 576;
/// Where the FIFO market starts: the header, then 256 bytes of padding.
const FIFO_OFFSET: usize = HEADER_SIZE + 256;
/// Where the bid tree starts.
const BIDS_OFFSET: usize = FIFO_OFFSET + 48;
const ORDER_NODE: usize = 64;
const SEAT_NODE: usize = 144;
const TREE_HEADER: usize = 32;

/// Which book an order rests in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BookSide {
    /// Orders to buy the base asset.
    Bid,
    /// Orders to sell it.
    Ask,
}

/// The market's fixed parameters, from its header and the FIFO market's
/// first fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarketHeader {
    /// Trading status (1: active).
    pub status: u64,
    /// Capacity of the bid tree.
    pub bids_size: u64,
    /// Capacity of the ask tree.
    pub asks_size: u64,
    /// Capacity of the seat tree.
    pub num_seats: u64,
    /// The base mint's decimals.
    pub base_decimals: u32,
    /// The base mint.
    pub base_mint: Pubkey,
    /// The market's base vault.
    pub base_vault: Pubkey,
    /// Base atoms per base lot.
    pub base_lot_size: u64,
    /// The quote mint's decimals.
    pub quote_decimals: u32,
    /// The quote mint.
    pub quote_mint: Pubkey,
    /// The market's quote vault.
    pub quote_vault: Pubkey,
    /// Quote atoms per quote lot.
    pub quote_lot_size: u64,
    /// One tick, in quote atoms per base unit.
    pub tick_size_in_quote_atoms_per_base_unit: u64,
    /// The market authority.
    pub authority: Pubkey,
    /// Raw base units per base unit (1 for an ordinary market).
    pub raw_base_units_per_base_unit: u32,
    /// Base lots per base unit.
    pub base_lots_per_base_unit: u64,
    /// One tick, in quote lots per base unit.
    pub tick_size_in_quote_lots_per_base_unit: u64,
    /// The sequence number the next order gets.
    pub order_sequence_number: u64,
    /// The taker fee, in basis points.
    pub taker_fee_bps: u64,
}

/// An order resting on the book.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RestingOrder {
    /// The book it rests in.
    pub side: BookSide,
    /// Its price, in ticks.
    pub price_in_ticks: u64,
    /// Its sequence number, as the tree's key stores it. A new number every
    /// time an order is placed: an order that keeps its number has rested.
    pub sequence_number: u64,
    /// The node index of its trader's seat.
    pub trader_index: u64,
    /// What is left of it, in base lots.
    pub base_lots: u64,
    /// The last slot it is valid in (0: no expiry).
    pub last_valid_slot: u64,
    /// The last Unix second it is valid in (0: no expiry).
    pub last_valid_unix_timestamp: u64,
}

impl RestingOrder {
    /// Whether the order still counts at `slot` and `unix_timestamp`.
    pub fn is_live(&self, slot: u64, unix_timestamp: u64) -> bool {
        (self.last_valid_slot == 0 || self.last_valid_slot >= slot)
            && (self.last_valid_unix_timestamp == 0
                || self.last_valid_unix_timestamp >= unix_timestamp)
    }
}

/// A trader's seat: its `TraderState`, the funds it keeps on the market.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TraderState {
    /// The seat's node index, which its orders refer to.
    pub index: u32,
    /// The trader.
    pub trader: Pubkey,
    /// Quote lots held by its resting bids.
    pub quote_lots_locked: u64,
    /// Quote lots it may withdraw or trade.
    pub quote_lots_free: u64,
    /// Base lots held by its resting asks.
    pub base_lots_locked: u64,
    /// Base lots it may withdraw or trade.
    pub base_lots_free: u64,
}

/// A decoded market account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarketState {
    /// The market's parameters.
    pub header: MarketHeader,
    /// Resting bids, in the tree's order (best first).
    pub bids: Vec<RestingOrder>,
    /// Resting asks, in the tree's order (best first).
    pub asks: Vec<RestingOrder>,
    /// Every seat, in the tree's order.
    pub seats: Vec<TraderState>,
}

fn decode_error(detail: impl Into<String>) -> DecodeError {
    DecodeError {
        what: "phoenix v1 market",
        detail: detail.into(),
    }
}

struct Bytes<'a>(&'a [u8]);

impl Bytes<'_> {
    fn array<const N: usize>(&self, at: usize) -> Result<[u8; N], DecodeError> {
        self.0
            .get(at..at + N)
            .and_then(|b| b.try_into().ok())
            .ok_or_else(|| decode_error(format!("account ends before byte {}", at + N)))
    }

    fn u32(&self, at: usize) -> Result<u32, DecodeError> {
        Ok(u32::from_le_bytes(self.array(at)?))
    }

    fn u64(&self, at: usize) -> Result<u64, DecodeError> {
        Ok(u64::from_le_bytes(self.array(at)?))
    }

    fn key(&self, at: usize) -> Result<Pubkey, DecodeError> {
        Ok(Pubkey::new_from_array(self.array(at)?))
    }

    /// The nodes of the tree at `offset`, in order: `(byte offset, index)`.
    fn tree(
        &self,
        offset: usize,
        node: usize,
        capacity: u64,
    ) -> Result<Vec<(usize, u32)>, DecodeError> {
        let at = |i: u32| offset + TREE_HEADER + (i as usize - 1) * node;
        let mut out = Vec::new();
        let mut stack: Vec<u32> = Vec::new();
        let mut cur = self.u32(offset)?;
        // Every node is pushed once in an in-order walk; more steps than
        // that is a cycle.
        let mut budget = 4 * capacity + 8;
        while cur != 0 || !stack.is_empty() {
            while cur != 0 {
                if cur as u64 > capacity {
                    return Err(decode_error(format!(
                        "node {cur} beyond the tree's {capacity}"
                    )));
                }
                budget = budget
                    .checked_sub(1)
                    .ok_or_else(|| decode_error("a tree walk did not terminate"))?;
                stack.push(cur);
                cur = self.u32(at(cur))?;
            }
            let node_index = stack.pop().expect("the loop pushed one");
            out.push((at(node_index), node_index));
            cur = self.u32(at(node_index) + 4)?;
        }
        Ok(out)
    }
}

impl MarketHeader {
    /// Decodes the header and the FIFO market's parameters.
    pub fn decode(data: &[u8]) -> Result<Self, DecodeError> {
        let d = Bytes(data);
        if data.len() < BIDS_OFFSET {
            return Err(decode_error("too short for a market"));
        }
        Ok(Self {
            status: d.u64(8)?,
            bids_size: d.u64(16)?,
            asks_size: d.u64(24)?,
            num_seats: d.u64(32)?,
            base_decimals: d.u32(40)?,
            base_mint: d.key(48)?,
            base_vault: d.key(80)?,
            base_lot_size: d.u64(112)?,
            quote_decimals: d.u32(120)?,
            quote_mint: d.key(128)?,
            quote_vault: d.key(160)?,
            quote_lot_size: d.u64(192)?,
            tick_size_in_quote_atoms_per_base_unit: d.u64(200)?,
            authority: d.key(208)?,
            raw_base_units_per_base_unit: d.u32(312)?,
            base_lots_per_base_unit: d.u64(FIFO_OFFSET)?,
            tick_size_in_quote_lots_per_base_unit: d.u64(FIFO_OFFSET + 8)?,
            order_sequence_number: d.u64(FIFO_OFFSET + 16)?,
            taker_fee_bps: d.u64(FIFO_OFFSET + 24)?,
        })
    }
}

impl MarketState {
    /// Decodes a whole market account.
    pub fn decode(data: &[u8]) -> Result<Self, DecodeError> {
        let header = MarketHeader::decode(data)?;
        let to_usize =
            |v: u64| usize::try_from(v).map_err(|_| decode_error("tree size out of range"));
        let asks_offset = BIDS_OFFSET + TREE_HEADER + to_usize(header.bids_size)? * ORDER_NODE;
        let seats_offset = asks_offset + TREE_HEADER + to_usize(header.asks_size)? * ORDER_NODE;
        let end = seats_offset + TREE_HEADER + to_usize(header.num_seats)? * SEAT_NODE;
        if end != data.len() {
            return Err(decode_error(format!(
                "{} bytes, but its size parameters make {end}",
                data.len()
            )));
        }
        let d = Bytes(data);
        let orders = |offset: usize,
                      capacity: u64,
                      side: BookSide|
         -> Result<Vec<RestingOrder>, DecodeError> {
            d.tree(offset, ORDER_NODE, capacity)?
                .into_iter()
                .map(|(p, _)| {
                    Ok(RestingOrder {
                        side,
                        price_in_ticks: d.u64(p + 16)?,
                        sequence_number: d.u64(p + 24)?,
                        trader_index: d.u64(p + 32)?,
                        base_lots: d.u64(p + 40)?,
                        last_valid_slot: d.u64(p + 48)?,
                        last_valid_unix_timestamp: d.u64(p + 56)?,
                    })
                })
                .collect()
        };
        let bids = orders(BIDS_OFFSET, header.bids_size, BookSide::Bid)?;
        let asks = orders(asks_offset, header.asks_size, BookSide::Ask)?;
        let seats = d
            .tree(seats_offset, SEAT_NODE, header.num_seats)?
            .into_iter()
            .map(|(p, index)| {
                Ok(TraderState {
                    index,
                    trader: d.key(p + 16)?,
                    quote_lots_locked: d.u64(p + 48)?,
                    quote_lots_free: d.u64(p + 56)?,
                    base_lots_locked: d.u64(p + 64)?,
                    base_lots_free: d.u64(p + 72)?,
                })
            })
            .collect::<Result<_, DecodeError>>()?;
        Ok(Self {
            header,
            bids,
            asks,
            seats,
        })
    }

    /// The seat of `trader`, if it has one.
    pub fn seat(&self, trader: &Pubkey) -> Option<&TraderState> {
        self.seats.iter().find(|s| &s.trader == trader)
    }

    /// The trader whose seat has node index `index`.
    pub fn trader_at(&self, index: u64) -> Option<Pubkey> {
        self.seats
            .iter()
            .find(|s| u64::from(s.index) == index)
            .map(|s| s.trader)
    }

    /// The resting orders on one side.
    pub fn orders(&self, side: BookSide) -> &[RestingOrder] {
        match side {
            BookSide::Bid => &self.bids,
            BookSide::Ask => &self.asks,
        }
    }
}

#[cfg(test)]
pub(crate) mod testing {
    //! A writer for small market accounts, for tests.

    use super::*;

    /// One order to write: `(price_in_ticks, sequence_number, seat index, base lots)`.
    pub(crate) type OrderSpec = (u64, u64, u64, u64);

    pub(crate) struct MarketSpec {
        pub base_mint: Pubkey,
        pub quote_mint: Pubkey,
        pub base_vault: Pubkey,
        pub quote_vault: Pubkey,
        pub bids: Vec<OrderSpec>,
        pub asks: Vec<OrderSpec>,
        /// `(trader, base lots free, base lots locked, quote lots free, quote lots locked)`.
        pub seats: Vec<(Pubkey, u64, u64, u64, u64)>,
    }

    impl Default for MarketSpec {
        fn default() -> Self {
            Self {
                base_mint: Pubkey::new_from_array([1; 32]),
                quote_mint: Pubkey::new_from_array([2; 32]),
                base_vault: Pubkey::new_from_array([3; 32]),
                quote_vault: Pubkey::new_from_array([4; 32]),
                bids: vec![],
                asks: vec![],
                seats: vec![],
            }
        }
    }

    const CAPACITY: usize = 8;

    fn put(data: &mut [u8], at: usize, bytes: &[u8]) {
        data[at..at + bytes.len()].copy_from_slice(bytes);
    }

    /// Writes nodes `1..=n` as a right-leaning chain: an in-order walk
    /// visits them in index order.
    fn chain(data: &mut [u8], offset: usize, node: usize, n: usize) {
        put(data, offset, &(if n > 0 { 1u32 } else { 0 }).to_le_bytes());
        for i in 1..=n {
            let at = offset + TREE_HEADER + (i - 1) * node;
            let right = if i < n { i as u32 + 1 } else { 0 };
            put(data, at + 4, &right.to_le_bytes());
        }
    }

    /// A SOL/USDC-shaped market: 9 and 6 decimals, 0.001 SOL lots, a 0.001
    /// USDC tick, 2 bps taker fee; trees of eight nodes.
    pub(crate) fn market_bytes(spec: &MarketSpec) -> Vec<u8> {
        let bids = BIDS_OFFSET;
        let asks = bids + TREE_HEADER + CAPACITY * ORDER_NODE;
        let seats = asks + TREE_HEADER + CAPACITY * ORDER_NODE;
        let mut d = vec![0u8; seats + TREE_HEADER + CAPACITY * SEAT_NODE];
        put(&mut d, 8, &1u64.to_le_bytes());
        for at in [16, 24, 32] {
            put(&mut d, at, &(CAPACITY as u64).to_le_bytes());
        }
        put(&mut d, 40, &9u32.to_le_bytes());
        put(&mut d, 48, spec.base_mint.as_ref());
        put(&mut d, 80, spec.base_vault.as_ref());
        put(&mut d, 112, &1_000_000u64.to_le_bytes());
        put(&mut d, 120, &6u32.to_le_bytes());
        put(&mut d, 128, spec.quote_mint.as_ref());
        put(&mut d, 160, spec.quote_vault.as_ref());
        put(&mut d, 192, &1u64.to_le_bytes());
        put(&mut d, 200, &1_000u64.to_le_bytes());
        put(&mut d, 312, &1u32.to_le_bytes());
        put(&mut d, FIFO_OFFSET, &1_000u64.to_le_bytes());
        put(&mut d, FIFO_OFFSET + 8, &1_000u64.to_le_bytes());
        put(&mut d, FIFO_OFFSET + 16, &42u64.to_le_bytes());
        put(&mut d, FIFO_OFFSET + 24, &2u64.to_le_bytes());
        for (offset, orders) in [(bids, &spec.bids), (asks, &spec.asks)] {
            chain(&mut d, offset, ORDER_NODE, orders.len());
            for (i, (price, seq, trader, lots)) in orders.iter().enumerate() {
                let at = offset + TREE_HEADER + i * ORDER_NODE;
                put(&mut d, at + 16, &price.to_le_bytes());
                put(&mut d, at + 24, &seq.to_le_bytes());
                put(&mut d, at + 32, &trader.to_le_bytes());
                put(&mut d, at + 40, &lots.to_le_bytes());
            }
        }
        chain(&mut d, seats, SEAT_NODE, spec.seats.len());
        for (i, (trader, base_free, base_locked, quote_free, quote_locked)) in
            spec.seats.iter().enumerate()
        {
            let at = seats + TREE_HEADER + i * SEAT_NODE;
            put(&mut d, at + 16, trader.as_ref());
            put(&mut d, at + 48, &quote_locked.to_le_bytes());
            put(&mut d, at + 56, &quote_free.to_le_bytes());
            put(&mut d, at + 64, &base_locked.to_le_bytes());
            put(&mut d, at + 72, &base_free.to_le_bytes());
        }
        d
    }
}

#[cfg(test)]
mod tests {
    use super::{testing::*, *};

    #[test]
    fn decodes_header_orders_and_seats() {
        let maker = Pubkey::new_from_array([8; 32]);
        let other = Pubkey::new_from_array([9; 32]);
        let data = market_bytes(&MarketSpec {
            bids: vec![(150_000, 7, 1, 2_000), (149_990, 3, 2, 500)],
            asks: vec![(150_020, 5, 1, 1_000)],
            seats: vec![
                (maker, 10, 1_000, 20, 300_000),
                (other, 0, 0, 0, 74_995_000),
            ],
            ..MarketSpec::default()
        });
        let m = MarketState::decode(&data).unwrap();
        assert_eq!(m.header.base_decimals, 9);
        assert_eq!(m.header.base_lots_per_base_unit, 1_000);
        assert_eq!(m.header.taker_fee_bps, 2);
        assert_eq!(m.header.order_sequence_number, 42);
        assert_eq!(m.bids.len(), 2);
        assert_eq!(m.bids[0].price_in_ticks, 150_000);
        assert_eq!(m.bids[1].trader_index, 2);
        assert_eq!(m.asks[0].side, BookSide::Ask);
        assert_eq!(m.asks[0].sequence_number, 5);
        assert_eq!(m.seats.len(), 2);
        assert_eq!(m.seat(&maker).unwrap().base_lots_locked, 1_000);
        assert_eq!(m.trader_at(2), Some(other));
        assert_eq!(m.trader_at(3), None);
        assert!(m.bids[0].is_live(100, 100));
    }

    #[test]
    fn rejects_a_size_mismatch_and_a_cycle() {
        let data = market_bytes(&MarketSpec::default());
        assert!(MarketState::decode(&data[..data.len() - 1]).is_err());

        let mut cyclic = market_bytes(&MarketSpec {
            bids: vec![(1, 1, 1, 1), (2, 2, 1, 1)],
            ..MarketSpec::default()
        });
        // node 2's right child back to node 1
        let at = BIDS_OFFSET + TREE_HEADER + ORDER_NODE + 4;
        cyclic[at..at + 4].copy_from_slice(&1u32.to_le_bytes());
        assert!(MarketState::decode(&cyclic).is_err());
    }

    #[test]
    fn expiry_is_by_slot_or_time() {
        let o = RestingOrder {
            side: BookSide::Ask,
            price_in_ticks: 1,
            sequence_number: 1,
            trader_index: 1,
            base_lots: 1,
            last_valid_slot: 10,
            last_valid_unix_timestamp: 0,
        };
        assert!(o.is_live(10, 99));
        assert!(!o.is_live(11, 99));
    }

    /// Mainnet's SOL-USDC market, `4DoNfFBfF7UokCC2FQzriy7yHK6DY6NVdYpuekQ5pRgg`,
    /// as read at slot 467582217: set `MTX_PHOENIX_V1_MARKET` to a dump of
    /// its account data (1,723,488 bytes). The expected values were read
    /// from the same dump by an independent decoder.
    #[test]
    #[ignore = "needs a dump of the mainnet market account"]
    fn decodes_mainnets_sol_usdc_market() {
        let data = std::fs::read(std::env::var("MTX_PHOENIX_V1_MARKET").unwrap()).unwrap();
        let m = MarketState::decode(&data).unwrap();
        let h = &m.header;
        assert_eq!((h.bids_size, h.asks_size, h.num_seats), (4096, 4096, 8321));
        assert_eq!((h.base_decimals, h.quote_decimals), (9, 6));
        assert_eq!((h.base_lot_size, h.quote_lot_size), (1_000_000, 1));
        assert_eq!(m.seats.len(), 6982);
        assert_eq!((m.bids.len(), m.asks.len()), (1278, 301));
        let (bid, ask) = (&m.bids[0], &m.asks[0]);
        assert_eq!(
            (bid.price_in_ticks, bid.trader_index, bid.base_lots),
            (70535, 650, 19850)
        );
        assert_eq!(
            (ask.price_in_ticks, ask.trader_index, ask.base_lots),
            (129000, 3352, 800)
        );
        let maker: Pubkey = "AR166mgnJ15pVzjXCB56mHeuz52ER49Cqqg7fiTdwi1i"
            .parse()
            .unwrap();
        let seat = m.seat(&maker).unwrap();
        assert_eq!(
            (
                seat.quote_lots_locked,
                seat.quote_lots_free,
                seat.base_lots_locked,
                seat.base_lots_free
            ),
            (7_886_545_246, 0, 510, 0)
        );
    }
}
