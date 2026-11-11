//! Phoenix v1's fills, from its own records: the `Fill` events of the log
//! instruction the program calls on itself, as the Phoenix venue profile
//! (Spot markets, Measurements) defines them.
//!
//! A log instruction's data is the `Log` tag (15), a `Header` event (the
//! instruction, the market's sequence number, the timestamp, the slot, the
//! market, the signer, the number of events), then the events, borsh
//! encoded. A `Fill` names the maker, the maker order's sequence number (its
//! top bit set for a bid), the price in ticks and the base lots filled. The
//! taker is the header's signer, on the side opposite the maker's.

use solana_pubkey::Pubkey;

use super::{PROGRAM_ID, PhoenixV1Market};
use crate::{chain::TransactionSummary, core::Side, protocols::spot::MarketFill};

const LOG_TAG: u8 = 15;
const HEADER: u8 = 1;
const HEADER_LEN: usize = 93;

/// Each event's length, tag included, by tag: `Fill` is 2.
fn event_len(tag: u8) -> Option<usize> {
    Some(match tag {
        2 => 67, // Fill
        3 => 43, // Place
        4 => 35, // Reduce
        5 => 59, // Evict
        6 => 43, // FillSummary
        7 => 11, // Fee
        8 => 27, // TimeInForce
        9 => 59, // ExpiredOrder
        _ => return None,
    })
}

fn u16_at(d: &[u8], o: usize) -> u16 {
    u16::from_le_bytes(d[o..o + 2].try_into().expect("two bytes"))
}

fn u64_at(d: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(d[o..o + 8].try_into().expect("eight bytes"))
}

fn key_at(d: &[u8], o: usize) -> Pubkey {
    Pubkey::new_from_array(d[o..o + 32].try_into().expect("32 bytes"))
}

/// The fills one log instruction records on `market`, at `slot`. Data that
/// isn't a Phoenix log of that market gives none.
pub fn fills_in_log(data: &[u8], market: &PhoenixV1Market, slot: u64) -> Vec<MarketFill> {
    if data.len() < HEADER_LEN || data[0] != LOG_TAG || data[1] != HEADER {
        return Vec::new();
    }
    if key_at(data, 27) != market.keys.market {
        return Vec::new();
    }
    let sequence = u64_at(data, 3);
    let timestamp = i64::from_le_bytes(data[11..19].try_into().expect("eight bytes"));
    let signer = key_at(data, 59);
    let count = u16_at(data, 91);
    let mut fills = Vec::new();
    let mut o = HEADER_LEN;
    for _ in 0..count {
        let Some(len) = data.get(o).copied().and_then(event_len) else {
            break;
        };
        if o + len > data.len() {
            break;
        }
        if data[o] == 2 {
            let index = u16_at(data, o + 1);
            let maker_order = u64_at(data, o + 35);
            let maker_bid = maker_order >> 63 == 1;
            fills.push(MarketFill {
                slot,
                instrument: market.id().clone(),
                sequence: (u128::from(sequence) << 16) | u128::from(index),
                taker: signer,
                maker: key_at(data, o + 3),
                taker_side: if maker_bid { Side::Sell } else { Side::Buy },
                price: market.ticks_to_price(u64_at(data, o + 43)),
                quantity: market.base_lots_to_quantity(u64_at(data, o + 51)),
                unix_timestamp: timestamp,
            });
        }
        o += len;
    }
    fills
}

/// Every fill a landed transaction recorded on `market`. A failed
/// transaction recorded none.
pub fn fills_in_transaction(
    summary: &TransactionSummary,
    market: &PhoenixV1Market,
) -> Vec<MarketFill> {
    if summary.error.is_some() {
        return Vec::new();
    }
    summary
        .inner_instructions
        .iter()
        .filter(|ix| ix.program == PROGRAM_ID)
        .flat_map(|ix| fills_in_log(&ix.data, market, summary.slot))
        .collect()
}

#[cfg(test)]
pub(crate) mod testing {
    //! Log instructions as Phoenix writes them, for tests.

    use super::*;

    /// One `Fill` event: maker, maker order sequence (top bit for a bid),
    /// ticks, base lots.
    pub(crate) struct FillSpec {
        pub maker: Pubkey,
        pub maker_bid: bool,
        pub ticks: u64,
        pub base_lots: u64,
    }

    /// A log instruction's data on `market`, signed by `signer`, with these
    /// fills and one `Place` event between them.
    pub(crate) fn log_data(
        market: &Pubkey,
        signer: &Pubkey,
        sequence: u64,
        fills: &[FillSpec],
    ) -> Vec<u8> {
        let mut d = vec![LOG_TAG, HEADER, 4];
        d.extend_from_slice(&sequence.to_le_bytes());
        d.extend_from_slice(&1_700_000_000i64.to_le_bytes());
        d.extend_from_slice(&123u64.to_le_bytes());
        d.extend_from_slice(market.as_ref());
        d.extend_from_slice(signer.as_ref());
        d.extend_from_slice(&((fills.len() + 1) as u16).to_le_bytes());
        // A Place event first: the decoder must step over it.
        d.push(3);
        d.extend_from_slice(&[0; 42]);
        for (i, f) in fills.iter().enumerate() {
            d.push(2);
            d.extend_from_slice(&(i as u16 + 1).to_le_bytes());
            d.extend_from_slice(f.maker.as_ref());
            let seq = if f.maker_bid { (1u64 << 63) | 7 } else { 7 };
            d.extend_from_slice(&seq.to_le_bytes());
            d.extend_from_slice(&f.ticks.to_le_bytes());
            d.extend_from_slice(&f.base_lots.to_le_bytes());
            d.extend_from_slice(&0u64.to_le_bytes());
        }
        d
    }
}
