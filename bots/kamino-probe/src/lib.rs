//! A probe of a Kamino lending session (`bench/checks/kamino`): it deposits
//! 10 SPCX and borrows 300 USDC against it, tries a 1 USDC borrow every 25
//! steps (and repays it), and each step liquidates the first obligation in
//! the market it finds below health 1, from the accounts as the keeper
//! last refreshed them. Its log says what became of every line.
//!
//! Under a transaction layer, its regime (`MTX_REGIME`) sets how its
//! liquidations bid to land: `bid`, a priority fee of 2,000,000
//! micro-lamports per compute unit; `bundle`, a bundle with a 2,000,000
//! lamport tip; anything else, nothing.

use base64::Engine;
use mtx_kit::{log, prelude::*, rpc};
use serde_json::{Value, json};

/// A session reserve, as the session's venue metadata names it.
struct Reserve {
    id: String,
    address: String,
    market: String,
}

#[derive(Default)]
struct Probe {
    reserves: Vec<Reserve>,
    authority: String,
    /// How its liquidations bid to land, under a transaction layer.
    bid: Option<TxOptions>,
}

/// What a bidding liquidation pays: per compute unit, or as a tip.
const BID_PRICE: u64 = 2_000_000;
const BID_TIP: u64 = 2_000_000;

const SF: f64 = (1u64 << 60) as f64;

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

/// What the probe needs of a reserve: its price per atom, collateral
/// exchange rate, liquidation threshold and borrow factor.
struct Terms {
    price_per_atom: f64,
    rate: f64,
    liquidation: f64,
    borrow_factor: f64,
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
    }
}

impl Probe {
    fn reserve(&self, address: &str) -> Option<&Reserve> {
        self.reserves.iter().find(|r| r.address == address)
    }

    /// The first obligation of someone else's below health 1: its address,
    /// the collateral to take and the debt to repay, by venue id, and the
    /// debt in asset units.
    fn liquidatable(&self) -> Option<(String, String, String, f64)> {
        let market = &self.reserves.first()?.market;
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
        let mut list: Vec<(String, Vec<u8>)> = found
            .as_array()?
            .iter()
            .filter_map(|o| Some((o["pubkey"].as_str()?.to_string(), decode(&o["account"])?)))
            .collect();
        list.sort();
        for (address, data) in list {
            if key(&data, 64) == self.authority {
                continue;
            }
            let (mut threshold, mut debt) = (0.0, 0.0);
            let (mut best_collateral, mut best_debt) = ((0.0, None), (0.0, None, 0.0));
            for i in 0..8 {
                let at = 96 + i * 136;
                let reserve = key(&data, at);
                let Some(t) = of(&reserve) else { continue };
                let value = u64_at(&data, at + 32) * t.rate * t.price_per_atom;
                threshold += value * t.liquidation;
                if value > best_collateral.0 {
                    best_collateral = (value, Some(reserve));
                }
            }
            for i in 0..5 {
                let at = 1208 + i * 200;
                let reserve = key(&data, at);
                let Some(t) = of(&reserve) else { continue };
                let atoms = sf_at(&data, at + 88);
                let value = atoms * t.price_per_atom;
                debt += value * t.borrow_factor;
                if value > best_debt.0 {
                    best_debt = (value, Some(reserve), value);
                }
            }
            if debt > 0.0 && threshold / debt < 1.0 {
                let collateral = self.reserve(best_collateral.1.as_deref()?)?.id.clone();
                let repay = self.reserve(best_debt.1.as_deref()?)?.id.clone();
                // The debt asset is a dollar: units of it are its value.
                return Some((address, collateral, repay, best_debt.2));
            }
        }
        None
    }
}

impl Strategy for Probe {
    fn start(&mut self, session: &Session) -> BotResult {
        let venue: Value = serde_json::from_str(&session.venue).map_err(|e| e.to_string())?;
        for r in venue["reserves"].as_array().into_iter().flatten() {
            self.reserves.push(Reserve {
                id: r["id"].as_str().unwrap_or_default().into(),
                address: r["reserve"].as_str().unwrap_or_default().into(),
                market: r["market"].as_str().unwrap_or_default().into(),
            });
        }
        self.authority = session.authority.clone();
        self.bid = match session.var("MTX_REGIME") {
            Some("bid") => Some(TxOptions {
                cu_price: Some(BID_PRICE),
                ..TxOptions::default()
            }),
            Some("bundle") => Some(TxOptions {
                tip: Some(BID_TIP),
                ..TxOptions::default()
            }),
            _ => None,
        };
        log(&format!(
            "reserves: {}",
            self.reserves
                .iter()
                .map(|r| r.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
        Ok(())
    }

    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        for send in &market.sends {
            log(&format!(
                "step {}: {:?} at {}: {:?}",
                send.step, send.order, send.venue, send.state
            ));
        }
        let (Some(collateral), Some(debt)) = (self.reserves.first(), self.reserves.get(1)) else {
            return Ok(());
        };
        let (collateral, debt) = (collateral.id.clone(), debt.id.clone());
        if let Some(view) = market.venues.iter().find(|v| v.id == debt) {
            let stat = |name: &str| view.stats.iter().find(|(n, _)| n == name).map(|(_, v)| *v);
            log(&format!(
                "step {}: lending {:?}, price {:?} twap {:?} status {:?}",
                market.k,
                view.lending,
                stat("price"),
                stat("twap"),
                stat("price_status")
            ));
        }
        match market.k {
            0 => {
                orders.deposit(10.0).at(&collateral);
                return Ok(());
            }
            1 => {
                orders.borrow(300.0).at(&debt);
                return Ok(());
            }
            k if k > 2 && k % 25 == 0 => {
                log(&format!("step {k}: probing a 1 USDC borrow"));
                orders.borrow(1.0).at(&debt);
            }
            k if k > 2 && k % 25 == 1 => {
                orders.repay(Some(1.0)).at(&debt);
            }
            _ => {}
        }
        if let Some((obligation, take, repay, owed)) = self.liquidatable() {
            log(&format!(
                "step {}: liquidating {obligation}, {owed:.2} owed",
                market.k
            ));
            orders.liquidate(&obligation, &take, owed * 0.2).at(&repay);
            if let Some(bid) = self.bid {
                orders.tx(bid);
            }
        }
        Ok(())
    }
}

mtx_kit::export!(Probe);
