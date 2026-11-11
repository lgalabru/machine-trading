//! Read only completed public blocks. The tape excludes our own orders and
//! uses order direction and arrival times, never account identity as a signal.
use mtx_kit::{Trader, phoenix_rise};
use serde_json::{Value, json};
use solana_rpc_client_api::request::RpcRequest;
use std::collections::VecDeque;

#[derive(Default)]
pub struct Flow {
    next_slot: u64,
    pub trades: VecDeque<(u64, f64, f64)>,
    pub buys: f64,
    pub sells: f64,
    pub start: u64,
    posterior: [f64; 7],
}

/// Decode the fixed prefix of the SDK's Borsh IOC packet, after checking
/// the Phoenix instruction discriminant. Prices and quantities are optional
/// and fixed-width respectively; no untrusted byte offset is indexed unchecked.
fn decode_ioc(data: &[u8]) -> Option<(f64, u64)> {
    let tag = phoenix_rise::ix::PhoenixInstruction::PlaceMarketOrder.discriminant();
    if data.len() < 19 || data[..8] != tag || data[8] != 2 || data[9] > 1 || data[10] > 1 {
        return None;
    }
    let offset = 11 + if data[10] == 1 { 8 } else { 0 };
    let lots = u64::from_le_bytes(data.get(offset..offset + 8)?.try_into().ok()?);
    Some((if data[9] == 0 { 1.0 } else { -1.0 }, lots))
}

fn orders_in_block(block: &Value, program: &str, authority: &str, book: &str) -> Vec<(f64, u64)> {
    let mut orders = Vec::new();
    let Some(txs) = block["transactions"].as_array() else {
        return orders;
    };
    for tx in txs {
        if !tx["meta"]["err"].is_null() {
            continue;
        }
        let msg = &tx["transaction"]["message"];
        let Some(keys) = msg["accountKeys"].as_array() else {
            continue;
        };
        if keys.first().and_then(|v| v.as_str()) == Some(authority) {
            continue;
        }
        let Some(ixs) = msg["instructions"].as_array() else {
            continue;
        };
        for ix in ixs {
            let idx = ix["programIdIndex"].as_u64().unwrap_or(u64::MAX) as usize;
            if keys.get(idx).and_then(|v| v.as_str()) != Some(program) {
                continue;
            }
            let Some(accounts) = ix["accounts"].as_array() else {
                continue;
            };
            if !accounts
                .iter()
                .filter_map(|v| v.as_u64())
                .any(|i| keys.get(i as usize).and_then(|v| v.as_str()) == Some(book))
            {
                continue;
            }
            let Some(encoded) = ix["data"].as_str() else {
                continue;
            };
            let Ok(data) = bs58::decode(encoded).into_vec() else {
                continue;
            };
            if let Some(order) = decode_ioc(&data) {
                orders.push(order);
            }
        }
    }
    orders
}

impl Flow {
    pub fn start(&mut self, slot: u64) {
        self.start = slot;
        self.next_slot = slot;
        self.posterior = [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
    }
    pub fn read(&mut self, trader: &Trader, slot: u64) {
        let Ok(cfg) = trader.phoenix.market(&trader.env.symbol) else {
            return;
        };
        let program = phoenix_rise::ix::PHOENIX_PROGRAM_ID.to_string();
        let authority = trader.authority().to_string();
        let (mut new_buys, mut new_sells) = (0usize, 0usize);
        for previous_slot in self.next_slot.max(slot.saturating_sub(8))..slot {
            let response: Result<Value, _> = trader.rpc.send(
                RpcRequest::GetBlock,
                json!([previous_slot, {
                    "encoding": "json", "transactionDetails": "full", "rewards": false,
                    "maxSupportedTransactionVersion": 0, "commitment": "confirmed"
                }]),
            );
            let Ok(block) = response else {
                continue;
            };
            for (side, lots) in orders_in_block(&block, &program, &authority, &cfg.market_pubkey) {
                let size = lots as f64 / 10f64.powi(cfg.base_lots_decimals as i32);
                self.trades.push_back((slot, side, size));
                if side > 0.0 {
                    self.buys += 1.0;
                    new_buys += 1;
                } else {
                    self.sells += 1.0;
                    new_sells += 1;
                }
                tracing::info!(slot, side, size, "flow");
            }
        }
        self.update_posterior(slot, new_buys, new_sells);
        self.next_slot = slot;
        while self
            .trades
            .front()
            .is_some_and(|(s, _, _)| slot.saturating_sub(*s) > 80)
        {
            self.trades.pop_front();
        }
    }

    /// A hidden-state arrival model distinguishes normal two-slot traffic
    /// from directional bursts at three plausible rates and lifetimes.
    fn update_posterior(&mut self, slot: u64, buys: usize, sells: usize) {
        let elapsed = slot.saturating_sub(self.start) as f64;
        let mut rb = ((self.buys + 15.0) / (elapsed + 100.0)).clamp(0.08, 0.27);
        let mut rs = ((self.sells + 15.0) / (elapsed + 100.0)).clamp(0.08, 0.27);
        if rb + rs > 0.48 {
            let total = rb + rs;
            rb = rb / total * 0.48;
            rs = rs / total * 0.48;
        }
        let none = 1.0 - rb - rs;
        let noise = [
            (0, 0, none * none),
            (1, 0, 2.0 * rb * none),
            (0, 1, 2.0 * rs * none),
            (2, 0, rb * rb),
            (0, 2, rs * rs),
            (1, 1, 2.0 * rb * rs),
        ];
        let rates = [0.5_f64, 1.0, 2.5];
        let hazards = [1.0 / 12.0, 1.0 / 8.0, 1.0 / 4.0];
        let mut likelihood = [0.0_f64; 7];
        for &(b, s, prob) in &noise {
            if b == buys && s == sells {
                likelihood[0] = prob;
            }
        }
        likelihood[0] = likelihood[0].max(1e-8);
        for j in 0..6 {
            for &(b, s, prob) in &noise {
                let excess = if j < 3 && s == sells && buys >= b {
                    Some(buys - b)
                } else if j >= 3 && b == buys && sells >= s {
                    Some(sells - s)
                } else {
                    None
                };
                if let Some(count) = excess {
                    let mut poisson = (-rates[j % 3]).exp();
                    for k in 1..=count {
                        poisson *= rates[j % 3] / k as f64;
                    }
                    likelihood[j + 1] += prob * poisson;
                }
            }
            likelihood[j + 1] = likelihood[j + 1].max(1e-15);
        }
        let mut next = [0.0_f64; 7];
        next[0] = self.posterior[0] * (1.0 - 0.006);
        for j in 0..6 {
            next[0] += self.posterior[j + 1] * hazards[j % 3];
            next[j + 1] =
                self.posterior[0] * 0.001 + self.posterior[j + 1] * (1.0 - hazards[j % 3]);
        }
        let mut total = 0.0;
        for j in 0..7 {
            next[j] *= likelihood[j];
            total += next[j];
        }
        if total > 0.0 {
            for j in 0..7 {
                self.posterior[j] = next[j] / total;
            }
        }
    }
    pub fn signal(&self, _slot: u64) -> (f64, f64) {
        let confidence =
            self.posterior[1..4].iter().sum::<f64>() - self.posterior[4..7].iter().sum::<f64>();
        (
            if confidence > 0.0 {
                1.0
            } else if confidence < 0.0 {
                -1.0
            } else {
                0.0
            },
            confidence.abs(),
        )
    }
    pub fn clear(&mut self) {
        self.trades.clear();
        self.posterior = [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn packet(side: u8, limit: bool) -> Vec<u8> {
        let mut data = phoenix_rise::ix::PhoenixInstruction::PlaceMarketOrder
            .discriminant()
            .to_vec();
        data.extend([2, side, u8::from(limit)]);
        if limit {
            data.extend(10000u64.to_le_bytes());
        }
        data.extend(1250u64.to_le_bytes());
        data
    }
    #[test]
    fn ioc_prefix_is_bounded_and_checks_kind_and_side() {
        for side in 0..=1 {
            for limit in [false, true] {
                let data = packet(side, limit);
                assert_eq!(
                    decode_ioc(&data),
                    Some((if side == 0 { 1.0 } else { -1.0 }, 1250))
                );
                for n in 0..data.len() {
                    assert!(decode_ioc(&data[..n]).is_none());
                }
            }
        }
        assert!(decode_ioc(&packet(2, true)).is_none());
        let mut data = packet(0, true);
        data[8] = 1;
        assert!(decode_ioc(&data).is_none());
    }
    #[test]
    fn flow_ignores_own_other_market_and_failed_transactions() {
        let encoded = bs58::encode(packet(0, true)).into_string();
        let block = json!({"transactions":[{"meta":{"err":null},"transaction":{"message":{
            "accountKeys":["taker", "phoenix", "spcx"],
            "instructions":[{"programIdIndex":1, "accounts":[2], "data":encoded}]
        }}}]});
        assert_eq!(
            orders_in_block(&block, "phoenix", "owner", "spcx"),
            vec![(1.0, 1250)]
        );
        assert!(orders_in_block(&block, "phoenix", "taker", "spcx").is_empty());
        assert!(orders_in_block(&block, "phoenix", "owner", "other").is_empty());
        assert!(orders_in_block(&block, "wrong-program", "owner", "spcx").is_empty());
        let mut failed = block;
        failed["transactions"][0]["meta"]["err"] = json!({"InstructionError":[0,"Custom"]});
        assert!(orders_in_block(&failed, "phoenix", "owner", "spcx").is_empty());
    }
    #[test]
    fn burst_is_directional_and_decays_without_new_orders() {
        let mut flow = Flow::default();
        flow.start(100);
        flow.buys = 3.0;
        flow.update_posterior(102, 3, 0);
        let (direction, score) = flow.signal(102);
        assert_eq!(direction, 1.0);
        assert!(score > 0.99);
        for slot in (104..=140).step_by(2) {
            flow.update_posterior(slot, 0, 0);
        }
        assert!(flow.signal(140).1 < 0.01);
        flow.clear();
        assert_eq!(flow.signal(140), (0.0, 0.0));
    }
}
