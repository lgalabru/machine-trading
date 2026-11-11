//! OpenBook v2's fills, from its own records, as the OpenBook venue profile
//! (Measurements) defines them: the `FillEvent`s waiting in a market's event
//! heap, and the `FillLog`s the program logs when a fill settles, the same
//! fill under the same `seq_num`.
//!
//! The event heap: an 8-byte discriminator, a header (`free_head`,
//! `used_head`, `count`, padding, `seq_num`), then 600 nodes of 152 bytes,
//! each a `next` and `prev` index and padding, then a 144-byte event. The
//! used list runs oldest first from `used_head`. A fill event has type 0:
//! the taker's side, the maker's order slot and whether it left the book,
//! the timestamp, `seq_num`, the maker's open orders account and the
//! taker's (or the signer, for a take-only order), the price in price lots
//! and the quantity in base lots.

use base64::Engine;
use solana_pubkey::Pubkey;

use super::OpenBookV2Market;
use crate::{
    chain::{ChainError, TransactionSummary},
    core::Side,
    protocols::spot::MarketFill,
};

const HEAP_HEADER: usize = 8;
const HEAP_NODES: usize = HEAP_HEADER + 16;
const NODE_LEN: usize = 152;
const MAX_EVENTS: usize = 600;
const FILL: u8 = 0;
/// `sha256("event:FillLog")[..8]`, Anchor's event discriminator.
const FILL_LOG: [u8; 8] = [150, 23, 41, 148, 152, 162, 215, 64];
const FILL_LOG_LEN: usize = 8 + 32 + 3 + 8 + 8 + 32 + 8 + 8 + 8 + 32 + 8 + 8 + 8 + 8;

fn u16_at(d: &[u8], o: usize) -> u16 {
    u16::from_le_bytes(d[o..o + 2].try_into().expect("two bytes"))
}

fn u64_at(d: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(d[o..o + 8].try_into().expect("eight bytes"))
}

fn i64_at(d: &[u8], o: usize) -> i64 {
    i64::from_le_bytes(d[o..o + 8].try_into().expect("eight bytes"))
}

fn key_at(d: &[u8], o: usize) -> Pubkey {
    Pubkey::new_from_array(d[o..o + 32].try_into().expect("32 bytes"))
}

fn taker_side(byte: u8) -> Side {
    if byte == 0 { Side::Buy } else { Side::Sell }
}

fn decode_error(detail: String) -> ChainError {
    ChainError::Decode {
        what: "openbook v2 event heap",
        detail,
    }
}

/// The fills waiting in `market`'s event heap, oldest first, dated `slot`.
pub fn heap_fills(
    data: &[u8],
    market: &OpenBookV2Market,
    slot: u64,
) -> Result<Vec<MarketFill>, ChainError> {
    if data.len() < HEAP_NODES + MAX_EVENTS * NODE_LEN {
        return Err(decode_error(format!("{} bytes", data.len())));
    }
    let used_head = usize::from(u16_at(data, HEAP_HEADER + 2));
    let count = usize::from(u16_at(data, HEAP_HEADER + 4));
    let mut fills = Vec::new();
    let mut node = used_head;
    for _ in 0..count {
        if node >= MAX_EVENTS {
            return Err(decode_error(format!("node index {node}")));
        }
        let o = HEAP_NODES + node * NODE_LEN;
        let e = o + 8;
        if data[e] == FILL {
            fills.push(MarketFill {
                slot,
                instrument: market.id().clone(),
                sequence: u128::from(u64_at(data, e + 16)),
                taker: key_at(data, e + 64),
                maker: key_at(data, e + 24),
                taker_side: taker_side(data[e + 1]),
                price: market.price_lots_to_price(i64_at(data, e + 104)),
                quantity: market.base_lots_to_quantity(i64_at(data, e + 120)),
                unix_timestamp: u64_at(data, e + 8) as i64,
            });
        }
        node = usize::from(u16_at(data, o));
    }
    Ok(fills)
}

/// A `FillLog` event's bytes, discriminator first, on `market`, at `slot`.
pub fn fill_log(data: &[u8], market: &OpenBookV2Market, slot: u64) -> Option<MarketFill> {
    if data.len() < FILL_LOG_LEN || data[..8] != FILL_LOG || key_at(data, 8) != market.keys.market {
        return None;
    }
    let side = data[40];
    let mut o = 43;
    let timestamp = u64_at(data, o);
    o += 8;
    let seq_num = u64_at(data, o);
    o += 8;
    let maker = key_at(data, o);
    // The maker's client order id, fee and timestamp.
    o += 32 + 24;
    let taker = key_at(data, o);
    // The taker's client order id and fee.
    o += 32 + 16;
    let price = i64_at(data, o);
    let quantity = i64_at(data, o + 8);
    Some(MarketFill {
        slot,
        instrument: market.id().clone(),
        sequence: u128::from(seq_num),
        taker,
        maker,
        taker_side: taker_side(side),
        price: market.price_lots_to_price(price),
        quantity: market.base_lots_to_quantity(quantity),
        unix_timestamp: timestamp as i64,
    })
}

/// Every `FillLog` a landed transaction logged for `market`.
pub fn logged_fills(summary: &TransactionSummary, market: &OpenBookV2Market) -> Vec<MarketFill> {
    if summary.error.is_some() {
        return Vec::new();
    }
    summary
        .logs
        .iter()
        .filter_map(|line| line.strip_prefix("Program data: "))
        .filter_map(|data| base64::engine::general_purpose::STANDARD.decode(data).ok())
        .filter_map(|data| fill_log(&data, market, summary.slot))
        .collect()
}

#[cfg(test)]
pub(crate) mod testing {
    //! Event heaps and fill logs as OpenBook writes them, for tests.

    use super::*;

    /// One fill: taker side (0 bid), seq_num, maker, taker, price lots,
    /// base lots.
    pub(crate) struct EventSpec {
        pub taker_side: u8,
        pub seq_num: u64,
        pub maker: Pubkey,
        pub taker: Pubkey,
        pub price_lots: i64,
        pub base_lots: i64,
    }

    /// A heap holding `fills` in its used list, in order, at scattered nodes,
    /// with an out event (type 1) between them.
    pub(crate) fn heap(fills: &[EventSpec]) -> Vec<u8> {
        let mut d = vec![0u8; HEAP_NODES + MAX_EVENTS * NODE_LEN];
        let nodes: Vec<usize> = (0..=fills.len())
            .map(|i| (i * 37 + 5) % MAX_EVENTS)
            .collect();
        d[HEAP_HEADER + 2..HEAP_HEADER + 4].copy_from_slice(&(nodes[0] as u16).to_le_bytes());
        d[HEAP_HEADER + 4..HEAP_HEADER + 6].copy_from_slice(&(nodes.len() as u16).to_le_bytes());
        for (i, node) in nodes.iter().enumerate() {
            let o = HEAP_NODES + node * NODE_LEN;
            let next = nodes.get(i + 1).copied().unwrap_or(0) as u16;
            d[o..o + 2].copy_from_slice(&next.to_le_bytes());
            let e = o + 8;
            // The out event sits second.
            let fill = match i {
                1 => {
                    d[e] = 1;
                    continue;
                }
                0 => &fills[0],
                i => &fills[i - 1],
            };
            d[e] = FILL;
            d[e + 1] = fill.taker_side;
            d[e + 8..e + 16].copy_from_slice(&1_700_000_000u64.to_le_bytes());
            d[e + 16..e + 24].copy_from_slice(&fill.seq_num.to_le_bytes());
            d[e + 24..e + 56].copy_from_slice(fill.maker.as_ref());
            d[e + 64..e + 96].copy_from_slice(fill.taker.as_ref());
            d[e + 104..e + 112].copy_from_slice(&fill.price_lots.to_le_bytes());
            d[e + 120..e + 128].copy_from_slice(&fill.base_lots.to_le_bytes());
        }
        d
    }

    /// The `Program data:` line of a `FillLog`.
    pub(crate) fn fill_log_line(market: &Pubkey, fill: &EventSpec) -> String {
        let mut d = FILL_LOG.to_vec();
        d.extend_from_slice(market.as_ref());
        d.extend_from_slice(&[fill.taker_side, 0, 1]);
        d.extend_from_slice(&1_700_000_000u64.to_le_bytes());
        d.extend_from_slice(&fill.seq_num.to_le_bytes());
        d.extend_from_slice(fill.maker.as_ref());
        d.extend_from_slice(&[0; 24]);
        d.extend_from_slice(fill.taker.as_ref());
        d.extend_from_slice(&[0; 16]);
        d.extend_from_slice(&fill.price_lots.to_le_bytes());
        d.extend_from_slice(&fill.base_lots.to_le_bytes());
        format!(
            "Program data: {}",
            base64::engine::general_purpose::STANDARD.encode(d)
        )
    }
}
