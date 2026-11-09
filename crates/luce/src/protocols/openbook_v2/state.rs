//! OpenBook v2 accounts: the market, its two book sides, and an open
//! orders account.
//!
//! All three are Anchor zero-copy accounts: an 8-byte discriminator, then
//! the struct's fields in declaration order, unpadded.
//!
//! A book side holds its orders in a crit-bit tree. Each leaf's 128-bit key
//! is the order's price in lots (the high 64 bits) and its sequence number
//! (the low 64, bitwise inverted on the bid side, so that earlier orders
//! sort first at a price).

use solana_pubkey::Pubkey;

use crate::chain::ChainError;

/// Size of a `Market` account.
pub const MARKET_SIZE: usize = 8 + 840;
/// Where a `BookSide`'s node array starts: discriminator, two roots, four
/// reserved roots, 256 reserved bytes, the node array's header (16) and its
/// 512 reserved bytes.
const NODES_OFFSET: usize = 8 + 16 + 32 + 256 + 16 + 512;
const NODE_SIZE: usize = 88;
const MAX_NODES: u32 = 1024;
/// Where an `OpenOrdersAccount`'s position starts.
const POSITION_OFFSET: usize = 8 + 32 * 4 + 4 + 1 + 1 + 2;

fn decode_error(what: &'static str, detail: impl Into<String>) -> ChainError {
    ChainError::Decode {
        what,
        detail: detail.into(),
    }
}

struct Bytes<'a>(&'a [u8], &'static str);

impl Bytes<'_> {
    fn array<const N: usize>(&self, at: usize) -> Result<[u8; N], ChainError> {
        self.0
            .get(at..at + N)
            .and_then(|b| b.try_into().ok())
            .ok_or_else(|| decode_error(self.1, format!("account ends before byte {}", at + N)))
    }

    fn u8(&self, at: usize) -> Result<u8, ChainError> {
        Ok(self.array::<1>(at)?[0])
    }

    fn u32(&self, at: usize) -> Result<u32, ChainError> {
        Ok(u32::from_le_bytes(self.array(at)?))
    }

    fn u64(&self, at: usize) -> Result<u64, ChainError> {
        Ok(u64::from_le_bytes(self.array(at)?))
    }

    fn i64(&self, at: usize) -> Result<i64, ChainError> {
        Ok(i64::from_le_bytes(self.array(at)?))
    }

    fn u128(&self, at: usize) -> Result<u128, ChainError> {
        Ok(u128::from_le_bytes(self.array(at)?))
    }

    fn key(&self, at: usize) -> Result<Pubkey, ChainError> {
        Ok(Pubkey::new_from_array(self.array(at)?))
    }

    /// An optional key, stored as the zero key when absent.
    fn optional_key(&self, at: usize) -> Result<Option<Pubkey>, ChainError> {
        let key = self.key(at)?;
        Ok((key != Pubkey::default()).then_some(key))
    }
}

/// A market's accounts and parameters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarketAccount {
    /// The base mint's decimals.
    pub base_decimals: u8,
    /// The quote mint's decimals.
    pub quote_decimals: u8,
    /// The PDA that signs for the vaults.
    pub market_authority: Pubkey,
    /// When the market stops trading, in Unix seconds (0: never).
    pub time_expiry: i64,
    /// Who must co-sign every order, if anyone.
    pub open_orders_admin: Option<Pubkey>,
    /// Who must sign every event consumption, if anyone.
    pub consume_events_admin: Option<Pubkey>,
    /// The market's name, trailing zeros removed.
    pub name: String,
    /// The bid side's account.
    pub bids: Pubkey,
    /// The ask side's account.
    pub asks: Pubkey,
    /// The event heap's account.
    pub event_heap: Pubkey,
    /// The first oracle, if any.
    pub oracle_a: Option<Pubkey>,
    /// The second oracle, if any.
    pub oracle_b: Option<Pubkey>,
    /// Quote atoms per quote lot (a power of ten).
    pub quote_lot_size: i64,
    /// Base atoms per base lot (a power of ten).
    pub base_lot_size: i64,
    /// Orders placed so far.
    pub seq_num: u64,
    /// The maker fee, in millionths of the notional (negative: a rebate).
    pub maker_fee: i64,
    /// The taker fee, in millionths of the notional.
    pub taker_fee: i64,
    /// The base mint.
    pub base_mint: Pubkey,
    /// The quote mint.
    pub quote_mint: Pubkey,
    /// The market's base vault.
    pub base_vault: Pubkey,
    /// The market's quote vault.
    pub quote_vault: Pubkey,
}

impl MarketAccount {
    /// Decodes a `Market` account.
    pub fn decode(data: &[u8]) -> Result<Self, ChainError> {
        if data.len() != MARKET_SIZE {
            return Err(decode_error(
                "openbook v2 market",
                format!("{} bytes, a market has {MARKET_SIZE}", data.len()),
            ));
        }
        let d = Bytes(data, "openbook v2 market");
        let name = d.array::<16>(184)?;
        Ok(Self {
            base_decimals: d.u8(9)?,
            quote_decimals: d.u8(10)?,
            market_authority: d.key(16)?,
            time_expiry: d.i64(48)?,
            open_orders_admin: d.optional_key(88)?,
            consume_events_admin: d.optional_key(120)?,
            name: String::from_utf8_lossy(&name)
                .trim_end_matches('\0')
                .to_string(),
            bids: d.key(200)?,
            asks: d.key(232)?,
            event_heap: d.key(264)?,
            oracle_a: d.optional_key(296)?,
            oracle_b: d.optional_key(328)?,
            quote_lot_size: d.i64(448)?,
            base_lot_size: d.i64(456)?,
            seq_num: d.u64(464)?,
            maker_fee: d.i64(480)?,
            taker_fee: d.i64(488)?,
            base_mint: d.key(576)?,
            quote_mint: d.key(608)?,
            base_vault: d.key(640)?,
            quote_vault: d.key(680)?,
        })
    }
}

/// An order resting in a book side's fixed-price tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LeafOrder {
    /// Price, in price lots.
    pub price_lots: i64,
    /// The key's low 64 bits: the order's sequence number (inverted on the
    /// bid side). With [`timestamp`](Self::timestamp), a new pair every
    /// time an order is placed.
    pub sequence_key: u64,
    /// What is left, in base lots.
    pub base_lots: i64,
    /// The open orders account that placed it.
    pub owner: Pubkey,
    /// The slot in the owner's account the order occupies.
    pub owner_slot: u8,
    /// When it was placed, in Unix seconds.
    pub timestamp: u64,
    /// Seconds it lives after `timestamp` (0: until cancelled).
    pub time_in_force: u16,
    /// The owner's own id for it.
    pub client_order_id: u64,
}

impl LeafOrder {
    /// Whether the order still counts at `unix_timestamp`.
    pub fn is_live(&self, unix_timestamp: u64) -> bool {
        self.time_in_force == 0 || self.timestamp + u64::from(self.time_in_force) > unix_timestamp
    }
}

/// The fixed-price orders of a `BookSide` account, in the tree's order.
pub fn book_side_orders(data: &[u8]) -> Result<Vec<LeafOrder>, ChainError> {
    let d = Bytes(data, "openbook v2 book side");
    if data.len() < NODES_OFFSET + NODE_SIZE * MAX_NODES as usize {
        return Err(decode_error(
            "openbook v2 book side",
            "too short for a book side",
        ));
    }
    let root = d.u32(8)?;
    let leaf_count = d.u32(12)?;
    let mut out = Vec::with_capacity(leaf_count as usize);
    if leaf_count == 0 {
        return Ok(out);
    }
    let mut stack = vec![root];
    // A crit-bit tree with n leaves has 2n − 1 nodes.
    let mut budget = 2 * MAX_NODES as usize;
    while let Some(index) = stack.pop() {
        budget = budget.checked_sub(1).ok_or_else(|| {
            decode_error("openbook v2 book side", "a tree walk did not terminate")
        })?;
        if index >= MAX_NODES {
            return Err(decode_error(
                "openbook v2 book side",
                format!("node {index} out of range"),
            ));
        }
        let at = NODES_OFFSET + index as usize * NODE_SIZE;
        match d.u8(at)? {
            1 => {
                stack.push(d.u32(at + 28)?);
                stack.push(d.u32(at + 24)?);
            }
            2 => {
                let key = d.u128(at + 8)?;
                out.push(LeafOrder {
                    price_lots: (key >> 64) as i64,
                    sequence_key: key as u64,
                    base_lots: d.i64(at + 56)?,
                    owner: d.key(at + 24)?,
                    owner_slot: d.u8(at + 1)?,
                    timestamp: d.u64(at + 64)?,
                    time_in_force: u16::from_le_bytes(d.array(at + 2)?),
                    client_order_id: d.u64(at + 80)?,
                });
            }
            // free nodes are never reachable from the root
            _ => {}
        }
    }
    Ok(out)
}

/// What an open orders account holds on its market.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenOrdersPosition {
    /// The wallet that owns the account.
    pub owner: Pubkey,
    /// The market it trades.
    pub market: Pubkey,
    /// Base lots its resting bids would buy.
    pub bids_base_lots: i64,
    /// Base lots held by its resting asks.
    pub asks_base_lots: i64,
    /// Base atoms it may settle or trade.
    pub base_free_native: u64,
    /// Quote atoms it may settle or trade.
    pub quote_free_native: u64,
    /// Maker fees held against its resting bids, in quote atoms.
    pub locked_maker_fees: u64,
    /// Its cumulative volume as a maker, in quote atoms.
    pub maker_volume: u128,
    /// Its cumulative volume as a taker, in quote atoms.
    pub taker_volume: u128,
    /// Quote lots held by its resting bids.
    pub bids_quote_lots: i64,
}

impl OpenOrdersPosition {
    /// The base it holds, as the OpenBook venue profile measures it:
    /// `base_free_native` + `asks_base_lots` × `base_lot_size`, in base atoms.
    pub fn base_atoms(&self, base_lot_size: i64) -> i128 {
        i128::from(self.base_free_native)
            + i128::from(self.asks_base_lots) * i128::from(base_lot_size)
    }

    /// The quote it holds, as the profile measures it, in quote atoms:
    /// `quote_free_native` plus `bids_quote_lots` × `quote_lot_size` plus
    /// `locked_maker_fees`.
    pub fn quote_atoms(&self, quote_lot_size: i64) -> i128 {
        i128::from(self.quote_free_native)
            + i128::from(self.bids_quote_lots) * i128::from(quote_lot_size)
            + i128::from(self.locked_maker_fees)
    }

    /// Decodes an `OpenOrdersAccount`'s owner, market and position.
    pub fn decode(data: &[u8]) -> Result<Self, ChainError> {
        let d = Bytes(data, "openbook v2 open orders account");
        let p = POSITION_OFFSET;
        Ok(Self {
            owner: d.key(8)?,
            market: d.key(40)?,
            bids_base_lots: d.i64(p)?,
            asks_base_lots: d.i64(p + 8)?,
            base_free_native: d.u64(p + 16)?,
            quote_free_native: d.u64(p + 24)?,
            locked_maker_fees: d.u64(p + 32)?,
            maker_volume: d.u128(p + 56)?,
            taker_volume: d.u128(p + 72)?,
            bids_quote_lots: d.i64(p + 88)?,
        })
    }
}

/// The Anchor discriminator of an `OpenOrdersIndexer` account.
const OPEN_ORDERS_INDEXER_DISCRIMINATOR: [u8; 8] = [195, 83, 128, 213, 204, 91, 19, 150];

/// A wallet's `OpenOrdersIndexer`: every open orders account it owns, on
/// any market.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenOrdersIndexer {
    /// How many open orders accounts the wallet has created; the next one
    /// is numbered one more.
    pub created_counter: u32,
    /// The open orders accounts it owns now.
    pub addresses: Vec<Pubkey>,
}

impl OpenOrdersIndexer {
    /// Decodes an `OpenOrdersIndexer` account.
    pub fn decode(data: &[u8]) -> Result<Self, ChainError> {
        let what = "openbook v2 open orders indexer";
        let d = Bytes(data, what);
        if d.array::<8>(0)? != OPEN_ORDERS_INDEXER_DISCRIMINATOR {
            return Err(decode_error(what, "not an OpenOrdersIndexer account"));
        }
        let created_counter = d.u32(9)?;
        let len = d.u32(13)? as usize;
        let addresses = (0..len)
            .map(|i| d.key(17 + 32 * i))
            .collect::<Result<_, _>>()?;
        Ok(Self {
            created_counter,
            addresses,
        })
    }
}

#[cfg(test)]
pub(crate) mod testing {
    //! Writers for small accounts, for tests.

    use super::*;

    pub(crate) fn put(data: &mut [u8], at: usize, bytes: &[u8]) {
        data[at..at + bytes.len()].copy_from_slice(bytes);
    }

    /// An open orders account of `owner` on `market`: `asks_base_lots` and
    /// `base_free_native`, `bids_quote_lots` and `quote_free_native`.
    pub(crate) fn open_orders_bytes(
        owner: &Pubkey,
        market: &Pubkey,
        asks_base_lots: i64,
        base_free_native: u64,
        bids_quote_lots: i64,
        quote_free_native: u64,
    ) -> Vec<u8> {
        let mut d = vec![0u8; POSITION_OFFSET + 160];
        put(&mut d, 8, owner.as_ref());
        put(&mut d, 40, market.as_ref());
        let p = POSITION_OFFSET;
        put(&mut d, p + 8, &asks_base_lots.to_le_bytes());
        put(&mut d, p + 16, &base_free_native.to_le_bytes());
        put(&mut d, p + 24, &quote_free_native.to_le_bytes());
        put(&mut d, p + 88, &bids_quote_lots.to_le_bytes());
        d
    }

    /// An `OpenOrdersIndexer` listing `addresses`.
    pub(crate) fn indexer_bytes(addresses: &[Pubkey]) -> Vec<u8> {
        let mut d = OPEN_ORDERS_INDEXER_DISCRIMINATOR.to_vec();
        d.push(255);
        d.extend_from_slice(&(addresses.len() as u32).to_le_bytes());
        d.extend_from_slice(&(addresses.len() as u32).to_le_bytes());
        for a in addresses {
            d.extend_from_slice(a.as_ref());
        }
        d
    }

    /// A SOL/USDC-shaped market: 9 and 6 decimals, 0.001 SOL lots, 1-atom
    /// quote lots, 10 bps both ways.
    pub(crate) fn market_bytes(keys: &[Pubkey; 8]) -> Vec<u8> {
        let mut d = vec![0u8; MARKET_SIZE];
        d[9] = 9;
        d[10] = 6;
        put(&mut d, 16, keys[0].as_ref());
        put(&mut d, 184, b"SOL-USDC");
        put(&mut d, 200, keys[1].as_ref());
        put(&mut d, 232, keys[2].as_ref());
        put(&mut d, 264, keys[3].as_ref());
        put(&mut d, 448, &1i64.to_le_bytes());
        put(&mut d, 456, &1_000_000i64.to_le_bytes());
        put(&mut d, 480, &1_000i64.to_le_bytes());
        put(&mut d, 488, &1_000i64.to_le_bytes());
        put(&mut d, 576, keys[4].as_ref());
        put(&mut d, 608, keys[5].as_ref());
        put(&mut d, 640, keys[6].as_ref());
        put(&mut d, 680, keys[7].as_ref());
        d
    }

    /// One leaf to write: `(price lots, sequence key, base lots, owner, timestamp)`.
    pub(crate) type Leaf = (i64, u64, i64, Pubkey, u64);

    /// A book side whose leaves hang off a chain of inner nodes, in order.
    pub(crate) fn book_side_bytes(leaves: &[Leaf]) -> Vec<u8> {
        let mut d = vec![0u8; NODES_OFFSET + NODE_SIZE * MAX_NODES as usize];
        if leaves.is_empty() {
            return d;
        }
        // inner nodes 0..n-1: left child is leaf i, right child the next inner node (the last one's right is the last leaf)
        let n = leaves.len();
        let leaf_index = |i: usize| (n - 1 + i) as u32;
        let root = if n == 1 { leaf_index(0) } else { 0 };
        put(&mut d, 8, &root.to_le_bytes());
        put(&mut d, 12, &(n as u32).to_le_bytes());
        for i in 0..n.saturating_sub(1) {
            let at = NODES_OFFSET + i * NODE_SIZE;
            d[at] = 1;
            let right = if i + 2 < n {
                (i + 1) as u32
            } else {
                leaf_index(n - 1)
            };
            put(&mut d, at + 24, &leaf_index(i).to_le_bytes());
            put(&mut d, at + 28, &right.to_le_bytes());
        }
        for (i, (price, seq, lots, owner, ts)) in leaves.iter().enumerate() {
            let at = NODES_OFFSET + leaf_index(i) as usize * NODE_SIZE;
            d[at] = 2;
            let key = ((*price as u128) << 64) | u128::from(*seq);
            put(&mut d, at + 8, &key.to_le_bytes());
            put(&mut d, at + 24, owner.as_ref());
            put(&mut d, at + 56, &lots.to_le_bytes());
            put(&mut d, at + 64, &ts.to_le_bytes());
        }
        d
    }
}

#[cfg(test)]
mod tests {
    use super::{testing::*, *};

    #[test]
    fn decodes_a_market() {
        let keys: [Pubkey; 8] = std::array::from_fn(|i| Pubkey::new_from_array([i as u8 + 1; 32]));
        let m = MarketAccount::decode(&market_bytes(&keys)).unwrap();
        assert_eq!((m.base_decimals, m.quote_decimals), (9, 6));
        assert_eq!(m.name, "SOL-USDC");
        assert_eq!(m.market_authority, keys[0]);
        assert_eq!(m.event_heap, keys[3]);
        assert_eq!(m.base_lot_size, 1_000_000);
        assert_eq!(m.taker_fee, 1_000);
        assert_eq!(m.quote_vault, keys[7]);
        assert_eq!(m.open_orders_admin, None);
        assert!(MarketAccount::decode(&market_bytes(&keys)[..100]).is_err());
    }

    /// The SOL/USDC market on mainnet (CFSMrBssNG8Ud1edW59jNLnq2cwrQ9uY5cM3wXmqRJj3), as an RPC node returns it; the expected values
    /// are the program's IDL decoding of the same bytes.
    #[test]
    fn decodes_the_mainnet_sol_usdc_market() {
        let hex: String = include_str!("testdata/sol_usdc_market.hex")
            .split_whitespace()
            .collect();
        let data: Vec<u8> = (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect();
        let m = MarketAccount::decode(&data).unwrap();
        let key = |s: &str| s.parse::<Pubkey>().unwrap();
        assert_eq!(
            m.market_authority,
            key("B44ts4KVwst9dYSYqGB5vY4Wee2KB3AK3e92yEdJzwrw")
        );
        assert_eq!(m.bids, key("Ad5skEiFoaeA27G3UhbpuwnFBCvmuuGEyoiijZhcd5xX"));
        assert_eq!(m.asks, key("53v47CBoaKwoM8tSEDN4oNyCc2ZJenDeuhMJTEw7fL2M"));
        assert_eq!(
            m.event_heap,
            key("F7s6bScqRXB2gsU6s8QHSXJTmpS5t6SfVBs4V2k3HNKn")
        );
        assert_eq!(
            m.base_vault,
            key("D4eZbLyKTtCk8jjbrkqTU2vADKUdGJLj1y4matSdeYW5")
        );
        assert_eq!(
            m.quote_vault,
            key("EA3Qa1WUxuY2BZo6b2Dy3ZxGiK3qS5hkeSwbCzUNomBm")
        );
        assert_eq!(
            m.base_mint,
            key("So11111111111111111111111111111111111111112")
        );
        assert_eq!(
            m.quote_mint,
            key("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v")
        );
        assert_eq!((m.base_lot_size, m.quote_lot_size), (1000000, 1));
        assert_eq!((m.base_decimals, m.quote_decimals), (9, 6));
        assert_eq!((m.maker_fee, m.taker_fee), (1000, 1000));
        assert_eq!(m.name, "SOL-USDC");
        assert_eq!(m.seq_num, 51702855);
    }

    #[test]
    fn walks_a_book_side_in_order() {
        let a = Pubkey::new_from_array([1; 32]);
        let b = Pubkey::new_from_array([2; 32]);
        let data = book_side_bytes(&[
            (150_010, 4, 2_000, a, 100),
            (150_020, 9, 500, b, 101),
            (150_030, 2, 1, a, 102),
        ]);
        let orders = book_side_orders(&data).unwrap();
        assert_eq!(orders.len(), 3);
        assert_eq!(orders[0].price_lots, 150_010);
        assert_eq!(orders[1].owner, b);
        assert_eq!(orders[1].sequence_key, 9);
        assert_eq!(orders[2].timestamp, 102);
        assert!(orders[0].is_live(10_000));
        assert!(book_side_orders(&book_side_bytes(&[])).unwrap().is_empty());
        assert_eq!(
            book_side_orders(&book_side_bytes(&[(1, 1, 1, a, 1)]))
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn decodes_an_open_orders_position() {
        let mut d = vec![0u8; POSITION_OFFSET + 160];
        let owner = Pubkey::new_from_array([5; 32]);
        put(&mut d, 8, owner.as_ref());
        put(&mut d, POSITION_OFFSET + 8, &3i64.to_le_bytes());
        put(&mut d, POSITION_OFFSET + 16, &7u64.to_le_bytes());
        put(&mut d, POSITION_OFFSET + 56, &99u128.to_le_bytes());
        put(&mut d, POSITION_OFFSET + 88, &11i64.to_le_bytes());
        let p = OpenOrdersPosition::decode(&d).unwrap();
        assert_eq!(p.owner, owner);
        assert_eq!(p.asks_base_lots, 3);
        assert_eq!(p.base_free_native, 7);
        assert_eq!(p.maker_volume, 99);
        assert_eq!(p.bids_quote_lots, 11);
    }

    #[test]
    fn measures_an_open_orders_position_as_the_profile_does() {
        let p = OpenOrdersPosition {
            owner: Pubkey::new_unique(),
            market: Pubkey::new_unique(),
            bids_base_lots: 4,
            asks_base_lots: 3,
            base_free_native: 7,
            quote_free_native: 20,
            locked_maker_fees: 2,
            maker_volume: 0,
            taker_volume: 0,
            bids_quote_lots: 11,
        };
        // Bids lock quote, not base: bids_base_lots isn't held.
        assert_eq!(p.base_atoms(1_000_000), 3_000_007);
        assert_eq!(p.quote_atoms(1), 33);
    }

    #[test]
    fn decodes_an_open_orders_indexer() {
        let (a, b) = (Pubkey::new_unique(), Pubkey::new_unique());
        let mut d = OPEN_ORDERS_INDEXER_DISCRIMINATOR.to_vec();
        d.push(254);
        d.extend_from_slice(&3u32.to_le_bytes());
        d.extend_from_slice(&2u32.to_le_bytes());
        d.extend_from_slice(a.as_ref());
        d.extend_from_slice(b.as_ref());
        let indexer = OpenOrdersIndexer::decode(&d).unwrap();
        assert_eq!(indexer.created_counter, 3);
        assert_eq!(indexer.addresses, vec![a, b]);
        d[0] ^= 1;
        assert!(OpenOrdersIndexer::decode(&d).is_err());
        let mut short = OPEN_ORDERS_INDEXER_DISCRIMINATOR.to_vec();
        short.extend_from_slice(&[0, 0, 0, 0, 0, 9, 0, 0, 0]);
        assert!(OpenOrdersIndexer::decode(&short).is_err());
    }
}
