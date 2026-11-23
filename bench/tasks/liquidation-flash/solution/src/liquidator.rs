//! What the liquidation-flash reference does every step, and what its
//! informed twin shares with it: read SOL's price upstream of Kamino's
//! oracle, value every obligation there, and liquidate each one that is
//! under water in one atomic group (RFD 0005, task 1):
//!
//! 1. a push of SOL's Scope entries: when Scope still reads the obligation
//!    healthy, the liquidation is then allowed in the same transaction, and
//!    either way the program seizes at the price the group pushed;
//! 2. a flash borrow of the USDC it repays;
//! 3. the liquidation, at the market's close factor of the debt;
//! 4. a sale of the SOL it seizes on whichever pool pays more for it;
//! 5. the flash loan's repay, which the host adds.
//!
//! The group goes as a Jito bundle with a tip, so a race it loses fails
//! unpaid. How much it tips, and which races it enters, is a [`Bidding`]:
//! the reference tips a fixed share of the profit it expects; the informed
//! bot, told how the session's liquidators bid, tips what beats them.
//!
//! Health is the research's detector (`docs/RESEARCH.md`, "A detector"):
//! deposits at their liquidation threshold (the e-mode group's when the
//! obligation is in one) against debt at its borrow factor (1 in e-mode),
//! SOL at the upstream price.

use mtx_kit::{AmountUnit, Pubkey, log, prelude::*, rpc, rpc_accounts, serde_json};
use serde_json::{Value, json};

/// Kamino Lend.
pub const KLEND: &str = "KLend2g3cP87fffoy8q1mQqGKjrxjC8boSyAYavgmjD";
const WRAPPED_SOL: &str = "So11111111111111111111111111111111111111112";
/// Kamino's fixed-point scale, 2^60.
const SF: f64 = (1u64 << 60) as f64;
/// `OracleMappings::generic`, 20 bytes per entry, after a discriminator,
/// 512 keys, types, two-byte tolerances, TWAP masks and two-byte reference
/// prices (Scope `a09283d`): a pushed entry's `FixedPrice` upstream.
const MAPPINGS_GENERIC_AT: usize = 8 + 512 * 32 + 512 + 512 * 2 + 512 + 512 * 2;
/// An obligation's account size, and where its parts are (klend's layout).
const OBLIGATION_LEN: u64 = 3344;
const OBLIGATION_MARKET_AT: usize = 32;
const OBLIGATION_OWNER_AT: usize = 64;
const DEPOSITS_AT: usize = 96;
const DEPOSIT_LEN: usize = 136;
const DEPOSITS_MAX: usize = 8;
const BORROWS_AT: usize = 1208;
const BORROW_LEN: usize = 200;
const BORROWS_MAX: usize = 5;
const ELEVATION_GROUP_AT: usize = 2285;
/// A lending market's close factor, its least debt closed in full and its
/// most liquidated at once (whole USD), and its 32 elevation groups of 72
/// bytes (NOTES.md §4, `fetch_markets.py`).
const CLOSE_FACTOR_AT: usize = 126;
const MIN_FULL_AT: usize = 128;
const MAX_AT_ONCE_AT: usize = 136;
const ELEVATION_GROUPS_AT: usize = 200;
const ELEVATION_GROUP_LEN: usize = 72;
const ELEVATION_GROUPS_MAX: usize = 32;
/// The most obligations it values in a step: a session has at most 32
/// borrowers and the bot's own.
const OBLIGATIONS_MAX: usize = 64;
/// Under this health an obligation is liquidatable: a margin of a
/// millionth over the program's `>=`, for rounding (the research's false
/// flags were all at 1.0000).
const LIQUIDATABLE_BELOW: f64 = 1.0 - 1e-6;
/// What it repays of the close factor's share, for rounding: the program
/// refuses a repay over it.
const REPAY_MARGIN: f64 = 0.999;
/// What it sells of the SOL it expects to seize. The group pushes the
/// upstream price itself, so the program seizes at a price it knows; a
/// tenth of a percent is left for rounding (the reserve's collateral
/// exchange). The rest is swept later: mandate v0.3.0 judges what the
/// wallet holds beside USDC after every transaction, and the fee SOL is
/// already about $70 of the $100 it allows, so no more may stay behind.
const SELL_MARGIN: f64 = 0.999;
/// The least worse than its estimate it accepts a sale at: past this the
/// swap, and the group with it, fails.
const SALE_SLIPPAGE: f64 = 0.003;
/// Kamino's flash loan fee on USDC: 0.001% (NOTES.md §3).
const FLASH_FEE: f64 = 1e-5;
/// The share of the bonus the protocol keeps where the reserve says
/// nothing: half, on the main market (NOTES.md §1).
const PROTOCOL_SHARE_DEFAULT: f64 = 0.5;
/// It skips a liquidation expected to make less than this, in USD: a tip,
/// a push and the fee cost about a cent.
const PROFIT_MIN_USD: f64 = 0.02;
/// Steps it leaves an obligation alone after sending a group for it: a
/// bundle lands within a step or two, or loses.
const RETRY_STEPS: u64 = 2;
/// It sells wrapped SOL left over from earlier sales above this: the
/// mandate allows $100 of anything but USDC, and the wallet's own SOL for
/// fees is already about $70 of it.
const SWEEP_MIN_SOL: f64 = 0.05;
/// Most characters of a send's state it logs: a refused bundle's carries
/// the whole transaction's logs.
const STATE_CHARS_MAX: usize = 120;
/// Jito's least tip.
pub const MIN_TIP_LAMPORTS: u64 = 1_000;
/// Lamports in a SOL.
const LAMPORTS: f64 = 1e9;

/// A reserve as the session's venue metadata names it.
#[derive(Debug, Clone, Default)]
struct ReserveKeys {
    id: String,
    address: String,
    market: String,
    mint: String,
    scope_mappings: Option<String>,
    scope_entry: Option<u16>,
}

/// A pool the session lets it sell through.
#[derive(Debug, Clone, Default)]
struct PoolKeys {
    id: String,
    address: String,
}

/// A reserve's terms: its price per whole token as of its last refresh,
/// its collateral's exchange rate, liquidation threshold, borrow factor,
/// least bonus, the protocol's share of a bonus, and its decimals.
#[derive(Debug, Clone, Copy)]
pub struct Terms {
    pub price: f64,
    rate: f64,
    threshold: f64,
    borrow_factor: f64,
    pub bonus: f64,
    pub protocol_share: f64,
    decimals: i32,
}

/// The lending market's liquidation rules.
#[derive(Debug, Clone, Copy, Default)]
struct MarketRules {
    close_factor: f64,
    min_full_usd: f64,
    max_at_once_usd: f64,
    /// Each elevation group's liquidation threshold, at its id less one
    /// (id 0 is no group).
    groups: [Option<f64>; ELEVATION_GROUPS_MAX],
}

/// A liquidation it could make now, as it values it.
#[derive(Debug, Clone)]
pub struct Opportunity {
    pub obligation: String,
    /// Scope still reads the obligation healthy: the group pushes first.
    pub stale: bool,
    /// USDC it repays, and SOL it expects to seize and sells.
    pub repay: f64,
    pub sell_sol: f64,
    /// The pool it sells on, and the least average price it accepts.
    pub pool: String,
    pub limit: f64,
    /// The reserve's bonus on the repay and what of it it keeps, in USD,
    /// and what it expects to make after the sale's fee and impact and the
    /// flash fee.
    pub bonus_usd: f64,
    pub kept_usd: f64,
    pub profit_usd: f64,
    /// SOL's upstream price.
    pub sol_usd: f64,
}

/// How a liquidator bids: its tip for a liquidation, in lamports, or
/// `None` to stay out of the race.
pub trait Bidding {
    fn tip(&self, found: &Opportunity) -> Option<u64>;
}

/// Which SOL price it values obligations at.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PriceSource {
    /// The upstream, which leads Scope: what a liquidator that wins reads.
    #[default]
    Upstream,
    /// Scope's own, as the reserve last read it: a push too late. The
    /// ladder's `scope-reader` rung.
    Scope,
}

/// The liquidator's state across steps.
#[derive(Debug, Default)]
pub struct Liquidator {
    source: PriceSource,
    reserves: Vec<ReserveKeys>,
    pools: Vec<PoolKeys>,
    authority: String,
    rules: MarketRules,
    /// Obligations it sent for, and the step it did.
    tried: Vec<(String, u64)>,
    /// The last atomic group's id.
    group: u32,
}

impl Liquidator {
    /// A liquidator that values obligations at `source`'s price.
    pub fn reading(source: PriceSource) -> Self {
        Self {
            source,
            ..Self::default()
        }
    }

    /// Read the session's reserves, pools and market from its venue
    /// metadata and the chain.
    pub fn start(&mut self, session: &Session) -> BotResult {
        let venue: Value = serde_json::from_str(&session.venue).map_err(|e| e.to_string())?;
        for r in venue["reserves"].as_array().into_iter().flatten() {
            let text = |name: &str| r[name].as_str().unwrap_or_default().to_string();
            self.reserves.push(ReserveKeys {
                id: text("id"),
                address: text("reserve"),
                market: text("market"),
                mint: text("mint"),
                scope_mappings: r["scope_mappings"].as_str().map(str::to_string),
                scope_entry: r["scope_entries"][0]
                    .as_u64()
                    .and_then(|e| u16::try_from(e).ok()),
            });
        }
        for p in venue["pools"].as_array().into_iter().flatten() {
            self.pools.push(PoolKeys {
                id: p["id"].as_str().unwrap_or_default().to_string(),
                address: p["address"].as_str().unwrap_or_default().to_string(),
            });
        }
        if self.reserves.len() != 2 || self.pools.is_empty() {
            return Err(format!(
                "a session of two reserves and at least one pool, not {} and {}",
                self.reserves.len(),
                self.pools.len()
            )
            .into());
        }
        self.authority = session.authority.clone();
        let market = self.reserves[0].market.clone();
        let data = account(&market).ok_or("the lending market isn't readable")?;
        self.rules = market_rules(&data);
        log(&format!(
            "reserves {}, pools {}, close factor {}, least full ${}, most at once ${}",
            self.reserves
                .iter()
                .map(|r| r.id.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            self.pools
                .iter()
                .map(|p| p.id.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            self.rules.close_factor,
            self.rules.min_full_usd,
            self.rules.max_at_once_usd,
        ));
        Ok(())
    }

    /// One step: what became of last step's groups, every liquidation it
    /// can make, and a sweep of leftover SOL.
    pub fn step(
        &mut self,
        market: &Market,
        orders: &mut Orders,
        bidding: &dyn Bidding,
    ) -> BotResult {
        for send in market.sends.iter().filter(|s| s.step + 1 == market.k) {
            if let Some(mtx_kit::Action::Liquidate {
                obligation, repay, ..
            }) = &send.order
            {
                let state = format!("{:?}", send.state);
                log(&format!(
                    "step {}: {obligation} repaying {repay:.2}: {}, tip {:?}",
                    send.step,
                    state.chars().take(STATE_CHARS_MAX).collect::<String>(),
                    send.tip
                ));
            }
        }
        let Some(sol) = self
            .reserves
            .iter()
            .find(|r| r.mint == WRAPPED_SOL)
            .cloned()
        else {
            return Ok(());
        };
        let Some(usdc) = self
            .reserves
            .iter()
            .find(|r| r.mint != WRAPPED_SOL)
            .cloned()
        else {
            return Ok(());
        };
        let Some(upstream) = upstream(&sol) else {
            return Ok(());
        };
        let found = self.opportunities(market.k, &sol, &usdc, upstream);
        for opportunity in found {
            let Some(tip) = bidding.tip(&opportunity) else {
                log(&format!(
                    "step {}: staying out of {} (profit ${:.2})",
                    market.k, opportunity.obligation, opportunity.profit_usd
                ));
                continue;
            };
            self.send(market.k, &opportunity, tip, &sol.id, &usdc.id, orders);
        }
        self.sweep(upstream, orders);
        Ok(())
    }

    /// One liquidation's atomic group.
    fn send(
        &mut self,
        k: u64,
        found: &Opportunity,
        tip: u64,
        sol_id: &str,
        usdc_id: &str,
        orders: &mut Orders,
    ) {
        assert!(found.repay > 0.0 && found.sell_sol > 0.0);
        assert!(tip >= MIN_TIP_LAMPORTS);
        log(&format!(
            "step {k}: liquidating {} at SOL {:.4} ({}): repay {:.2} USDC, sell {:.4} SOL on {} at >= {:.2}, keep ${:.3}, expect ${:.3}, tip {tip}",
            found.obligation,
            found.sol_usd,
            if found.stale {
                "Scope stale"
            } else {
                "Scope fresh"
            },
            found.repay,
            found.sell_sol,
            found.pool,
            found.limit,
            found.kept_usd,
            found.profit_usd,
        ));
        self.group += 1;
        let id = self.group;
        let options = TxOptions {
            tip: Some(tip),
            atomic: Some(id),
            ..TxOptions::default()
        };
        // Pushed whether Scope reads the obligation healthy or not: the
        // seizure is then at the upstream it pushes, not at a price
        // another push may move before the group lands. A bot that reads
        // Scope alone knows no upstream to push.
        match self.source {
            PriceSource::Upstream => {
                orders.push_price().at(sol_id).tx(options);
                orders.flash(found.repay).at(usdc_id).atomic(id);
            }
            PriceSource::Scope => {
                orders.flash(found.repay).at(usdc_id).tx(options);
            }
        }
        orders
            .liquidate(&found.obligation, sol_id, found.repay)
            .at(usdc_id)
            .atomic(id);
        orders
            .swap(Side::Ask, found.sell_sol, AmountUnit::Base, found.limit)
            .at(&found.pool)
            .atomic(id);
        self.tried.push((found.obligation.clone(), k));
    }

    /// Sell the wrapped SOL earlier sales left, once it is worth selling.
    fn sweep(&self, upstream: f64, orders: &mut Orders) {
        let Some(held) = wrapped_sol(&self.authority) else {
            return;
        };
        if held < SWEEP_MIN_SOL {
            return;
        }
        let Some((pool, proceeds)) = self.best_sale(held) else {
            return;
        };
        let limit = cents_down(proceeds / held * (1.0 - SALE_SLIPPAGE));
        log(&format!(
            "sweeping {held:.4} SOL on {pool} at >= {limit:.2} (upstream {upstream:.4})"
        ));
        orders
            .swap(Side::Ask, held, AmountUnit::Base, limit)
            .at(&pool);
    }

    /// Every liquidation worth making now, the largest first.
    fn opportunities(
        &mut self,
        k: u64,
        sol: &ReserveKeys,
        usdc: &ReserveKeys,
        upstream: f64,
    ) -> Vec<Opportunity> {
        self.tried.retain(|(_, at)| k < at + RETRY_STEPS);
        let addresses = [sol.address.as_str(), usdc.address.as_str()];
        let Ok(reserves) = rpc_accounts(&addresses) else {
            return Vec::new();
        };
        let (Some(Some(sol_data)), Some(Some(usdc_data))) = (reserves.first(), reserves.get(1))
        else {
            return Vec::new();
        };
        let (sol_terms, usdc_terms) = (terms(sol_data), terms(usdc_data));
        if !(sol_terms.price > 0.0 && usdc_terms.price > 0.0) {
            return Vec::new();
        }
        let upstream = match self.source {
            PriceSource::Upstream => upstream,
            PriceSource::Scope => sol_terms.price,
        };
        // SOL upstream over SOL as the reserve last priced it.
        let ratio = upstream / sol_terms.price;
        let mut found: Vec<Opportunity> = Vec::new();
        for (address, data) in obligations(&sol.market) {
            if key(&data, OBLIGATION_OWNER_AT) == self.authority
                || self.tried.iter().any(|(a, _)| *a == address)
            {
                continue;
            }
            let of = |reserve: &str| {
                if reserve == sol.address {
                    Some(sol_terms)
                } else if reserve == usdc.address {
                    Some(usdc_terms)
                } else {
                    None
                }
            };
            let group = match usize::from(data[ELEVATION_GROUP_AT]) {
                0 => None,
                id => self.rules.groups.get(id - 1).copied().flatten(),
            };
            let Some(value) = value(&data, &of, &sol.address, ratio, group) else {
                continue;
            };
            if value.health_upstream >= LIQUIDATABLE_BELOW || value.owed_usdc <= 0.0 {
                continue;
            }
            if let Some(o) = self.opportunity(address, &value, &sol_terms, upstream) {
                found.push(o);
            }
        }
        found.sort_by(|a, b| b.repay.total_cmp(&a.repay));
        found
    }

    /// What liquidating one obligation would repay, seize and make.
    fn opportunity(
        &self,
        obligation: String,
        value: &Valued,
        sol_terms: &Terms,
        upstream: f64,
    ) -> Option<Opportunity> {
        let debt_usd = value.owed_usdc;
        let mut repay = if debt_usd < self.rules.min_full_usd {
            value.owed_usdc
        } else {
            value.owed_usdc * self.rules.close_factor * REPAY_MARGIN
        };
        if self.rules.max_at_once_usd > 0.0 {
            repay = repay.min(self.rules.max_at_once_usd * REPAY_MARGIN);
        }
        // USDC is a dollar, to a basis point: what the bot repays is what
        // the bonus is a share of.
        let kept_share = sol_terms.bonus * (1.0 - sol_terms.protocol_share);
        // The program seizes at Scope's price, which the group pushes to
        // the upstream first (a Scope reader's `upstream` is Scope's own).
        let stale = value.health_scope >= LIQUIDATABLE_BELOW;
        let seized_sol = repay * (1.0 + kept_share) / upstream;
        let sell_sol = seized_sol * SELL_MARGIN;
        let (pool, proceeds) = self.best_sale(sell_sol)?;
        // What the sale costs in fee and impact against the upstream, and
        // the flash fee: the SOL it keeps back is swept later at about the
        // same price.
        let sale_cost = (sell_sol * upstream - proceeds).max(0.0);
        let kept_usd = repay * kept_share;
        let profit_usd = kept_usd - sale_cost - repay * FLASH_FEE;
        if profit_usd < PROFIT_MIN_USD {
            return None;
        }
        Some(Opportunity {
            obligation,
            stale,
            repay,
            sell_sol,
            pool,
            limit: cents_down(proceeds / sell_sol * (1.0 - SALE_SLIPPAGE)),
            bonus_usd: repay * sol_terms.bonus,
            kept_usd,
            profit_usd,
            sol_usd: upstream,
        })
    }

    /// The pool that pays the most USDC for `sol`, and what it pays.
    fn best_sale(&self, sol: f64) -> Option<(String, f64)> {
        let addresses: Vec<&str> = self.pools.iter().map(|p| p.address.as_str()).collect();
        let found = rpc_accounts(&addresses).ok()?;
        self.pools
            .iter()
            .zip(found)
            .filter_map(|(pool, data)| Some((pool.id.clone(), sale(&data?, sol)?)))
            .max_by(|a, b| a.1.total_cmp(&b.1))
    }
}

/// An obligation valued at Scope's price and upstream.
struct Valued {
    health_scope: f64,
    health_upstream: f64,
    /// USDC it owes.
    owed_usdc: f64,
}

/// An obligation's health at Scope's price and with SOL at `ratio` times
/// it, and the USDC it owes; `None` if it holds a reserve the session
/// doesn't, or owes nothing.
fn value(
    data: &[u8],
    of: &dyn Fn(&str) -> Option<Terms>,
    sol: &str,
    ratio: f64,
    group_threshold: Option<f64>,
) -> Option<Valued> {
    let (mut limit_scope, mut limit_upstream, mut debt_scope, mut debt_upstream) =
        (0.0, 0.0, 0.0, 0.0);
    let mut owed_usdc = 0.0;
    for i in 0..DEPOSITS_MAX {
        let at = DEPOSITS_AT + i * DEPOSIT_LEN;
        let reserve = key(data, at);
        if reserve == Pubkey::default().to_string() {
            continue;
        }
        let t = of(&reserve)?;
        let value = u64_at(data, at + 32) * t.rate * t.price / 10f64.powi(t.decimals);
        let threshold = group_threshold.unwrap_or(t.threshold);
        let moved = if reserve == sol { ratio } else { 1.0 };
        limit_scope += value * threshold;
        limit_upstream += value * moved * threshold;
    }
    for i in 0..BORROWS_MAX {
        let at = BORROWS_AT + i * BORROW_LEN;
        let reserve = key(data, at);
        if reserve == Pubkey::default().to_string() {
            continue;
        }
        let t = of(&reserve)?;
        let units = sf_at(data, at + 88) / 10f64.powi(t.decimals);
        // In an elevation group, debt counts at its value.
        let factor = if group_threshold.is_some() {
            1.0
        } else {
            t.borrow_factor
        };
        let moved = if reserve == sol { ratio } else { 1.0 };
        debt_scope += units * t.price * factor;
        debt_upstream += units * t.price * moved * factor;
        if reserve != sol {
            owed_usdc += units;
        }
    }
    if debt_scope <= 0.0 {
        return None;
    }
    Some(Valued {
        health_scope: limit_scope / debt_scope,
        health_upstream: limit_upstream / debt_upstream,
        owed_usdc,
    })
}

/// USDC a pool pays for `sol`, after its fee, within its current tick
/// range: exact while the sale crosses no initialized tick, which a sale
/// of a few hundred SOL on the session's pools doesn't (500 SOL moved
/// Whirlpool 3 bp and Raydium 15 bp). `None` for an account that is
/// neither pool.
fn sale(data: &[u8], sol: f64) -> Option<f64> {
    assert!(sol >= 0.0);
    let pool = Pool::decode(data)?;
    let fee = match pool.kind {
        PoolKind::Whirlpool { fee_rate } => fee_rate,
        PoolKind::Raydium { config } => raydium_fee(&config)?,
    };
    let sqrt = pool.sqrt_price_x64 as f64 / 2f64.powi(64);
    let liquidity = pool.liquidity as f64;
    if !(sqrt > 0.0 && liquidity > 0.0) {
        return None;
    }
    let atoms_in = sol * LAMPORTS * (1.0 - fee);
    // USDC atoms out; the price's square root is in token 1 atoms per token
    // 0 atom.
    let usdc_atoms = if pool.sol_is_token_0 {
        let after = 1.0 / (1.0 / sqrt + atoms_in / liquidity);
        liquidity * (sqrt - after)
    } else {
        let after = sqrt + atoms_in / liquidity;
        liquidity * (1.0 / sqrt - 1.0 / after)
    };
    Some(usdc_atoms / 1e6)
}

enum PoolKind {
    /// Its fee rate, a share.
    Whirlpool { fee_rate: f64 },
    /// Its fee is its config's.
    Raydium { config: String },
}

struct Pool {
    kind: PoolKind,
    liquidity: u128,
    sqrt_price_x64: u128,
    sol_is_token_0: bool,
}

/// Account sizes of a Whirlpool and of a Raydium CLMM pool.
const WHIRLPOOL_LEN: usize = 653;
const RAYDIUM_POOL_LEN: usize = 1544;

impl Pool {
    fn decode(data: &[u8]) -> Option<Self> {
        match data.len() {
            WHIRLPOOL_LEN => Some(Self {
                kind: PoolKind::Whirlpool {
                    fee_rate: f64::from(u16::from_le_bytes([data[45], data[46]])) / 1e6,
                },
                liquidity: u128_at(data, 49),
                sqrt_price_x64: u128_at(data, 65),
                sol_is_token_0: key(data, 101) == WRAPPED_SOL,
            }),
            RAYDIUM_POOL_LEN => Some(Self {
                kind: PoolKind::Raydium {
                    config: key(data, 9),
                },
                liquidity: u128_at(data, 237),
                sqrt_price_x64: u128_at(data, 253),
                sol_is_token_0: key(data, 73) == WRAPPED_SOL,
            }),
            _ => None,
        }
    }
}

/// A Raydium CLMM config's trade fee, a share.
fn raydium_fee(config: &str) -> Option<f64> {
    let data = account(config)?;
    let rate = u32::from_le_bytes(data.get(47..51)?.try_into().ok()?);
    Some(f64::from(rate) / 1e6)
}

/// A reserve's [`Terms`] from its account.
fn terms(data: &[u8]) -> Terms {
    let decimals = u64_at(data, 272) as i32;
    let supply = u64_at(data, 224) + sf_at(data, 232)
        - sf_at(data, 344)
        - sf_at(data, 360)
        - sf_at(data, 376);
    let collateral = u64_at(data, 2592);
    let protocol_pct = data[4871];
    Terms {
        price: sf_at(data, 248),
        rate: if collateral > 0.0 {
            supply / collateral
        } else {
            1.0
        },
        threshold: f64::from(data[4873]) / 100.0,
        borrow_factor: u64_at(data, 5008) / 100.0,
        bonus: f64::from(u16::from_le_bytes([data[4874], data[4875]])) / 10_000.0,
        protocol_share: if protocol_pct <= 100 {
            f64::from(protocol_pct) / 100.0
        } else {
            PROTOCOL_SHARE_DEFAULT
        },
        decimals,
    }
}

/// The market's close factor, its liquidation limits and its elevation
/// groups' thresholds.
fn market_rules(data: &[u8]) -> MarketRules {
    let mut groups = [None; ELEVATION_GROUPS_MAX];
    for i in 0..ELEVATION_GROUPS_MAX {
        let at = ELEVATION_GROUPS_AT + ELEVATION_GROUP_LEN * i;
        let (id, threshold) = (usize::from(data[at + 2]), data[at + 4]);
        if id > 0 && id <= ELEVATION_GROUPS_MAX && threshold > 0 {
            groups[id - 1] = Some(f64::from(threshold) / 100.0);
        }
    }
    MarketRules {
        close_factor: f64::from(data[CLOSE_FACTOR_AT]) / 100.0,
        min_full_usd: u64_at(data, MIN_FULL_AT),
        max_at_once_usd: u64_at(data, MAX_AT_ONCE_AT),
        groups,
    }
}

/// SOL's upstream price: its pushed Scope entry's `FixedPrice`, in its
/// mappings.
fn upstream(sol: &ReserveKeys) -> Option<f64> {
    let data = account(sol.scope_mappings.as_deref()?)?;
    let at = MAPPINGS_GENERIC_AT + 20 * usize::from(sol.scope_entry?);
    let value = u64_at(&data, at);
    let exponent = u64_at(&data, at + 8) as i32;
    let price = value / 10f64.powi(exponent);
    (price.is_finite() && price > 0.0).then_some(price)
}

/// Every obligation of `market`, with its data, at most
/// [`OBLIGATIONS_MAX`].
fn obligations(market: &str) -> Vec<(String, Vec<u8>)> {
    let params = json!([KLEND, {
        "encoding": "base64",
        "filters": [
            { "dataSize": OBLIGATION_LEN },
            { "memcmp": { "offset": OBLIGATION_MARKET_AT, "bytes": market } },
        ],
    }]);
    let Ok(text) = rpc("getProgramAccounts", &params.to_string()) else {
        return Vec::new();
    };
    let Ok(found) = serde_json::from_str::<Value>(&text) else {
        return Vec::new();
    };
    found
        .as_array()
        .into_iter()
        .flatten()
        .take(OBLIGATIONS_MAX)
        .filter_map(|o| {
            let address = o["pubkey"].as_str()?.to_string();
            let data = decode_base64(o["account"]["data"][0].as_str()?)?;
            (data.len() == OBLIGATION_LEN as usize).then_some((address, data))
        })
        .collect()
}

/// The wrapped SOL the bot's token accounts hold, in SOL.
fn wrapped_sol(owner: &str) -> Option<f64> {
    let params = json!([owner, { "mint": WRAPPED_SOL }, { "encoding": "jsonParsed" }]);
    let text = rpc("getTokenAccountsByOwner", &params.to_string()).ok()?;
    let found: Value = serde_json::from_str(&text).ok()?;
    let held = found["value"]
        .as_array()?
        .iter()
        .filter_map(|a| a["account"]["data"]["parsed"]["info"]["tokenAmount"]["uiAmount"].as_f64())
        .sum();
    Some(held)
}

fn account(address: &str) -> Option<Vec<u8>> {
    mtx_kit::rpc_account(address).ok().flatten()
}

/// A price rounded down to the cent, as a sale's least.
fn cents_down(price: f64) -> f64 {
    (price * 100.0).floor() / 100.0
}

fn u64_at(data: &[u8], at: usize) -> f64 {
    u64::from_le_bytes(data[at..at + 8].try_into().expect("8 bytes")) as f64
}

fn u128_at(data: &[u8], at: usize) -> u128 {
    u128::from_le_bytes(data[at..at + 16].try_into().expect("16 bytes"))
}

fn sf_at(data: &[u8], at: usize) -> f64 {
    u128_at(data, at) as f64 / SF
}

fn key(data: &[u8], at: usize) -> String {
    let bytes: [u8; 32] = data[at..at + 32].try_into().expect("32 bytes");
    Pubkey::new_from_array(bytes).to_string()
}

/// Standard base64, as `getProgramAccounts` answers: four characters to
/// three bytes, `=` padding.
fn decode_base64(text: &str) -> Option<Vec<u8>> {
    fn sextet(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some(u32::from(c - b'A')),
            b'a'..=b'z' => Some(u32::from(c - b'a') + 26),
            b'0'..=b'9' => Some(u32::from(c - b'0') + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let bytes = text.trim_end_matches('=').as_bytes();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    for chunk in bytes.chunks(4) {
        let mut word = 0u32;
        for (i, c) in chunk.iter().enumerate() {
            word |= sextet(*c)? << (18 - 6 * i);
        }
        let take = match chunk.len() {
            4 => 3,
            3 => 2,
            2 => 1,
            _ => return None,
        };
        out.extend_from_slice(&word.to_be_bytes()[1..=take]);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_reads_padding_and_refuses_junk() {
        assert_eq!(decode_base64("TWFu").unwrap(), b"Man");
        assert_eq!(decode_base64("TWE=").unwrap(), b"Ma");
        assert_eq!(decode_base64("TQ==").unwrap(), b"M");
        assert_eq!(decode_base64("").unwrap(), b"");
        assert!(decode_base64("T").is_none());
        assert!(decode_base64("T*==").is_none());
    }

    #[test]
    fn a_sale_pays_less_than_the_mid_and_more_for_more() {
        // A pool at $70 a SOL (token 1 USDC per token 0 lamport: 0.07
        // atoms) with liquidity deep enough for a few basis points.
        let mut data = vec![0u8; WHIRLPOOL_LEN];
        data[45..47].copy_from_slice(&400u16.to_le_bytes());
        let sqrt = (0.07f64).sqrt() * 2f64.powi(64);
        data[65..81].copy_from_slice(&(sqrt as u128).to_le_bytes());
        data[49..65].copy_from_slice(&(1e13 as u128).to_le_bytes());
        let wsol: Pubkey = WRAPPED_SOL.parse().unwrap();
        data[101..133].copy_from_slice(wsol.as_ref());
        let one = sale(&data, 1.0).unwrap();
        let hundred = sale(&data, 100.0).unwrap();
        assert!(one < 70.0 * (1.0 - 0.0004) + 1e-9 && one > 69.9);
        assert!(hundred / 100.0 < one, "impact");
        assert!(hundred > 99.0 * one);
        assert!(sale(&data[1..], 1.0).is_none());
    }
}
