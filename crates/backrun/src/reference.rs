//! Fixed reference searchers. Approximate quotes rank candidate routes only;
//! actual program simulation determines profit before a signed submission.
use anyhow::{Context as _, Result};
use base64::{Engine, prelude::BASE64_STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use solana_signer::Signer;

use crate::{
    fixture::{Leg, Market, PoolKind},
    programs::*,
    sdk::{Context, Observation},
};

#[derive(Clone, Serialize, Deserialize)]
pub struct Strategy {
    pub hops: usize,
    pub search_budget: usize,
    pub keep_basis_points: u64,
}
pub fn strategies() -> Vec<Strategy> {
    vec![
        Strategy {
            hops: 2,
            search_budget: 32,
            keep_basis_points: 4500,
        },
        Strategy {
            hops: 4,
            search_budget: 256,
            keep_basis_points: 2500,
        },
        Strategy {
            hops: 5,
            search_budget: 1024,
            keep_basis_points: 6500,
        },
    ]
}

pub fn cycles(market: &Market, max_hops: usize) -> Vec<Vec<Leg>> {
    fn visit(
        m: &Market,
        at: usize,
        max: usize,
        path: &mut Vec<Leg>,
        seen: u32,
        out: &mut Vec<Vec<Leg>>,
    ) {
        if path.len() == max {
            return;
        }
        for (index, pool) in m.pools.iter().enumerate() {
            if path.iter().any(|leg| leg.pool == index) {
                continue;
            }
            let direction = if pool.mints[0] == at {
                true
            } else if pool.mints[1] == at {
                false
            } else {
                continue;
            };
            let next = pool.mints[usize::from(direction)];
            path.push(Leg {
                pool: index,
                a_to_b: direction,
            });
            if next == 0 && path.len() >= 2 {
                out.push(path.clone());
            } else if seen & (1 << next) == 0 {
                visit(m, next, max, path, seen | (1 << next), out);
            }
            path.pop();
        }
    }
    let mut out = vec![];
    visit(market, 0, max_hops, &mut vec![], 1, &mut out);
    out
}

fn estimate(market: &Market, state: &Observation, route: &[Leg], input: u64) -> f64 {
    let mut amount = input as f64;
    for leg in route {
        let pool = &market.pools[leg.pool];
        let state = &state.pools[leg.pool];
        let i = usize::from(!leg.a_to_b);
        let (a, b) = if pool.kind == PoolKind::ConstantProduct {
            (state.reserves[i] as f64, state.reserves[1 - i] as f64)
        } else {
            let sqrt = state.sqrt_price.parse::<f64>().unwrap_or(0.0) / (1u128 << 64) as f64;
            let liquidity = state.liquidity.parse::<f64>().unwrap_or(0.0);
            if leg.a_to_b {
                (liquidity / sqrt, liquidity * sqrt)
            } else {
                (liquidity * sqrt, liquidity / sqrt)
            }
        };
        amount *= 1.0 - pool.fee_millionths as f64 / 1_000_000.0;
        amount = b * amount / (a + amount);
    }
    amount - input as f64
}

pub struct Client {
    http: reqwest::blocking::Client,
    url: String,
    token: String,
}
impl Client {
    pub fn new(url: String, token: String) -> Result<Self> {
        Ok(Self {
            http: reqwest::blocking::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()?,
            url,
            token,
        })
    }
    pub fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T> {
        Ok(self
            .http
            .get(format!("{}{path}", self.url))
            .bearer_auth(&self.token)
            .send()?
            .error_for_status()?
            .json()?)
    }
    pub fn ready(&self) -> Result<()> {
        self.http
            .post(format!("{}/ready", self.url))
            .bearer_auth(&self.token)
            .send()?
            .error_for_status()?;
        Ok(())
    }
    pub fn rpc(&self, method: &str, params: Value) -> Result<Value> {
        let response: Value = self
            .http
            .post(format!("{}/rpc", self.url))
            .bearer_auth(&self.token)
            .json(&json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}))
            .send()?
            .error_for_status()?
            .json()?;
        anyhow::ensure!(
            response.get("error").is_none(),
            "gateway: {}",
            response["error"]
        );
        Ok(response["result"].clone())
    }
}

pub fn run(url: String, token: String, strategy: Strategy, idle: bool) -> Result<()> {
    let client = Client::new(url, token)?;
    let mut context: Context = client.get("/context")?;
    context.market.hydrate();
    let session = context.wallet()?;
    let routes = cycles(&context.market, strategy.hops);
    client.ready()?;
    loop {
        std::thread::sleep(std::time::Duration::from_millis(100));
        let status: Value = match client.get("/status") {
            Ok(s) => s,
            Err(_) => continue,
        };
        if status["finished"] == true {
            return Ok(());
        }
        if status["running"] != true || idle {
            continue;
        }
        let observation = match observe(&client, &context.market) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let mut ranked = vec![];
        for route in &routes {
            for amount in [100_000_000, 1_000_000_000, 5_000_000_000, 15_000_000_000] {
                let profit = estimate(&context.market, &observation, route, amount);
                if profit > 10_000.0 {
                    ranked.push((profit, amount, route));
                }
            }
        }
        ranked.sort_by(|a, b| b.0.total_cmp(&a.0));
        for (_, amount, route) in ranked
            .into_iter()
            .take(strategy.search_budget.min(crate::MAX_SIMULATIONS as usize))
        {
            let mut instructions = vec![];
            let mut inputs = vec![];
            for (hop, leg) in route.iter().enumerate() {
                let pool = &context.market.pools[leg.pool];
                inputs.push(ata(
                    session.trader.pubkey(),
                    key(&context.market.tokens[pool.mints[usize::from(!leg.a_to_b)]]),
                ));
                let amount = if hop == 0 { amount } else { 1 };
                instructions.push(match pool.kind {
                    PoolKind::ConstantProduct => {
                        cpmm_swap(pool, session.trader.pubkey(), leg.a_to_b, amount, 1)
                    }
                    PoolKind::Concentrated => whirlpool_swap(
                        pool,
                        session.trader.pubkey(),
                        leg.a_to_b,
                        amount,
                        1,
                        observation.pools[leg.pool].tick,
                    )?,
                });
            }
            let route_instruction = match session.route(&instructions, &inputs) {
                Ok(ix) => ix,
                Err(_) => continue,
            };
            let wire = context.transaction(
                vec![route_instruction],
                &observation.blockhash,
                1_400_000,
                0,
            )?;
            let simulation =
                match client.rpc("simulateTransaction", json!([BASE64_STANDARD.encode(wire),{"encoding":"base64","commitment":"processed","accounts":{"encoding":"base64","addresses":[context.token_accounts[0]]}}])) {
                    Ok(sim) => sim,
                    Err(_) => break,
                };
            if !simulation["value"]["err"].is_null() {
                continue;
            }
            let encoded = simulation["value"]["accounts"][0]["data"][0]
                .as_str()
                .context("missing simulated settlement")?;
            let data = BASE64_STANDARD.decode(encoded)?;
            let after = u64::from_le_bytes(data[64..72].try_into()?);
            let account = client.rpc(
                "getAccountInfo",
                json!([context.token_accounts[0],{"encoding":"base64","commitment":"processed"}]),
            )?;
            let data = BASE64_STANDARD.decode(
                account["value"]["data"][0]
                    .as_str()
                    .context("missing settlement")?,
            )?;
            let before = u64::from_le_bytes(data[64..72].try_into()?);
            let Some(gross) = after.checked_sub(before) else {
                continue;
            };
            if gross <= 10_000 {
                continue;
            }
            let units = simulation["value"]["unitsConsumed"]
                .as_u64()
                .context("missing simulation compute")?;
            let cu = ((units * 110 / 100 + 10_000).min(1_400_000)) as u32;
            let fee_budget = (gross - 5000) * (10_000 - strategy.keep_basis_points) / 10_000;
            let price = u64::try_from(u128::from(fee_budget) * 1_000_000 / u128::from(cu))?;
            let route_instruction = session.route(&instructions, &inputs)?;
            let wire =
                context.transaction(vec![route_instruction], &observation.blockhash, cu, price)?;
            let _ = client.rpc(
                "sendTransaction",
                json!([BASE64_STANDARD.encode(wire),{"encoding":"base64","skipPreflight":true}]),
            );
            break;
        }
    }
}

/// Each RPC batch is a native processed-state read; the market may change
/// between batches and between simulation and execution, just as for the bot.
fn observe(client: &Client, market: &Market) -> Result<Observation> {
    let addresses = market
        .pools
        .iter()
        .flat_map(|p| [p.address.clone(), p.vaults[0].clone(), p.vaults[1].clone()])
        .collect::<Vec<_>>();
    let mut accounts = vec![];
    for chunk in addresses.chunks(100) {
        let response = client.rpc(
            "getMultipleAccounts",
            json!([chunk,{"encoding":"base64","commitment":"processed"}]),
        )?;
        for account in response["value"].as_array().context("missing accounts")? {
            accounts.push(
                BASE64_STANDARD.decode(account["data"][0].as_str().context("missing data")?)?,
            );
        }
    }
    let mut pools = vec![];
    for (pool, a) in market.pools.iter().zip(accounts.chunks(3)) {
        let mut state = crate::sdk::PoolState {
            reserves: [
                u64::from_le_bytes(a[1][64..72].try_into()?),
                u64::from_le_bytes(a[2][64..72].try_into()?),
            ],
            ..Default::default()
        };
        if pool.kind == PoolKind::Concentrated {
            state.liquidity = u128::from_le_bytes(a[0][49..65].try_into()?).to_string();
            state.sqrt_price = u128::from_le_bytes(a[0][65..81].try_into()?).to_string();
            state.tick = i32::from_le_bytes(a[0][81..85].try_into()?);
        }
        pools.push(state);
    }
    let hash = client.rpc("getLatestBlockhash", json!([{"commitment":"processed"}]))?;
    Ok(Observation {
        pools,
        blockhash: hash["value"]["blockhash"]
            .as_str()
            .context("missing blockhash")?
            .into(),
        ..Default::default()
    })
}
