//! Routing (`specs/venues/router.md`): Jupiter-style routes across the
//! session's pools, served to a bot two ways.
//!
//! - **As a service.** The bot's guard answers Jupiter's v1 `GET /quote`
//!   and `POST /swap-instructions` at the bot's RPC URL, and the same as
//!   the JSON-RPC methods `mtx_routerQuote` and `mtx_routerSwapInstructions`
//!   (what a strategy component's `rpc` reaches). The bot sets its own
//!   slippage, signs and sends, as it would with Jupiter.
//! - **As a venue.** A component's `swap` line on the pseudo-venue
//!   `router` is routed the same way and sent as one transaction
//!   ([`RouterSeat`]); its limit becomes the route's least out.
//!
//! A scenario opts in ([`crate::spot::RouterSpec`]); without it neither
//! exists. A route crosses only the pools of the spot instruments in the
//! mandate's universe, so it reaches no venue the bot couldn't trade by
//! hand.
//!
//! **Search** ([`search`]) is a pure function of the pools as one read left
//! them ([`Loaded`]): every path of up to [`MAX_HOPS`] pools from the token
//! in to the token out, then the amount split across them greedily in
//! twentieths, each twentieth to the path whose output it raises most, at
//! most [`MAX_LEGS`] pools in all and no pool on two paths. Each quote is
//! the pool program's own arithmetic, so a quote is what the swap would
//! pay on the state it was read at. Ties go to the earlier path: fewer
//! hops, then the pools in the scenario's venue order and by address.
//!
//! **Execution** is the pools' own swap instructions in one transaction,
//! each leg exactly in. A leg after another spends what the one before
//! guarantees (its least out, the quote less the slippage), so the
//! transaction never fails for want of the token in between; whatever the
//! earlier leg pays above that stays in the wallet. Each path's last leg
//! carries its share of the route's least out. The transaction is a v0
//! one, naming the session's address lookup table, made at setup with the
//! pools' accounts.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use base64::Engine as _;
use mtx_core::mandate::InstrumentRef;
use mtx_kit::{
    Action, AmountUnit, Instruction, MarketKind, Planned, SendState, Sent, Side, VenueInfo,
    VenueView,
    chain::{ChainClient, lookup, token},
};
use serde_json::{Value, json};
use solana_keypair::Keypair;
use solana_message::{AddressLookupTableAccount, VersionedMessage};
use solana_pubkey::Pubkey;
use solana_rpc_client::rpc_client::RpcClient;
use solana_signer::Signer;
use solana_transaction::versioned::VersionedTransaction;
use surfpool_sdk::Surfnet;

use crate::venue::{
    Seat, SeatResult,
    pools::{Loaded, Pool, Quote, wallet_atoms},
    price_usd,
};

/// The pseudo-venue's name, as a line and the session's venues name it.
pub const VENUE: &str = "router";
/// The most pools one path crosses.
pub const MAX_HOPS: usize = 3;
/// The most pools one route crosses, all its paths together: what one
/// transaction holds, with the lookup table, under the compute limit.
pub const MAX_LEGS: usize = 4;
/// The amount is split in this many parts (5% each).
pub const SPLIT_STEPS: u64 = 20;
/// A transaction's compute limit.
pub const MAX_COMPUTE_UNITS: u32 = 1_400_000;
/// The slippage a route's legs allow when the asker gives none, in basis
/// points: Jupiter's default.
pub const DEFAULT_SLIPPAGE_BPS: u16 = 50;
/// Quotes and swap instructions one bot may ask for in a step.
pub const CALLS_PER_STEP: u32 = 64;
const BPS: u128 = 10_000;
/// Compute units for the route's own instructions: its token accounts.
const SETUP_COMPUTE_UNITS: u32 = 30_000;

/// One pool a route may cross, as read for one search.
pub struct RoutePool {
    /// Its spot instrument, as the mandate names it.
    pub instrument: InstrumentRef,
    pub pool: Arc<dyn Loaded>,
}

impl RoutePool {
    /// The token a swap of `input` pays out here, and whether `input` is
    /// the base; `None` if the pool doesn't trade `input`.
    fn other(&self, input: &Pubkey) -> Option<(Pubkey, bool)> {
        let assets = self.pool.assets();
        if *input == assets.base_mint {
            Some((assets.quote_mint, true))
        } else if *input == assets.quote_mint {
            Some((assets.base_mint, false))
        } else {
            None
        }
    }

    /// The token program of `mint`, one of the pool's.
    fn program_of(&self, mint: &Pubkey) -> Pubkey {
        let assets = self.pool.assets();
        if *mint == assets.base_mint {
            assets.base_program
        } else {
            assets.quote_program
        }
    }
}

/// One pool of a route: what goes in, what it is quoted to pay, and the
/// least it may.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Leg {
    /// The pool, by its place in the search's pools.
    pub pool: usize,
    pub input: Pubkey,
    pub output: Pubkey,
    pub quote: Quote,
    pub min_out: u64,
}

/// Pools one after another, each spending what the one before paid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Path {
    pub legs: Vec<Leg>,
}

impl Path {
    pub fn amount_in(&self) -> u64 {
        self.legs.first().map_or(0, |l| l.quote.amount_in)
    }

    pub fn amount_out(&self) -> u64 {
        self.legs.last().map_or(0, |l| l.quote.amount_out)
    }

    pub fn min_out(&self) -> u64 {
        self.legs.last().map_or(0, |l| l.min_out)
    }
}

/// A route: the amount split across paths, all in one transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    pub input: Pubkey,
    pub output: Pubkey,
    pub amount_in: u64,
    pub amount_out: u64,
    pub min_out: u64,
    pub slippage_bps: u16,
    pub paths: Vec<Path>,
}

impl Route {
    /// Give each path's last leg its share of `min_out`, the route's least
    /// out, in proportion to what the path is quoted to pay; the rounding
    /// goes to the path paying most.
    pub fn set_min_out(&mut self, min_out: u64) {
        let quoted = u128::from(self.amount_out.max(1));
        let shares: Vec<u64> = self
            .paths
            .iter()
            .map(|p| {
                u64::try_from(u128::from(min_out) * u128::from(p.amount_out()) / quoted)
                    .unwrap_or(u64::MAX)
            })
            .collect();
        let rest = min_out.saturating_sub(shares.iter().fold(0u64, |a, s| a.saturating_add(*s)));
        // The first of the paths paying most.
        let largest = (0..self.paths.len())
            .rev()
            .max_by_key(|i| self.paths[*i].amount_out())
            .unwrap_or(0);
        for (i, (path, share)) in self.paths.iter_mut().zip(shares).enumerate() {
            if let Some(last) = path.legs.last_mut() {
                last.min_out = if i == largest {
                    share.saturating_add(rest)
                } else {
                    share
                };
            }
        }
        self.min_out = min_out;
    }

    fn legs(&self) -> impl Iterator<Item = &Leg> {
        self.paths.iter().flat_map(|p| &p.legs)
    }
}

/// What a route is asked for: `amount` atoms of `input`, exactly, for as
/// much of `output` as it gets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub input: Pubkey,
    pub output: Pubkey,
    pub amount: u64,
    pub slippage_bps: u16,
    pub max_hops: usize,
}

/// Every path of up to `max_hops` pools from `input` to `output` that
/// crosses no pool or token twice: fewer hops first, then in the pools'
/// order.
fn candidates(
    pools: &[RoutePool],
    input: &Pubkey,
    output: &Pubkey,
    max_hops: usize,
) -> Vec<Vec<(usize, bool)>> {
    fn extend(
        pools: &[RoutePool],
        at: &Pubkey,
        output: &Pubkey,
        max_hops: usize,
        path: &mut Vec<(usize, bool)>,
        seen: &mut Vec<Pubkey>,
        found: &mut Vec<Vec<(usize, bool)>>,
    ) {
        if path.len() == max_hops {
            return;
        }
        for (i, pool) in pools.iter().enumerate() {
            let Some((next, base_in)) = pool.other(at) else {
                continue;
            };
            if path.iter().any(|(j, _)| *j == i) || seen.contains(&next) {
                continue;
            }
            path.push((i, base_in));
            if next == *output {
                found.push(path.clone());
            } else {
                seen.push(next);
                extend(pools, &next, output, max_hops, path, seen, found);
                seen.pop();
            }
            path.pop();
        }
    }
    let mut found = Vec::new();
    extend(
        pools,
        input,
        output,
        max_hops,
        &mut Vec::new(),
        &mut vec![*input],
        &mut found,
    );
    found.sort_by_key(Vec::len);
    found
}

/// `amount` atoms through `hops`, each leg exactly in, each after the
/// first spending the least the one before may pay: the quote less
/// `slippage_bps`.
fn run(
    pools: &[RoutePool],
    hops: &[(usize, bool)],
    amount: u64,
    slippage_bps: u16,
) -> SeatResult<Path> {
    let mut legs = Vec::with_capacity(hops.len());
    let mut amount = amount;
    for &(i, base_in) in hops {
        let pool = &pools[i];
        let quote = pool.pool.quote(base_in, amount, true)?;
        let min_out = u128::from(quote.amount_out) * (BPS - u128::from(slippage_bps)) / BPS;
        let min_out = u64::try_from(min_out).unwrap_or(u64::MAX);
        if min_out == 0 {
            return Err(format!(
                "{} pays nothing for {amount} atoms",
                pool.instrument
            ));
        }
        let assets = pool.pool.assets();
        let (input, output) = if base_in {
            (assets.base_mint, assets.quote_mint)
        } else {
            (assets.quote_mint, assets.base_mint)
        };
        legs.push(Leg {
            pool: i,
            input,
            output,
            quote,
            min_out,
        });
        amount = min_out;
    }
    Ok(Path { legs })
}

/// The best route for `request` through `pools`, as the module docs
/// describe: a pure function of the pools as read.
pub fn search(pools: &[RoutePool], request: &Request) -> SeatResult<Route> {
    if request.input == request.output {
        return Err("the token in and the token out are the same".into());
    }
    if request.amount == 0 {
        return Err("an amount of 0".into());
    }
    if u128::from(request.slippage_bps) >= BPS {
        return Err("slippage of 100% or more".into());
    }
    let max_hops = request.max_hops.clamp(1, MAX_HOPS);
    let paths = candidates(pools, &request.input, &request.output, max_hops);
    if paths.is_empty() {
        return Err(format!(
            "no pool path from {} to {} in this session",
            request.input, request.output
        ));
    }
    let steps = SPLIT_STEPS.min(request.amount);
    let part = |k: u64| {
        u64::try_from(u128::from(request.amount) * u128::from(k) / u128::from(steps))
            .expect("at most the amount")
    };
    let mut given = vec![0u64; paths.len()];
    let mut out = vec![0u64; paths.len()];
    let mut units = vec![0u32; paths.len()];
    let compute = |path: &Path| -> u32 {
        path.legs
            .iter()
            .map(|l| pools[l.pool].pool.compute_units(l.quote.crossed))
            .sum()
    };
    for step in 0..steps {
        let chunk = part(step + 1) - part(step);
        let used: Vec<usize> = (0..paths.len()).filter(|i| given[*i] > 0).collect();
        let legs: usize = used.iter().map(|i| paths[*i].len()).sum();
        let mut best: Option<(usize, u64, u64, u32)> = None;
        let spent: u32 = units.iter().sum();
        for (i, hops) in paths.iter().enumerate() {
            if given[i] == 0 {
                let shared = used.iter().any(|j| {
                    paths[*j]
                        .iter()
                        .any(|(p, _)| hops.iter().any(|(q, _)| p == q))
                });
                if shared || legs + hops.len() > MAX_LEGS {
                    continue;
                }
            }
            // Ranked on what the path is quoted to pay, before slippage.
            let Ok(path) = run(pools, hops, given[i] + chunk, 0) else {
                continue;
            };
            // The whole route must fit one transaction's compute.
            let needs = compute(&path);
            if spent - units[i] + needs + SETUP_COMPUTE_UNITS > MAX_COMPUTE_UNITS {
                continue;
            }
            let gain = path.amount_out().saturating_sub(out[i]);
            if best.is_none_or(|(_, best, _, _)| gain > best) {
                best = Some((i, gain, path.amount_out(), needs));
            }
        }
        let Some((i, _, paid, needs)) = best else {
            return Err(format!(
                "the pools can't take {} atoms: they run out of liquidity within the arrays a swap can name",
                request.amount
            ));
        };
        given[i] += chunk;
        out[i] = paid;
        units[i] = needs;
    }
    let mut route = Route {
        input: request.input,
        output: request.output,
        amount_in: request.amount,
        amount_out: 0,
        min_out: 0,
        slippage_bps: request.slippage_bps,
        paths: Vec::new(),
    };
    for (i, hops) in paths.iter().enumerate() {
        if given[i] > 0 {
            let path = run(pools, hops, given[i], request.slippage_bps)?;
            route.amount_out += path.amount_out();
            route.min_out += path.min_out();
            route.paths.push(path);
        }
    }
    Ok(route)
}

/// A route's transaction, by part, as Jupiter's `swap-instructions` gives
/// it.
pub struct Built {
    pub compute: Instruction,
    pub compute_units: u32,
    /// The token accounts its legs pay into, made where missing.
    pub setup: Vec<Instruction>,
    /// Each leg's swap, in order.
    pub swaps: Vec<Instruction>,
}

impl Built {
    pub fn instructions(&self) -> Vec<Instruction> {
        let mut all = vec![self.compute.clone()];
        all.extend(self.setup.iter().cloned());
        all.extend(self.swaps.iter().cloned());
        all
    }
}

/// `owner`'s transaction for `route` through `pools`.
pub fn build(route: &Route, pools: &[RoutePool], owner: &Pubkey) -> SeatResult<Built> {
    let mut units = SETUP_COMPUTE_UNITS;
    let mut setup = Vec::new();
    let mut swaps = Vec::new();
    let mut held = vec![route.input];
    for leg in route.legs() {
        let pool = pools
            .get(leg.pool)
            .ok_or("a leg names a pool the route doesn't have")?;
        if !held.contains(&leg.output) {
            setup.push(token::create_ata_idempotent_for(
                owner,
                owner,
                &leg.output,
                &pool.program_of(&leg.output),
            ));
            held.push(leg.output);
        }
        units = units.saturating_add(pool.pool.compute_units(leg.quote.crossed));
        swaps.push(pool.pool.swap(owner, &leg.quote, leg.min_out)?);
    }
    let units = units.min(MAX_COMPUTE_UNITS);
    Ok(Built {
        compute: crate::raw::compute_limit(units),
        compute_units: units,
        setup,
        swaps,
    })
}

/// A Jupiter v1 instruction.
fn instruction_json(ix: &Instruction) -> Value {
    json!({
        "programId": ix.program_id.to_string(),
        "accounts": ix.accounts.iter().map(|a| json!({
            "pubkey": a.pubkey.to_string(),
            "isSigner": a.is_signer,
            "isWritable": a.is_writable,
        })).collect::<Vec<_>>(),
        "data": base64::engine::general_purpose::STANDARD.encode(&ix.data),
    })
}

/// `route` as Jupiter's v1 `/quote` answers: `routePlan` path by path, the
/// first leg of each with its share of the amount, a later one with 100.
/// `impact` is against the reference prices.
pub fn quote_json(route: &Route, pools: &[RoutePool], slot: u64, impact: Option<f64>) -> Value {
    let share = |amount: u64, of: u128| u128::from(amount) * of / u128::from(route.amount_in);
    let plan: Vec<Value> = route
        .paths
        .iter()
        .flat_map(|path| {
            path.legs.iter().enumerate().map(move |(k, leg)| {
                let pool = &pools[leg.pool];
                let (percent, bps) = if k == 0 {
                    (
                        share(leg.quote.amount_in, 100),
                        share(leg.quote.amount_in, BPS),
                    )
                } else {
                    (100, BPS)
                };
                json!({
                    "swapInfo": {
                        "ammKey": pool.pool.assets().address.to_string(),
                        "label": pool.pool.label(),
                        "inputMint": leg.input.to_string(),
                        "outputMint": leg.output.to_string(),
                        "inAmount": leg.quote.amount_in.to_string(),
                        "outAmount": leg.quote.amount_out.to_string(),
                        "feeAmount": leg.quote.fee.to_string(),
                        "feeMint": leg.quote.fee_mint.to_string(),
                    },
                    "percent": percent,
                    "bps": bps,
                })
            })
        })
        .collect();
    json!({
        "inputMint": route.input.to_string(),
        "inAmount": route.amount_in.to_string(),
        "outputMint": route.output.to_string(),
        "outAmount": route.amount_out.to_string(),
        "otherAmountThreshold": route.min_out.to_string(),
        "swapMode": "ExactIn",
        "slippageBps": route.slippage_bps,
        "platformFee": null,
        "priceImpactPct": impact.map_or_else(|| "0".to_string(), |i| format!("{i:.6}")),
        "routePlan": plan,
        "contextSlot": slot,
    })
}

fn field<'a>(value: &'a Value, name: &str) -> SeatResult<&'a Value> {
    value
        .get(name)
        .filter(|v| !v.is_null())
        .ok_or_else(|| format!("{name} is missing"))
}

fn mint_field(value: &Value, name: &str) -> SeatResult<Pubkey> {
    field(value, name)?
        .as_str()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| format!("{name} is not a base58 address"))
}

/// An amount as Jupiter writes them: a string of digits, or a number.
fn atoms_field(value: &Value, name: &str) -> SeatResult<u64> {
    let v = field(value, name)?;
    v.as_u64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        .ok_or_else(|| format!("{name} is not a whole number of atoms"))
}

/// The route a `/quote` answer describes, re-quoted leg by leg on `pools`
/// as they are now: each leg spends its `inAmount`; a leg before another
/// may pay no less than the next one's `inAmount`, and the paths' last legs
/// share `otherAmountThreshold`, as their `outAmount`s do. Only pools of
/// `pools` may appear: a route goes nowhere the router doesn't.
pub fn route_from_quote(quote: &Value, pools: &[RoutePool]) -> SeatResult<Route> {
    if quote
        .get("swapMode")
        .and_then(Value::as_str)
        .unwrap_or("ExactIn")
        != "ExactIn"
    {
        return Err("the router routes ExactIn swaps only".into());
    }
    let input = mint_field(quote, "inputMint")?;
    let output = mint_field(quote, "outputMint")?;
    let amount_in = atoms_field(quote, "inAmount")?;
    let min_out = atoms_field(quote, "otherAmountThreshold")?;
    let slippage_bps = quote
        .get("slippageBps")
        .and_then(Value::as_u64)
        .and_then(|s| u16::try_from(s).ok())
        .unwrap_or(DEFAULT_SLIPPAGE_BPS);
    let plan = field(quote, "routePlan")?
        .as_array()
        .ok_or("routePlan is not a list")?;
    if plan.is_empty() || plan.len() > MAX_LEGS {
        return Err(format!(
            "a route has 1 to {MAX_LEGS} legs; this one has {}",
            plan.len()
        ));
    }
    // Each step's pool, which way, its amounts and the token it pays,
    // grouped into paths.
    type Step = (usize, bool, u64, u64, Pubkey);
    let mut steps: Vec<Vec<Step>> = Vec::new();
    for step in plan {
        let info = field(step, "swapInfo")?;
        let amm = mint_field(info, "ammKey")?;
        let (i, pool) = pools
            .iter()
            .enumerate()
            .find(|(_, p)| p.pool.assets().address == amm)
            .ok_or_else(|| format!("{amm} is not a pool this session's router routes through"))?;
        let leg_in = mint_field(info, "inputMint")?;
        let leg_out = mint_field(info, "outputMint")?;
        let base_in = pool
            .other(&leg_in)
            .filter(|(other, _)| *other == leg_out)
            .map(|(_, base_in)| base_in)
            .ok_or_else(|| format!("{amm} doesn't trade {leg_in} for {leg_out}"))?;
        let entry = (
            i,
            base_in,
            atoms_field(info, "inAmount")?,
            atoms_field(info, "outAmount")?,
            leg_out,
        );
        if leg_in == input {
            steps.push(vec![entry]);
        } else {
            match steps.last_mut() {
                Some(path) if path.last().is_some_and(|prev| prev.4 == leg_in) => path.push(entry),
                _ => {
                    return Err(format!(
                        "a leg spends {leg_in}, which the leg before it doesn't pay"
                    ));
                }
            }
        }
    }
    if steps
        .iter()
        .any(|path| path.last().is_none_or(|l| l.4 != output))
    {
        return Err(format!("a path ends in a token other than {output}"));
    }
    let spent: u128 = steps.iter().map(|p| u128::from(p[0].2)).sum();
    if spent != u128::from(amount_in) {
        return Err(format!(
            "the paths spend {spent} atoms, not the route's inAmount {amount_in}"
        ));
    }
    let mut route = Route {
        input,
        output,
        amount_in,
        amount_out: 0,
        min_out: 0,
        slippage_bps,
        paths: Vec::new(),
    };
    for path in &steps {
        let mut legs = Vec::new();
        for (k, &(i, base_in, leg_in, quoted_out, _)) in path.iter().enumerate() {
            let quote = pools[i].pool.quote(base_in, leg_in, true)?;
            let assets = pools[i].pool.assets();
            let (input, output) = if base_in {
                (assets.base_mint, assets.quote_mint)
            } else {
                (assets.quote_mint, assets.base_mint)
            };
            let quote = Quote {
                amount_out: quoted_out,
                ..quote
            };
            let min_out = path.get(k + 1).map_or(0, |next| next.2);
            legs.push(Leg {
                pool: i,
                input,
                output,
                quote,
                min_out,
            });
        }
        let path = Path { legs };
        route.amount_out += path.amount_out();
        route.paths.push(path);
    }
    route.set_min_out(min_out);
    Ok(route)
}

/// `built` as Jupiter's v1 `/swap-instructions` answers, `table` the
/// lookup table its legs' accounts are in. A route of one leg has it as
/// `swapInstruction`; every route has its legs, in order, as
/// `swapInstructions` (Jupiter's own program makes any route one
/// instruction; these are the pools' own).
pub fn swap_instructions_json(built: &Built, table: Option<&Pubkey>) -> Value {
    let swaps: Vec<Value> = built.swaps.iter().map(instruction_json).collect();
    json!({
        "tokenLedgerInstruction": null,
        "computeBudgetInstructions": [instruction_json(&built.compute)],
        "setupInstructions": built.setup.iter().map(instruction_json).collect::<Vec<_>>(),
        "swapInstruction": if swaps.len() == 1 { swaps[0].clone() } else { Value::Null },
        "swapInstructions": swaps,
        "cleanupInstruction": null,
        "otherInstructions": [],
        "addressLookupTableAddresses": table.map(|t| vec![t.to_string()]).unwrap_or_default(),
        "computeUnitLimit": built.compute_units,
        "prioritizationFeeLamports": 0,
    })
}

/// Addresses one lookup table holds.
pub const MAX_TABLE_ADDRESSES: usize = 256;
/// Addresses one extension adds: what fits a transaction.
const EXTEND_BY: usize = 20;

/// The session's lookup table for its routes, made at setup by
/// `authority`, a grader key funded here, holding the first
/// [`MAX_TABLE_ADDRESSES`] of `accounts`. Its address follows from the key
/// and the slot, so a replay makes the same one.
pub fn lookup_table(
    surfnet: &Surfnet,
    rpc: &RpcClient,
    authority: &Keypair,
    mut accounts: Vec<Pubkey>,
) -> crate::Result<AddressLookupTableAccount> {
    accounts.truncate(MAX_TABLE_ADDRESSES);
    let key = authority.pubkey();
    surfnet.cheatcodes().fund_sol(&key, 1_000_000_000)?;
    // The newest slot the SlotHashes sysvar holds: one the program accepts
    // as recent, whatever the stepped world's own slot is.
    let hashes = rpc.get_account(&solana_pubkey::pubkey!(
        "SysvarS1otHashes111111111111111111111111111"
    ))?;
    let recent = hashes
        .data
        .get(8..16)
        .and_then(|b| b.try_into().ok())
        .map(u64::from_le_bytes)
        .ok_or_else(|| crate::Error::Transaction {
            label: "the router's lookup table".into(),
            reason: "the SlotHashes sysvar holds no slot".into(),
        })?;
    let (create, address) = lookup::create(&key, &key, recent);
    crate::spot::send_signed(
        rpc,
        authority,
        None,
        vec![create],
        "the router's lookup table",
    )?;
    for chunk in accounts.chunks(EXTEND_BY) {
        let extend = lookup::extend(&address, &key, &key, chunk.to_vec());
        crate::spot::send_signed(
            rpc,
            authority,
            None,
            vec![extend],
            "the router's lookup table",
        )?;
    }
    Ok(AddressLookupTableAccount {
        key: address,
        addresses: accounts,
    })
}

/// The session's router: its pools, read through the grader's endpoint,
/// and the lookup table its routes name.
pub struct Router {
    /// The spot instruments it routes through and their pools, in the
    /// scenario's venue order, then by address.
    pools: Vec<(InstrumentRef, Arc<dyn Pool>)>,
    chain: Arc<dyn ChainClient>,
    table: Option<AddressLookupTableAccount>,
    /// The bot's key: the only one it builds transactions for.
    owner: Pubkey,
    max_hops: usize,
    /// The slippage the pseudo-venue's routes allow a leg before another.
    slippage_bps: u16,
}

/// A route found, and the pools as read for it.
pub struct Found {
    pub route: Route,
    pub pools: Vec<RoutePool>,
    pub slot: u64,
}

impl Router {
    pub fn new(
        mut pools: Vec<(InstrumentRef, Arc<dyn Pool>)>,
        venues: &[String],
        chain: Arc<dyn ChainClient>,
        owner: Pubkey,
        max_hops: usize,
        slippage_bps: u16,
    ) -> Self {
        let rank = |i: &InstrumentRef| venues.iter().position(|v| *v == i.venue);
        pools.sort_by_key(|(i, p)| (rank(i), p.assets().address));
        Self {
            pools,
            chain,
            table: None,
            owner,
            max_hops,
            slippage_bps,
        }
    }

    /// Name `table` in every transaction it builds.
    pub fn with_table(mut self, table: AddressLookupTableAccount) -> Self {
        self.table = Some(table);
        self
    }

    pub fn table(&self) -> Option<&AddressLookupTableAccount> {
        self.table.as_ref()
    }

    /// Every account its routes may name, for the lookup table: each
    /// pool's, as read now.
    pub async fn accounts(&self) -> crate::Result<Vec<Pubkey>> {
        let mut accounts = Vec::new();
        for (_, pool) in &self.pools {
            for account in pool.load(self.chain.as_ref()).await?.accounts() {
                if !accounts.contains(&account) {
                    accounts.push(account);
                }
            }
        }
        Ok(accounts)
    }

    /// Each pool as read now, but those `keep` turns away.
    async fn load(&self, keep: impl Fn(&str) -> bool) -> SeatResult<(Vec<RoutePool>, u64)> {
        let slot = self.chain.get_slot().await.map_err(|e| e.to_string())?;
        let mut pools = Vec::new();
        for (instrument, pool) in &self.pools {
            let loaded = pool
                .load(self.chain.as_ref())
                .await
                .map_err(|e| e.to_string())?;
            if keep(loaded.label()) {
                pools.push(RoutePool {
                    instrument: instrument.clone(),
                    pool: loaded,
                });
            }
        }
        Ok((pools, slot))
    }

    /// The best route for `request` on the pools as they are now, through
    /// the pools whose label `keep` keeps.
    pub async fn route(&self, request: &Request, keep: impl Fn(&str) -> bool) -> SeatResult<Found> {
        let (pools, slot) = self.load(keep).await?;
        let request = Request {
            max_hops: request.max_hops.min(self.max_hops),
            ..request.clone()
        };
        let route = search(&pools, &request)?;
        Ok(Found { route, pools, slot })
    }

    /// The route's price impact against the reference prices: what it pays
    /// short of what the amount is worth. `None` where a token has no
    /// reference price.
    async fn impact(&self, found: &Found) -> Option<f64> {
        let decimals = |mint: &Pubkey| {
            found.pools.iter().find_map(|p| {
                let a = p.pool.assets();
                if a.base_mint == *mint {
                    Some(a.base.decimals)
                } else if a.quote_mint == *mint {
                    Some(a.quote.decimals)
                } else {
                    None
                }
            })
        };
        let route = &found.route;
        let worth = |atoms: u64, mint: &Pubkey, price: mtx_core::Decimal| {
            let units = atoms as f64 / 10f64.powi(i32::from(decimals(mint)?));
            Some(units * crate::session::grader_f64(price))
        };
        let price_in = price_usd(self.chain.as_ref(), &route.input).await.ok()?;
        let price_out = price_usd(self.chain.as_ref(), &route.output).await.ok()?;
        let paid = worth(route.amount_in, &route.input, price_in)?;
        let got = worth(route.amount_out, &route.output, price_out)?;
        (paid > 0.0).then(|| 1.0 - got / paid)
    }

    /// Jupiter's `GET /quote`, from its query parameters (or the same as a
    /// JSON object): `inputMint`, `outputMint`, `amount`, and optionally
    /// `slippageBps`, `swapMode` (ExactIn only), `onlyDirectRoutes`,
    /// `maxHops`, `dexes` and `excludeDexes` (pool labels, comma
    /// separated).
    pub async fn quote_api(&self, params: &Value) -> SeatResult<Value> {
        if params
            .get("swapMode")
            .and_then(Value::as_str)
            .unwrap_or("ExactIn")
            != "ExactIn"
        {
            return Err("the router routes ExactIn swaps only".into());
        }
        let flag = |name: &str| {
            params
                .get(name)
                .is_some_and(|v| v.as_bool() == Some(true) || v.as_str() == Some("true"))
        };
        let number = |name: &str| {
            params.get(name).and_then(|v| {
                v.as_u64()
                    .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
            })
        };
        let labels = |name: &str| -> Option<Vec<String>> {
            params.get(name).and_then(Value::as_str).map(|s| {
                s.split(',')
                    .map(|l| l.trim().to_string())
                    .filter(|l| !l.is_empty())
                    .collect()
            })
        };
        let slippage_bps = match number("slippageBps") {
            Some(bps) => u16::try_from(bps).map_err(|_| "slippageBps is too big")?,
            None => DEFAULT_SLIPPAGE_BPS,
        };
        let max_hops = if flag("onlyDirectRoutes") {
            1
        } else {
            number("maxHops").map_or(MAX_HOPS, |h| h as usize)
        };
        let request = Request {
            input: mint_field(params, "inputMint")?,
            output: mint_field(params, "outputMint")?,
            amount: atoms_field(params, "amount")?,
            slippage_bps,
            max_hops,
        };
        let (only, except) = (labels("dexes"), labels("excludeDexes"));
        let keep = |label: &str| {
            only.as_ref().is_none_or(|l| l.iter().any(|x| x == label))
                && except
                    .as_ref()
                    .is_none_or(|l| !l.iter().any(|x| x == label))
        };
        let found = self.route(&request, keep).await?;
        let impact = self.impact(&found).await;
        Ok(quote_json(&found.route, &found.pools, found.slot, impact))
    }

    /// Jupiter's `POST /swap-instructions`: `quoteResponse` (a `/quote`
    /// answer, as given or edited) and `userPublicKey`, which must be the
    /// bot's.
    pub async fn swap_instructions_api(&self, body: &Value) -> SeatResult<Value> {
        let user = mint_field(body, "userPublicKey")?;
        if user != self.owner {
            return Err("userPublicKey must be this session's bot".into());
        }
        let (pools, _) = self.load(|_| true).await?;
        let route = route_from_quote(field(body, "quoteResponse")?, &pools)?;
        let built = build(&route, &pools, &self.owner)?;
        Ok(swap_instructions_json(
            &built,
            self.table.as_ref().map(|t| &t.key),
        ))
    }
}

/// The pseudo-venue `router` as a component's seat: a `swap` line routed
/// and sent as one transaction, signed with the bot's key.
pub struct RouterSeat {
    router: Arc<Router>,
    signer: Keypair,
    /// The bot's endpoint, the guard's host side.
    chain: Arc<dyn ChainClient>,
    /// The session's pair.
    base: (Pubkey, u8),
    quote: (Pubkey, u8),
    /// What the last sends did, for the next step's notes.
    notes: Mutex<Vec<String>>,
}

impl RouterSeat {
    pub fn new(
        router: Arc<Router>,
        signer: Keypair,
        chain: Arc<dyn ChainClient>,
        base: (Pubkey, u8),
        quote: (Pubkey, u8),
    ) -> Self {
        Self {
            router,
            signer,
            chain,
            base,
            quote,
            notes: Mutex::default(),
        }
    }

    /// The venue as a session lists it.
    pub fn info() -> VenueInfo {
        VenueInfo {
            id: VENUE.into(),
            kind: MarketKind::Spot,
            address: None,
            tradable: true,
        }
    }

    /// A line's swap as a route request and its least out: a sale of base
    /// units, or a purchase for quote units (both exactly in).
    fn request(&self, action: &Action) -> SeatResult<(Request, u64)> {
        let scale = |units: f64, decimals: u8| units * 10f64.powi(i32::from(decimals));
        let whole = |atoms: f64| -> SeatResult<u64> {
            (atoms.is_finite() && atoms >= 0.0 && atoms < u64::MAX as f64)
                .then_some(atoms.floor() as u64)
                .ok_or_else(|| "an amount no token can hold".to_string())
        };
        let (side, amount, unit, limit) = match *action {
            Action::Swap {
                side,
                amount,
                unit,
                limit,
            } => (side, amount, unit, limit),
            Action::Market { side, size, limit } => (side, size, AmountUnit::Base, limit),
            _ => return Err("the router routes swaps".into()),
        };
        if !(amount.is_finite() && amount > 0.0 && limit.is_finite() && limit >= 0.0) {
            return Err("a swap needs a positive amount and a limit".into());
        }
        let (input, output, atoms, min_out) = match (side, unit) {
            (Side::Ask, AmountUnit::Base) => (
                self.base.0,
                self.quote.0,
                whole(scale(amount, self.base.1))?,
                whole(scale(amount * limit, self.quote.1))?,
            ),
            (Side::Bid, AmountUnit::Quote) if limit > 0.0 => (
                self.quote.0,
                self.base.0,
                whole(scale(amount, self.quote.1))?,
                whole(scale(amount / limit, self.base.1))?,
            ),
            _ => {
                return Err(
                    "the router routes exact-in swaps: sell base units, or buy for quote units with a positive limit"
                        .into(),
                );
            }
        };
        if atoms == 0 {
            return Err("rounds to nothing in the token's atoms".into());
        }
        Ok((
            Request {
                input,
                output,
                amount: atoms,
                slippage_bps: self.router.slippage_bps,
                max_hops: MAX_HOPS,
            },
            min_out,
        ))
    }

    /// Route, sign and send `action`: the route's signature, and a note of
    /// its paths.
    async fn route_and_send(&self, action: &Action) -> SeatResult<(mtx_kit::Signature, String)> {
        let (request, min_out) = self.request(action)?;
        let mut found = self.router.route(&request, |_| true).await?;
        found.route.set_min_out(min_out);
        let owner = self.signer.pubkey();
        let built = build(&found.route, &found.pools, &owner)?;
        let blockhash = self
            .chain
            .get_latest_blockhash()
            .await
            .map_err(|e| e.to_string())?;
        let tables: Vec<AddressLookupTableAccount> =
            self.router.table().cloned().into_iter().collect();
        let message: VersionedMessage =
            crate::raw::compile(owner, &built.instructions(), &tables, blockhash)?;
        let tx = VersionedTransaction::try_new(message, &[&self.signer])
            .map_err(|e| format!("signing the route: {e}"))?;
        let note = note(&found.route, &found.pools);
        let signature = self.chain.send(&tx).await.map_err(|e| e.to_string())?;
        Ok((signature, note))
    }
}

/// A route's paths as a step's note: each path's share and its pools.
pub fn note(route: &Route, pools: &[RoutePool]) -> String {
    let paths: Vec<String> = route
        .paths
        .iter()
        .map(|path| {
            let share = u128::from(path.amount_in()) * 100 / u128::from(route.amount_in.max(1));
            let legs: Vec<String> = path
                .legs
                .iter()
                .map(|l| pools[l.pool].instrument.to_string())
                .collect();
            format!("{share}% {}", legs.join(" > "))
        })
        .collect();
    format!(
        "router: {} in, {} out quoted, at least {}: {}",
        route.amount_in,
        route.amount_out,
        route.min_out,
        paths.join("; ")
    )
}

#[async_trait]
impl Seat for RouterSeat {
    fn market(&self) -> &str {
        VENUE
    }

    fn executes(&self, action: &Action) -> bool {
        matches!(
            action,
            Action::Swap {
                side: Side::Ask,
                unit: AmountUnit::Base,
                ..
            } | Action::Swap {
                side: Side::Bid,
                unit: AmountUnit::Quote,
                ..
            } | Action::Market {
                side: Side::Ask,
                ..
            }
        )
    }

    async fn send(&self, planned: &Planned) -> Sent {
        if !self.executes(&planned.action) {
            return Sent {
                state: SendState::NotSent("the router routes exact-in swaps".into()),
                sent: false,
            };
        }
        match self.route_and_send(&planned.action).await {
            Ok((signature, note)) => {
                self.notes.lock().expect("notes").push(note);
                let state = match self.chain.get_transaction(&signature).await {
                    Ok(Some(summary)) => match summary.error {
                        None => SendState::Landed(signature.to_string()),
                        Some(error) => SendState::Failed {
                            reason: error.to_string(),
                            signature: Some(signature.to_string()),
                        },
                    },
                    Ok(None) | Err(_) => SendState::Delayed,
                };
                Sent { state, sent: true }
            }
            Err(reason) => Sent {
                state: SendState::failed(reason),
                sent: true,
            },
        }
    }

    /// The session's base asset in the wallet.
    async fn position(&self) -> SeatResult<f64> {
        wallet_atoms(self.chain.as_ref(), &self.signer.pubkey(), &self.base.0)
            .await
            .map(|atoms| atoms as f64 / 10f64.powi(i32::from(self.base.1)))
            .map_err(|e| e.to_string())
    }

    /// How many pools it routes through.
    async fn view(&self, view: &mut VenueView) -> SeatResult {
        view.stats = vec![("pools".into(), self.router.pools.len() as f64)];
        Ok(())
    }

    fn notes(&self) -> Vec<String> {
        std::mem::take(&mut *self.notes.lock().expect("notes"))
    }
}

#[cfg(test)]
mod tests {
    use mtx_core::{Asset, mandate::Kind};
    use solana_transaction::AccountMeta;

    use super::*;
    use crate::venue::pools::PoolAssets;

    /// A constant-product pool with a fee on what goes in, in atoms.
    struct Product {
        assets: PoolAssets,
        base: u64,
        quote: u64,
        fee_bps: u64,
    }

    impl Loaded for Product {
        fn assets(&self) -> &PoolAssets {
            &self.assets
        }

        fn label(&self) -> &'static str {
            "Product"
        }

        fn quote(&self, base_in: bool, amount: u64, exact_in: bool) -> SeatResult<Quote> {
            assert!(exact_in, "routes are exact in");
            let (x, y) = if base_in {
                (self.base, self.quote)
            } else {
                (self.quote, self.base)
            };
            let fee = (u128::from(amount) * u128::from(self.fee_bps) / 10_000) as u64;
            let net = u128::from(amount - fee);
            let out = u128::from(y) * net / (u128::from(x) + net);
            if out == 0 || out >= u128::from(y) {
                return Err("no liquidity".into());
            }
            Ok(Quote {
                base_in,
                exact_in,
                amount_in: amount,
                amount_out: out as u64,
                fee,
                fee_mint: if base_in {
                    self.assets.base_mint
                } else {
                    self.assets.quote_mint
                },
                crossed: 0,
            })
        }

        fn swap(&self, owner: &Pubkey, quote: &Quote, threshold: u64) -> SeatResult<Instruction> {
            let mut data = vec![u8::from(quote.base_in)];
            data.extend_from_slice(&quote.amount_in.to_le_bytes());
            data.extend_from_slice(&threshold.to_le_bytes());
            Ok(Instruction {
                program_id: self.assets.address,
                accounts: vec![AccountMeta::new_readonly(*owner, true)],
                data,
            })
        }

        fn accounts(&self) -> Vec<Pubkey> {
            vec![self.assets.address]
        }

        fn compute_units(&self, _: u32) -> u32 {
            100_000
        }
    }

    fn mint(n: u8) -> Pubkey {
        Pubkey::new_from_array([n; 32])
    }

    fn pool(venue: &str, n: u8, base: u8, quote: u8, depth: (u64, u64), fee_bps: u64) -> RoutePool {
        let asset = |m: u8| Asset::new(mint(m).to_string(), format!("T{m}"), 6).unwrap();
        RoutePool {
            instrument: InstrumentRef {
                venue: venue.into(),
                kind: Kind::Spot,
                symbol: format!("T{base}-T{quote}"),
            },
            pool: Arc::new(Product {
                assets: PoolAssets {
                    address: mint(100 + n),
                    base: asset(base),
                    quote: asset(quote),
                    base_mint: mint(base),
                    quote_mint: mint(quote),
                    base_program: token::token_program(),
                    quote_program: token::token_program(),
                    base_vault: mint(150 + n),
                    quote_vault: mint(200 + n),
                },
                base: depth.0,
                quote: depth.1,
                fee_bps,
            }),
        }
    }

    fn request(input: u8, output: u8, amount: u64) -> Request {
        Request {
            input: mint(input),
            output: mint(output),
            amount,
            slippage_bps: 50,
            max_hops: MAX_HOPS,
        }
    }

    // Tokens: 1 the base (SPCX), 2 USDC, 3 SOL.
    fn session() -> Vec<RoutePool> {
        vec![
            pool("raydium", 0, 1, 2, (10_000_000_000, 1_400_000_000_000), 25),
            pool("meteora", 1, 1, 2, (3_000_000_000, 420_000_000_000), 10),
            pool("meteora", 2, 1, 3, (2_000_000_000, 3_500_000_000_000), 10),
            pool(
                "meteora",
                3,
                3,
                2,
                (1_000_000_000_000_000, 80_000_000_000_000),
                5,
            ),
        ]
    }

    #[test]
    fn a_small_order_takes_the_best_single_path_and_a_big_one_splits() {
        let pools = session();
        // Too small to move either pool: all of it to the cheaper fee.
        let direct: Vec<RoutePool> = session().into_iter().take(2).collect();
        let small = search(&direct, &request(1, 2, 1_000_000)).unwrap();
        assert_eq!(small.paths.len(), 1);
        assert_eq!(small.paths[0].legs[0].pool, 1);
        assert_eq!(small.amount_in, 1_000_000);

        let big = search(&pools, &request(1, 2, 500_000_000)).unwrap();
        assert!(big.paths.len() > 1, "{big:?}");
        assert_eq!(
            big.paths.iter().map(Path::amount_in).sum::<u64>(),
            500_000_000
        );
        // Each path's share is a whole number of twentieths.
        for path in &big.paths {
            assert_eq!(path.amount_in() % 25_000_000, 0);
        }
        // Splitting pays at least what the best single pool would.
        let alone: u64 = (0..2)
            .map(|i| {
                pools[i]
                    .pool
                    .quote(true, 500_000_000, true)
                    .unwrap()
                    .amount_out
            })
            .max()
            .unwrap();
        assert!(big.amount_out >= alone);
        let legs: usize = big.paths.iter().map(|p| p.legs.len()).sum();
        assert!(legs <= MAX_LEGS);
        // No pool on two paths.
        let mut used: Vec<usize> = big
            .paths
            .iter()
            .flat_map(|p| p.legs.iter().map(|l| l.pool))
            .collect();
        let all = used.len();
        used.dedup();
        assert_eq!(used.len(), all);
    }

    #[test]
    fn a_hop_spends_what_the_leg_before_guarantees() {
        // Only the route through SOL.
        let pools: Vec<RoutePool> = session().into_iter().skip(2).collect();
        let route = search(&pools, &request(1, 2, 10_000_000)).unwrap();
        let [path] = route.paths.as_slice() else {
            panic!("{route:?}");
        };
        let [first, second] = path.legs.as_slice() else {
            panic!("{path:?}");
        };
        assert_eq!(first.output, mint(3));
        assert_eq!(
            first.min_out,
            first.quote.amount_out * (10_000 - 50) / 10_000
        );
        assert_eq!(second.quote.amount_in, first.min_out);
        assert_eq!(route.min_out, second.min_out);
        assert!(
            search(
                &pools,
                &Request {
                    max_hops: 1,
                    ..request(1, 2, 10)
                }
            )
            .is_err()
        );
    }

    #[test]
    fn the_search_is_a_pure_function_of_the_pools() {
        let a = search(&session(), &request(1, 2, 123_456_789)).unwrap();
        let b = search(&session(), &request(1, 2, 123_456_789)).unwrap();
        assert_eq!(a, b);
        // Two equal pools: the first in order takes the whole small order.
        let twins = vec![
            pool("raydium", 0, 1, 2, (1_000_000_000, 140_000_000_000), 25),
            pool("meteora", 1, 1, 2, (1_000_000_000, 140_000_000_000), 25),
        ];
        let route = search(&twins, &request(1, 2, 1_000)).unwrap();
        assert_eq!(route.paths[0].legs[0].pool, 0);
    }

    #[test]
    fn nonsense_requests_are_refused() {
        let pools = session();
        assert!(search(&pools, &request(1, 1, 10)).is_err());
        assert!(search(&pools, &request(1, 2, 0)).is_err());
        assert!(search(&pools, &request(1, 9, 10)).is_err());
        assert!(
            search(
                &pools,
                &Request {
                    slippage_bps: 10_000,
                    ..request(1, 2, 10)
                }
            )
            .is_err()
        );
    }

    #[test]
    fn the_least_out_is_shared_by_what_each_path_pays() {
        let mut route = search(&session(), &request(1, 2, 500_000_000)).unwrap();
        let wanted = route.amount_out - 12_345;
        route.set_min_out(wanted);
        assert_eq!(route.paths.iter().map(Path::min_out).sum::<u64>(), wanted);
        for path in &route.paths {
            assert!(path.min_out() <= path.amount_out());
        }
    }

    #[test]
    fn a_quote_round_trips_into_the_same_route_and_only_through_its_pools() {
        let pools = session();
        let route = search(&pools, &request(1, 2, 500_000_000)).unwrap();
        let json = quote_json(&route, &pools, 7, Some(0.001));
        assert_eq!(json["inAmount"], "500000000");
        assert_eq!(json["contextSlot"], 7);
        let percents: u64 = json["routePlan"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|s| s["swapInfo"]["inputMint"] == mint(1).to_string())
            .map(|s| s["percent"].as_u64().unwrap())
            .sum();
        assert_eq!(percents, 100);
        let back = route_from_quote(&json, &pools).unwrap();
        assert_eq!(back.paths.len(), route.paths.len());
        for (a, b) in back.paths.iter().zip(&route.paths) {
            for (x, y) in a.legs.iter().zip(&b.legs) {
                assert_eq!((x.pool, x.quote.amount_in), (y.pool, y.quote.amount_in));
                assert_eq!(x.min_out, y.min_out);
            }
        }
        // An edited threshold is what the legs enforce.
        let mut edited = json.clone();
        edited["otherAmountThreshold"] = json!("0");
        let loose = route_from_quote(&edited, &pools).unwrap();
        assert_eq!(loose.min_out, 0);
        // A pool the router doesn't route through, or amounts that don't add
        // up, are refused.
        let mut foreign = json.clone();
        foreign["routePlan"][0]["swapInfo"]["ammKey"] = json!(mint(77).to_string());
        assert!(route_from_quote(&foreign, &pools).is_err());
        let mut short = json.clone();
        short["inAmount"] = json!("1");
        assert!(route_from_quote(&short, &pools).is_err());
        let mut exact_out = json;
        exact_out["swapMode"] = json!("ExactOut");
        assert!(route_from_quote(&exact_out, &pools).is_err());
    }

    #[test]
    fn a_routes_transaction_makes_the_accounts_it_pays_into() {
        let pools: Vec<RoutePool> = session().into_iter().skip(2).collect();
        let route = search(&pools, &request(1, 2, 10_000_000)).unwrap();
        let owner = mint(42);
        let built = build(&route, &pools, &owner).unwrap();
        // SOL in between and USDC out: two accounts made, two swaps.
        assert_eq!(built.setup.len(), 2);
        assert_eq!(built.swaps.len(), 2);
        assert_eq!(built.compute_units, SETUP_COMPUTE_UNITS + 200_000);
        let json = swap_instructions_json(&built, Some(&mint(9)));
        assert!(json["swapInstruction"].is_null());
        assert_eq!(json["swapInstructions"].as_array().unwrap().len(), 2);
        assert_eq!(json["addressLookupTableAddresses"][0], mint(9).to_string());
        assert!(note(&route, &pools).contains("100% meteora:spot:T1-T3 > meteora:spot:T3-T2"));
    }

    /// The realism gate: on the route snapshot, our route for each size
    /// Jupiter quoted at the fork (restricted to the venues we copy) pays
    /// within its tolerance of Jupiter's: 5 bps under 1% of the pools'
    /// value, 20 above.
    #[tokio::test(flavor = "multi_thread")]
    async fn routes_pay_within_reach_of_jupiters_on_the_same_pools() {
        use mtx_kit::chain::RpcChainClient;

        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../snapshots/spcx-route-2026-11-13")
            .canonicalize()
            .unwrap();
        let (surfnet, _) = crate::snapshot::start_snapshot_stepped(&dir).await.unwrap();
        let internal = crate::guard::Guard::internal(surfnet.rpc_url())
            .await
            .unwrap();
        let chain: Arc<dyn ChainClient> = Arc::new(RpcChainClient::new(internal.url()));
        let mut pools = Vec::new();
        for venue in crate::spot::POOLS {
            for named in crate::venue::books::named_markets(venue).unwrap() {
                let pool = crate::venue::pools::connect(venue, chain.as_ref(), named)
                    .await
                    .unwrap();
                pools.push(RoutePool {
                    instrument: InstrumentRef {
                        venue: venue.into(),
                        kind: Kind::Spot,
                        symbol: named.symbol.into(),
                    },
                    pool: pool.load(chain.as_ref()).await.unwrap(),
                });
            }
        }
        let recorded: Value = serde_json::from_str(
            &std::fs::read_to_string(dir.join("jupiter-quotes.json")).unwrap(),
        )
        .unwrap();
        for quote in recorded["quotes"].as_array().unwrap() {
            let atoms = |name: &str| quote[name].as_str().unwrap().parse::<u64>().unwrap();
            let mint = |name: &str| quote[name].as_str().unwrap().parse::<Pubkey>().unwrap();
            let route = search(
                &pools,
                &Request {
                    input: mint("inputMint"),
                    output: mint("outputMint"),
                    amount: atoms("inAmount"),
                    slippage_bps: 0,
                    max_hops: MAX_HOPS,
                },
            )
            .unwrap();
            let theirs = atoms("outAmount") as f64;
            let short_bps = (theirs - route.amount_out as f64) / theirs * 1e4;
            let tolerance = quote["tolerance_bps"].as_f64().unwrap();
            println!(
                "{} -> {}: Jupiter {theirs}, ours {} ({short_bps:+.2} bps short, tolerance {tolerance}): {}",
                quote["inAmount"],
                quote["outputMint"],
                route.amount_out,
                note(&route, &pools)
            );
            assert!(short_bps <= tolerance, "{short_bps} bps short of Jupiter");
        }
    }
}
