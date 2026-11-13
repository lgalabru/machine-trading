//! Recover public taker orders from completed blocks. The current block is
//! still open during a turn, so only query slots strictly before this turn.
use mtx_kit::venue::phoenix_rise::ix::PhoenixInstruction;
use mtx_kit::{log, rpc};
use serde_json::{Value, json};
use std::collections::VecDeque;

#[derive(Default)]
pub struct Flow {
    pub buys: f64,
    pub sells: f64,
    pub authority: String,
    pub market: String,
    pub cursor: u64,
    pub events: VecDeque<(u64, f64, f64)>,
}
impl Flow {
    pub fn update(&mut self, slot: u64) -> (f64, f64, f64) {
        let disc = PhoenixInstruction::PlaceMarketOrder.discriminant();
        let first = self.cursor.max(slot.saturating_sub(4));
        for s in first..slot {
            let Ok(raw)=rpc("getBlock",&json!([s,{"encoding":"json","commitment":"confirmed","transactionDetails":"full","rewards":false,"maxSupportedTransactionVersion":0}]).to_string()) else {continue};
            let Ok(block) = serde_json::from_str::<Value>(&raw) else {
                continue;
            };
            let Some(txs) = block["transactions"].as_array() else {
                continue;
            };
            for tx in txs {
                let msg = &tx["transaction"]["message"];
                let Some(keys) = msg["accountKeys"].as_array() else {
                    continue;
                };
                if keys.first().and_then(Value::as_str) == Some(&self.authority) {
                    continue;
                }
                if !keys.iter().any(|k| k.as_str() == Some(&self.market)) {
                    continue;
                }
                let Some(ixs) = msg["instructions"].as_array() else {
                    continue;
                };
                for ix in ixs {
                    let Some(encoded) = ix["data"].as_str() else {
                        continue;
                    };
                    let Ok(data) = bs58::decode(encoded).into_vec() else {
                        continue;
                    };
                    if data.len() < 19 || data[..8] != disc || data[8] != 2 || data[9] > 1 {
                        continue;
                    }
                    let side = if data[9] == 0 { 1.0 } else { -1.0 };
                    let offset = 11 + if data[10] == 1 { 8 } else { 0 };
                    let Some(bytes) = data.get(offset..offset + 8) else {
                        continue;
                    };
                    let size =
                        u64::from_le_bytes(bytes.try_into().unwrap_or([0; 8])) as f64 / 1000.0;
                    if side > 0.0 {
                        self.buys += 1.0;
                    } else {
                        self.sells += 1.0;
                    }
                    self.events.push_back((s, side, size));
                    log(&format!("FLOW slot={} side={} size={}", s, side, size));
                }
            }
        }
        self.cursor = slot;
        while self.events.front().is_some_and(|e| e.0 + 40 < slot) {
            self.events.pop_front();
        }
        let mut fast = 0.0;
        let mut slow = 0.0;
        let mut volume = 0.0;
        for &(s, d, v) in &self.events {
            let age = slot - s;
            if age <= 14 {
                fast += d;
                volume += d * v;
            }
            if age <= 30 {
                slow += d;
            }
        }
        (fast, slow, volume)
    }
}
