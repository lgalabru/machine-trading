//! A probe of `liquidation-flash` (RFD 0005, task 1): each step it reads
//! the upstream SOL price (Scope's pushed `FixedPrice` entry, in its
//! mappings), finds the largest obligation of someone else's that is
//! liquidatable there, and liquidates it in one atomic group:
//!
//! 1. a push of SOL's Scope entries, if Scope still reads it healthy;
//! 2. a flash borrow of the USDC it repays;
//! 3. the liquidation, which seizes SOL at the bonus;
//! 4. a swap of the SOL seized back to USDC, through Orca's pool;
//! 5. the flash loan's repay, which the host adds.
//!
//! It tips 60% of the bonus it expects to keep (half of the reserve's), more
//! than the session's liquidators, who bid 25 to 45%: as a Jito bundle, so
//! a group that loses the race fails unpaid rather than landing and paying
//! its fee. Its log says what became of every group.

use base64::Engine;
use mtx_kit::{AmountUnit, log, prelude::*, rpc};
use serde_json::{Value, json};

const SF: f64 = (1u64 << 60) as f64;
/// `OracleMappings::generic`, 20 bytes per entry, after a discriminator,
/// 512 keys, types, two-byte tolerances, TWAP masks and two-byte reference
/// prices (Scope `a09283d`).
const MAPPINGS_GENERIC_AT: usize = 8 + 512 * 32 + 512 + 512 * 2 + 512 + 512 * 2;
/// Steps it leaves an obligation alone after sending for it.
const RETRY_STEPS: u64 = 4;
/// What it sells of the SOL a liquidation seizes, over the repay's worth:
/// the bonus less the protocol's share is at least this.
const SELL_OVER_REPAY: f64 = 1.002;
/// The share of the expected bonus it tips.
const BID_SHARE: f64 = 0.6;
const BONUS_KEPT_SHARE: f64 = 0.5;

struct Reserve {
    id: String,
    address: String,
    market: String,
    mint: String,
    scope_mappings: Option<String>,
    scope_entry: Option<u16>,
}

#[derive(Default)]
struct Probe {
    reserves: Vec<Reserve>,
    pool: String,
    authority: String,
    close_factor: f64,
    tried: Vec<(String, u64)>,
    group: u32,
}

fn u64_at(data: &[u8], at: usize) -> f64 {
    u64::from_le_bytes(data[at..at + 8].try_into().expect("8 bytes")) as f64
}

fn sf_at(data: &[u8], at: usize) -> f64 {
    u128::from_le_bytes(data[at..at + 16].try_into().expect("16 bytes")) as f64 / SF
}

fn key(data: &[u8], at: usize) -> String {
    bs58::encode(&data[at..at + 32]).into_string()
}

fn decode(account: &Value) -> Option<Vec<u8>> {
    let text = account["data"][0].as_str()?;
    base64::engine::general_purpose::STANDARD.decode(text).ok()
}

fn account(address: &str) -> Option<Vec<u8>> {
    let params = json!([address, { "encoding": "base64" }]).to_string();
    let found: Value = serde_json::from_str(&rpc("getAccountInfo", &params).ok()?).ok()?;
    decode(&found["value"])
}

/// A reserve's terms: its price per atom as Scope last gave it, the
/// collateral's exchange rate, its liquidation threshold, borrow factor and
/// least bonus, and its decimals.
#[derive(Clone, Copy)]
struct Terms {
    price_per_atom: f64,
    rate: f64,
    liquidation: f64,
    borrow_factor: f64,
    bonus: f64,
    decimals: i32,
}

fn terms(data: &[u8]) -> Terms {
    let decimals = u64_at(data, 272) as i32;
    let supply = u64_at(data, 224) + sf_at(data, 232)
        - sf_at(data, 344)
        - sf_at(data, 360)
        - sf_at(data, 376);
    let collateral = u64_at(data, 2592);
    Terms {
        price_per_atom: sf_at(data, 248) / 10f64.powi(decimals),
        rate: if collateral > 0.0 {
            supply / collateral
        } else {
            1.0
        },
        liquidation: f64::from(data[4873]) / 100.0,
        borrow_factor: u64_at(data, 5008) / 100.0,
        bonus: f64::from(u16::from_le_bytes([data[4874], data[4875]])) / 10_000.0,
        decimals,
    }
}

/// A liquidation it can make: the obligation, its health at Scope's price,
/// the USDC it repays and what that is worth.
struct Target {
    obligation: String,
    stale: bool,
    repay: f64,
    repay_usd: f64,
}

impl Probe {
    /// The SOL reserve's upstream price: its pushed entry's `FixedPrice`.
    fn upstream(&self, sol: &Reserve) -> Option<f64> {
        let data = account(sol.scope_mappings.as_deref()?)?;
        let at = MAPPINGS_GENERIC_AT + 20 * usize::from(sol.scope_entry?);
        let value = u64_at(&data, at);
        let exp = u64_at(&data, at + 8) as i32;
        Some(value / 10f64.powi(exp))
    }

    /// The largest repay among obligations liquidatable at `upstream`.
    fn target(&self, k: u64, sol: &Reserve, upstream: f64) -> Option<(Target, f64, Terms)> {
        let market = &sol.market;
        let params = json!(["KLend2g3cP87fffoy8q1mQqGKjrxjC8boSyAYavgmjD", {
            "encoding": "base64",
            "filters": [{ "dataSize": 3344 }, { "memcmp": { "offset": 32, "bytes": market } }],
        }]);
        let found: Value =
            serde_json::from_str(&rpc("getProgramAccounts", &params.to_string()).ok()?).ok()?;
        let addresses: Vec<&str> = self.reserves.iter().map(|r| r.address.as_str()).collect();
        let reserves: Value = serde_json::from_str(
            &rpc(
                "getMultipleAccounts",
                &json!([addresses, { "encoding": "base64" }]).to_string(),
            )
            .ok()?,
        )
        .ok()?;
        let terms: Vec<(String, Terms)> = self
            .reserves
            .iter()
            .zip(reserves["value"].as_array()?)
            .filter_map(|(r, a)| Some((r.address.clone(), terms(&decode(a)?))))
            .collect();
        let of = |address: &str| terms.iter().find(|(a, _)| a == address).map(|(_, t)| t);
        let sol_terms = of(&sol.address)?;
        let scope_sol = sol_terms.price_per_atom * 10f64.powi(sol_terms.decimals);
        let ratio = upstream / scope_sol;
        let mut best: Option<Target> = None;
        for o in found.as_array()? {
            let address = o["pubkey"].as_str()?.to_string();
            let data = decode(&o["account"])?;
            if key(&data, 64) == self.authority
                || self
                    .tried
                    .iter()
                    .any(|(a, at)| *a == address && k < at + RETRY_STEPS)
            {
                continue;
            }
            let (mut threshold, mut threshold_scope, mut debt, mut owed) = (0.0, 0.0, 0.0, 0.0);
            for i in 0..8 {
                let at = 96 + i * 136;
                let reserve = key(&data, at);
                let Some(t) = of(&reserve) else { continue };
                let value = u64_at(&data, at + 32) * t.rate * t.price_per_atom;
                let moved = if reserve == sol.address { ratio } else { 1.0 };
                threshold += value * moved * t.liquidation;
                threshold_scope += value * t.liquidation;
            }
            for i in 0..5 {
                let at = 1208 + i * 200;
                let reserve = key(&data, at);
                let Some(t) = of(&reserve) else { continue };
                let atoms = sf_at(&data, at + 88);
                debt += atoms * t.price_per_atom * t.borrow_factor;
                if reserve != sol.address {
                    owed += atoms / 10f64.powi(t.decimals);
                }
            }
            if debt <= 0.0 || threshold / debt >= 1.0 {
                continue;
            }
            let repay = owed * self.close_factor * 0.999;
            if best.as_ref().is_none_or(|b| repay > b.repay) {
                best = Some(Target {
                    obligation: address,
                    stale: threshold_scope / debt >= 1.0,
                    repay,
                    repay_usd: repay,
                });
            }
        }
        best.map(|b| (b, ratio, *sol_terms))
    }
}

impl Strategy for Probe {
    fn start(&mut self, session: &Session) -> BotResult {
        let venue: Value = serde_json::from_str(&session.venue).map_err(|e| e.to_string())?;
        for r in venue["reserves"].as_array().into_iter().flatten() {
            let text = |name: &str| r[name].as_str().unwrap_or_default().to_string();
            self.reserves.push(Reserve {
                id: text("id"),
                address: text("reserve"),
                market: text("market"),
                mint: text("mint"),
                scope_mappings: r["scope_mappings"].as_str().map(str::to_string),
                scope_entry: r["scope_entries"][0].as_u64().map(|e| e as u16),
            });
        }
        self.pool = venue["pools"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|p| p["id"].as_str())
            .find(|id| id.starts_with("orca:"))
            .unwrap_or_default()
            .to_string();
        self.authority = session.authority.clone();
        let market = self
            .reserves
            .first()
            .map(|r| r.market.clone())
            .unwrap_or_default();
        self.close_factor = account(&market).map_or(0.1, |d| f64::from(d[126]) / 100.0);
        log(&format!(
            "reserves {}, pool {}, close factor {}, lookup table {}",
            self.reserves
                .iter()
                .map(|r| r.id.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            self.pool,
            self.close_factor,
            venue["lookup_table"]
        ));
        Ok(())
    }

    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        for send in &market.sends {
            log(&format!(
                "step {} sent: {:?} at {}: {:?} slot {:?} fee {:?}",
                send.step, send.order, send.venue, send.state, send.slot, send.fee
            ));
        }
        let wrapped = "So11111111111111111111111111111111111111112";
        let Some(sol) = self.reserves.iter().find(|r| r.mint == wrapped) else {
            return Ok(());
        };
        let Some(usdc) = self.reserves.iter().find(|r| r.mint != wrapped) else {
            return Ok(());
        };
        let Some(upstream) = self.upstream(sol) else {
            return Ok(());
        };
        let Some((target, ratio, sol_terms)) = self.target(market.k, sol, upstream) else {
            return Ok(());
        };
        let (sol_id, usdc_id) = (sol.id.clone(), usdc.id.clone());
        let kept_usd = target.repay_usd * sol_terms.bonus * BONUS_KEPT_SHARE;
        let tip = (BID_SHARE * kept_usd / upstream * 1e9) as u64;
        let sell = target.repay_usd / upstream * SELL_OVER_REPAY;
        let limit = (upstream * 0.997 * 100.0).floor() / 100.0;
        log(&format!(
            "step {}: liquidating {} at upstream {upstream:.4} (Scope x{:.5}, {}): repay {:.2} USDC, sell {sell:.4} SOL at >= {limit}, tip {tip}",
            market.k,
            target.obligation,
            1.0 / ratio,
            if target.stale { "pushing" } else { "fresh" },
            target.repay,
        ));
        self.group += 1;
        let id = self.group;
        let options = TxOptions {
            tip: Some(tip),
            atomic: Some(id),
            ..TxOptions::default()
        };
        // Built with `split` (`just liquidation-flash-smoke`'s check of
        // mandate v0.3.0's rule 11), it repays from its wallet's USDC and
        // sells the seized SOL in a transaction of its own in the same
        // step: between the two the wallet holds the SOL, and the rule
        // breaks.
        let split = cfg!(feature = "split");
        match (target.stale, split) {
            (true, false) => {
                orders.push_price().at(&sol_id).tx(options);
                orders.flash(target.repay).at(&usdc_id).atomic(id);
            }
            (false, false) => {
                orders.flash(target.repay).at(&usdc_id).tx(options);
            }
            (true, true) => {
                orders.push_price().at(&sol_id).tx(options);
            }
            (false, true) => {}
        }
        let liquidation = orders
            .liquidate(&target.obligation, &sol_id, target.repay)
            .at(&usdc_id);
        match (target.stale, split) {
            (false, true) => {
                liquidation.tx(options);
            }
            _ => {
                liquidation.atomic(id);
            }
        }
        let sale = orders
            .swap(Side::Ask, sell, AmountUnit::Base, limit)
            .at(&self.pool);
        match split {
            true => {
                sale.tx(TxOptions {
                    tip: Some(tip),
                    ..TxOptions::default()
                });
            }
            false => {
                sale.atomic(id);
            }
        }
        self.tried.push((target.obligation, market.k));
        Ok(())
    }
}

mtx_kit::export!(Probe);
