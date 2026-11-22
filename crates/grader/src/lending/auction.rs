//! A lending session's competition under the transaction layer
//! (ADR 0004, RFD 0005 task 1): Scope's keeper and the competing
//! liquidators send through the layer's auction, under their own names,
//! as the bot does.
//!
//! - **The keeper** pushes every pushed Scope entry ([`ScopeKeeper`]) on
//!   mainnet's cadence, with a low priority fee: Scope's own price lags the
//!   upstream by up to that many slots, unless someone pushes first.
//! - **A liquidator** reads the upstream (the pushed entries' `FixedPrice`
//!   in Scope's mappings, as a bot can), and computes each obligation's
//!   health there with the research's detector: debt adjusted by its borrow
//!   factor against the deposits' value times their liquidation threshold,
//!   e-mode included through the program's own values ([`upstream_health`]).
//!   When one is liquidatable, it may join the race for it ([`competes`]),
//!   after a latency in slots ([`latency_slots`]), and sends one
//!   transaction: a push of the collateral's price if Scope still reads it
//!   healthy, then the liquidation, paid from its own inventory, with a bid
//!   that is a share of the bonus it expects ([`Bid`]).
//!
//! Every number is the research's (`docs/RESEARCH.md`, "Kamino Lend
//! liquidations", and `ops/research/atomic-arb/kamino-liquidations/NOTES.md`
//! §3 and §6), and every draw comes from the scenario's lending seed
//! through [`crate::txlayer::draw`], keyed by the liquidator, the borrower
//! and the step it first saw the obligation, so a session replays its
//! competition exactly.

use std::{collections::BTreeMap, sync::Arc};

use mtx_kit::chain::{ChainClient, InstructionGroup, TransactionPlan};
use serde::{Deserialize, Serialize};
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use solana_transaction::{Instruction, versioned::VersionedTransaction};
use tracing::warn;

use super::{ActorAction, Attempt, KaminoActors, atoms};
use crate::{
    session::SLOTS_PER_STEP,
    txlayer::{
        RouteKind, TxEngine, draw,
        engine::{Fate, Submission},
        jito, pool,
    },
    venue::kamino::{self, Reading, klend},
};

/// The keeper's push cadence by default, in slots: Scope updated every 5
/// slots (p50) on the main market while active (NOTES.md §6).
const KEEPER_EVERY_SLOTS_DEFAULT: u64 = 5;
/// The keeper's priority fee by default, in micro-lamports per compute
/// unit: a crank's, far under a liquidator's bid (NOTES.md §3, priority fee
/// p50 5.2k lamports a liquidation, against a keeper paying for nothing but
/// inclusion).
const KEEPER_CU_PRICE_DEFAULT: u64 = 100;
/// The slowest cadence a scenario may set: a blockhash's life.
const KEEPER_EVERY_SLOTS_MAX: u64 = 150;
/// The highest price a scenario may set for the keeper: a spike's p99.
const KEEPER_CU_PRICE_MAX: u64 = 10_000_000;

/// Who pushes Scope's pushed entries on a schedule.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ScopeKeeper {
    /// Slots between its pushes.
    #[serde(default = "default_every_slots")]
    pub every_slots: u64,
    /// Its priority fee, in micro-lamports per compute unit.
    #[serde(default = "default_cu_price")]
    pub cu_price: u64,
}

fn default_every_slots() -> u64 {
    KEEPER_EVERY_SLOTS_DEFAULT
}

fn default_cu_price() -> u64 {
    KEEPER_CU_PRICE_DEFAULT
}

impl Default for ScopeKeeper {
    fn default() -> Self {
        Self {
            every_slots: KEEPER_EVERY_SLOTS_DEFAULT,
            cu_price: KEEPER_CU_PRICE_DEFAULT,
        }
    }
}

impl ScopeKeeper {
    pub fn validate(&self) -> Result<(), String> {
        if !(1..=KEEPER_EVERY_SLOTS_MAX).contains(&self.every_slots) {
            return Err(format!(
                "lending: keeper.every_slots is 1 to {KEEPER_EVERY_SLOTS_MAX}"
            ));
        }
        if self.cu_price > KEEPER_CU_PRICE_MAX {
            return Err(format!(
                "lending: keeper.cu_price is at most {KEEPER_CU_PRICE_MAX}"
            ));
        }
        Ok(())
    }
}

/// The share of the liquidation bonus a liquidator keeps: Kamino's main
/// market pays 100 bp and keeps half (RESEARCH.md, "What gets
/// liquidated").
pub const BONUS_KEPT_SHARE: f64 = 0.5;

/// The latest a liquidator acts, in slots after it first sees an
/// obligation it can liquidate: twice the research's p90 of 21, which
/// bounds a tail it didn't measure.
pub const LATENCY_SLOTS_MAX: u64 = 42;

/// Where the repaid size changes the competition: under $10, from $10 to
/// $1,000, and above (NOTES.md §3).
const SMALL_USD: f64 = 10.0;
const LARGE_USD: f64 = 1_000.0;

/// How a session's liquidators race under the layer: the research's
/// distributions by default, or a session's own, which a scenario family
/// draws per session (`liquidation-flash`). Every rule is the process the
/// competition is drawn from, never a draw itself: which liquidator joins
/// which race, and how late, stays the auction's draw.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Race {
    /// Each liquidator's bid, a share of the bonus it expects to keep, is
    /// drawn uniformly from `[low, high]`. Absent, 0.25 to 0.45.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bid_share: Option<[f64; 2]>,
    /// The chance each liquidator tips through a bundle rather than paying a
    /// priority fee. Absent, 0.46.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tipping_share: Option<f64>,
    /// The share of races (repaying $1,000 or less) a liquidator joins in
    /// the slot it saw the obligation turn; the rest of the lag keeps the
    /// research's shape. Absent, 0.5.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub same_slot_share: Option<f64>,
    /// The research's mean count of competitors for a size, times this.
    /// Absent, 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub competition: Option<f64>,
}

/// The widest a session may scale the research's competition: 4.4
/// competitors above $1,000 times this is still at most a session's five
/// liquidators, all of them in.
const COMPETITION_MAX: f64 = 4.0;
/// A same-slot share must leave room on both sides of the lag's shape.
const SAME_SLOT_SHARE_MIN: f64 = 0.05;
const SAME_SLOT_SHARE_MAX: f64 = 0.95;

impl Race {
    pub fn validate(&self) -> Result<(), String> {
        if let Some([low, high]) = self.bid_share
            && !(low.is_finite() && high.is_finite() && 0.0 < low && low <= high && high <= 1.0)
        {
            return Err("lending: liquidators.race.bid_share is a range within (0, 1]".into());
        }
        if self
            .tipping_share
            .is_some_and(|t| !(t.is_finite() && (0.0..=1.0).contains(&t)))
        {
            return Err("lending: liquidators.race.tipping_share is within [0, 1]".into());
        }
        if self
            .same_slot_share
            .is_some_and(|s| !(SAME_SLOT_SHARE_MIN..=SAME_SLOT_SHARE_MAX).contains(&s))
        {
            return Err(format!(
                "lending: liquidators.race.same_slot_share is {SAME_SLOT_SHARE_MIN} to {SAME_SLOT_SHARE_MAX}"
            ));
        }
        if self
            .competition
            .is_some_and(|c| !(c.is_finite() && (0.0..=COMPETITION_MAX).contains(&c)))
        {
            return Err(format!(
                "lending: liquidators.race.competition is 0 to {COMPETITION_MAX}"
            ));
        }
        Ok(())
    }
}

/// The cumulative shares of the lag's breakpoints, at 0, 2 and 21 slots.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Lag {
    zero: f64,
    two: f64,
    ninety: f64,
}

impl Lag {
    /// The lag's shape for a race repaying `repay_usd`, at a session's
    /// same-slot share. The research's, `None`: overall, 0 slots in 50% of
    /// episodes, 2 or fewer in 71%, p90 21; above $1,000 repaid, 93% in
    /// the same slot (RESEARCH.md, "What tipped them over"). Above $1,000
    /// the research measured only the first; the rest of the tail keeps the
    /// overall one's shape, compressed into the 7% left. A session's own
    /// share `s` keeps that shape: 2 or fewer at `s + 0.42 (1 - s)`, 21 or
    /// fewer at `s + 0.8 (1 - s)`, and above $1,000 a same-slot share that
    /// leaves `0.07 (1 - s) / 0.5` to the tail.
    fn of(repay_usd: f64, same_slot_share: Option<f64>) -> Self {
        let large = repay_usd > LARGE_USD;
        let zero = match (same_slot_share, large) {
            // The research's literal breakpoints: the same floats as ever.
            (None, true) => {
                return Self {
                    zero: 0.93,
                    two: 0.93 + 0.07 * 0.42,
                    ninety: 0.93 + 0.07 * 0.8,
                };
            }
            (None, false) => {
                return Self {
                    zero: 0.50,
                    two: 0.71,
                    ninety: 0.90,
                };
            }
            (Some(s), true) => 1.0 - 0.07 * (1.0 - s) / 0.5,
            (Some(s), false) => s,
        };
        Self::compressed(zero)
    }

    fn compressed(zero: f64) -> Self {
        let lag = Self {
            zero,
            two: zero + (1.0 - zero) * 0.42,
            ninety: zero + (1.0 - zero) * 0.8,
        };
        assert!(0.0 < lag.zero && lag.zero < lag.two && lag.two < lag.ninety && lag.ninety < 1.0);
        lag
    }
}

/// How long a liquidator takes to act on an obligation it saw turn
/// liquidatable, in slots, for a uniform draw `u`, the size it would repay
/// and the session's same-slot share ([`Race::same_slot_share`], the
/// research's when `None`): the inverse of the distribution of the lag from
/// the update to the liquidation ([`Lag`]).
pub fn latency_slots(u: f64, repay_usd: f64, same_slot_share: Option<f64>) -> u64 {
    assert!((0.0..1.0).contains(&u), "a uniform draw: {u}");
    let Lag { zero, two, ninety } = Lag::of(repay_usd, same_slot_share);
    let between = |from: f64, to: f64, low: u64, high: u64| -> u64 {
        let share = (u - from) / (to - from);
        low + ((share * (high - low + 1) as f64) as u64).min(high - low)
    };
    let slots = if u < zero {
        0
    } else if u < two {
        between(zero, two, 1, 2)
    } else if u < ninety {
        between(two, ninety, 3, 21)
    } else {
        between(ninety, 1.0, 22, LATENCY_SLOTS_MAX)
    };
    assert!(slots <= LATENCY_SLOTS_MAX);
    slots
}

/// How many other liquidators go after an obligation, on average, for the
/// size repaid: 0.56 under $10, 2.6 from $10 to $1,000, 4.4 above (NOTES.md
/// §3, "Mean other signers by size").
pub fn competitors_mean(repay_usd: f64) -> f64 {
    if repay_usd < SMALL_USD {
        0.56
    } else if repay_usd <= LARGE_USD {
        2.6
    } else {
        4.4
    }
}

/// Whether one of `count` liquidators joins the race for an obligation, for
/// a uniform draw `u`: each joins with the chance that makes the expected
/// number of them the research's mean for the size, times the session's
/// `competition` (1 when `None`), all of them when that is more than the
/// session has.
pub fn competes(u: f64, repay_usd: f64, count: u32, competition: Option<f64>) -> bool {
    assert!(count > 0, "a liquidator among none");
    let mean = match competition {
        None => competitors_mean(repay_usd),
        Some(scale) => competitors_mean(repay_usd) * scale,
    };
    let chance = (mean / f64::from(count)).min(1.0);
    u < chance
}

/// How a liquidator bids, drawn once per liquidator: a share of the bonus
/// it expects, paid as a Jito tip or as a priority fee. Fees and tips took
/// 25% of the liquidators' gross bonus overall and 28 to 44% by size;
/// tips were in 46% of wins (NOTES.md §3 and §6).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bid {
    /// Of the bonus it expects to keep, 0.25 to 0.45.
    pub share: f64,
    /// It tips through a bundle rather than paying a priority fee.
    pub tips: bool,
}

const BID_SHARE_MIN: f64 = 0.25;
const BID_SHARE_MAX: f64 = 0.45;
const TIPPING_SHARE: f64 = 0.46;

impl Bid {
    /// Liquidator `index`'s, from the scenario's `seed` and the session's
    /// race (the research's shares where it says nothing).
    pub fn draw(seed: u64, index: u32, race: &Race) -> Self {
        let [low, high] = race.bid_share.unwrap_or([BID_SHARE_MIN, BID_SHARE_MAX]);
        let tipping = race.tipping_share.unwrap_or(TIPPING_SHARE);
        let share = low + (high - low) * draw(seed, "liquidator-bid", &[index.into()]);
        let tips = draw(seed, "liquidator-tips", &[index.into()]) < tipping;
        assert!((low..=high).contains(&share));
        Self { share, tips }
    }

    /// What it pays to liquidate `repay_usd` at a bonus of `bonus_bps`, in
    /// lamports at `sol_usd`.
    pub fn lamports(&self, repay_usd: f64, bonus_bps: u16, sol_usd: f64) -> u64 {
        assert!(sol_usd > 0.0, "SOL has a price");
        let kept_usd = repay_usd.max(0.0) * f64::from(bonus_bps) / 10_000.0 * BONUS_KEPT_SHARE;
        (self.share * kept_usd / sol_usd * 1e9).round() as u64
    }
}

/// Compute the keeper's push asks for: Scope's refresh of a price and a
/// TWAP entry, about 30,000 units each on mainnet's keepers, with margin.
const KEEPER_CU_LIMIT: u32 = 100_000;
/// The most compute a liquidator's transaction asks for, a push included:
/// mainnet's liquidations used 333k at the median and 450k at p90
/// (NOTES.md §3). Its simulation sets the limit under this.
const LIQUIDATION_CU_LIMIT_MAX: u32 = 600_000;
/// The margin on a simulation's units, as the bot's sends get
/// ([`crate::txlayer::client`]).
const UNITS_MARGIN: f64 = 0.1;
const UNITS_FLOOR: u64 = 1_000;

impl KaminoActors {
    /// The keeper's turn: when its next push falls before the next step's
    /// turn, it sends one refresh per pushed feed, each of its entries,
    /// timed to land on its cadence (the layer adds its route's latency).
    /// Without the layer it lands its push at once, once due.
    pub(super) async fn keeper_acts(&mut self, at: u64) {
        let Some(keeper) = self.scope_keeper.as_mut() else {
            return;
        };
        let due = match &self.engine {
            Some(_) => keeper.next_at <= at + SLOTS_PER_STEP,
            None => keeper.next_at <= at,
        };
        if !due {
            return;
        }
        let delay_slots = keeper.next_at.saturating_sub(at + 1);
        while keeper.next_at <= at + SLOTS_PER_STEP {
            keeper.next_at += keeper.spec.every_slots;
        }
        let feeds = self.pushed_feeds();
        let keeper = self.scope_keeper.as_ref().expect("checked above");
        for (feed, entries) in feeds {
            let push = crate::scope::refresh_fixed_prices(&feed, &entries);
            let result = match &self.engine {
                Some(engine) => {
                    let plan = TransactionPlan::from_group(
                        &InstructionGroup::new("scope push", vec![push]),
                        KEEPER_CU_LIMIT,
                    )
                    .with_compute_unit_price(keeper.spec.cu_price);
                    submit(
                        engine,
                        self.chain.as_ref(),
                        &keeper.keypair,
                        "scope-keeper",
                        &plan,
                        None,
                        delay_slots,
                    )
                    .await
                    .map(|_| ())
                }
                None => {
                    let market = self.books.markets()[0];
                    super::land(
                        self.chain.as_ref(),
                        &self.books,
                        &market,
                        &keeper.keypair,
                        "scope push",
                        vec![push],
                    )
                    .await
                }
            };
            if let Err(error) = result {
                warn!(%error, "the Scope keeper's push failed");
            }
        }
    }

    /// The liquidators' turn under the layer: what resolved since the last
    /// one is logged, then each liquidator, in its drawn order, looks at
    /// every obligation at upstream prices and sends what its draws say.
    pub(super) async fn liquidators_bid(&mut self, engine: &Arc<TxEngine>) {
        self.resolve_attempts(engine);
        // In the borrowers' order, not their addresses': each role plays a
        // session with keys of its own, so its obligations' addresses
        // differ, and the order the liquidators send in sets their
        // sequence numbers, which the layer's latency draws are keyed by.
        // Sorted by address, the reference and the bot would meet
        // different races on the same scenario.
        let mut obligations = self.obligations().await;
        obligations.sort_by_key(|(address, _)| {
            (
                self.borrower_index
                    .get(address)
                    .copied()
                    .unwrap_or(u64::MAX),
                *address,
            )
        });
        let upstream = super::upstream_prices(self.chain.as_ref(), &self.pushed).await;
        let mut readings: BTreeMap<Pubkey, Reading> = BTreeMap::new();
        for l in 0..self.liquidators.len() {
            for (place, (address, market)) in obligations.iter().enumerate() {
                if self
                    .attempts
                    .iter()
                    .any(|a| a.liquidator == l && a.obligation == *address)
                {
                    continue;
                }
                if !readings.contains_key(address) {
                    match kamino::read_obligation(
                        self.chain.as_ref(),
                        &self.books,
                        address,
                        market,
                        &self.keeper,
                    )
                    .await
                    {
                        Ok(reading) => {
                            readings.insert(*address, reading);
                        }
                        Err(error) => {
                            warn!(%address, %error, "couldn't read an obligation");
                            continue;
                        }
                    }
                }
                let key = self
                    .borrower_index
                    .get(address)
                    .copied()
                    .unwrap_or((self.borrower_index.len() + place) as u64);
                let reading = readings[address].clone();
                if let Some(attempt) = self
                    .bid_for(l, key, address, market, &reading, &upstream)
                    .await
                {
                    self.attempts.push(attempt);
                }
            }
        }
    }

    /// Liquidator `l`'s bid for the obligation at `address`, the `key`th the
    /// draws know, if it sends one now.
    async fn bid_for(
        &mut self,
        l: usize,
        key: u64,
        address: &Pubkey,
        market: &Pubkey,
        reading: &Reading,
        upstream: &BTreeMap<Pubkey, f64>,
    ) -> Option<Attempt> {
        let obligation = reading.obligation.as_ref()?;
        let health = upstream_health(obligation, |r| ratio(reading, upstream, r))?;
        if health >= 1.0 {
            self.liquidators[l].seen.remove(address);
            return None;
        }
        let step = self.step;
        let episode = *self.liquidators[l].seen.entry(*address).or_insert(step);
        if self.refused.contains(&(l, *address, episode)) {
            return None;
        }
        let found = self.opportunity(market, reading, upstream).await?;
        let count = u32::try_from(self.liquidators.len()).expect("at most five");
        let index = u64::from(self.liquidators[l].index);
        let joins = draw(self.seed, "liquidator-joins", &[index, key, episode]);
        if !competes(joins, found.repay_usd, count, self.race.competition) {
            return None;
        }
        let latency = latency_slots(
            draw(self.seed, "liquidator-latency", &[index, key, episode]),
            found.repay_usd,
            self.race.same_slot_share,
        );
        if step < episode + latency / SLOTS_PER_STEP {
            return None;
        }
        let payer = self.liquidators[l].keypair.pubkey();
        let instructions = self.liquidation(&payer, address, market, obligation, &found);
        let sent = self
            .send_liquidation(l, market, instructions, &found, latency)
            .await;
        match sent {
            Ok((signature, bid_lamports, tipped)) => Some(Attempt {
                liquidator: l,
                obligation: *address,
                signature,
                amount: found.amount,
                step,
                latency_slots: latency,
                bid_lamports,
                tipped,
                pushed: found.pushes,
            }),
            Err(error) => {
                warn!(by = self.liquidators[l].name, %error, "a liquidation wasn't sent");
                self.refused.insert((l, *address, episode));
                None
            }
        }
    }

    /// What liquidating `reading`'s obligation would repay and take: the
    /// close factor's share of its largest debt against its largest
    /// deposit, each a reserve the session knows, and what that is worth
    /// upstream.
    async fn opportunity(
        &self,
        market: &Pubkey,
        reading: &Reading,
        upstream: &BTreeMap<Pubkey, f64>,
    ) -> Option<Opportunity> {
        let obligation = reading.obligation.as_ref()?;
        let largest = |values: Vec<(f64, Pubkey)>| {
            values
                .into_iter()
                .filter_map(|(v, r)| self.books.at(&r).map(|b| (v, b.clone())))
                .max_by(|a, b| a.0.total_cmp(&b.0))
                .map(|(_, b)| b)
        };
        let borrows = obligation
            .borrows
            .iter()
            .map(|b| (b.market_value, b.reserve));
        let deposits = obligation
            .deposits
            .iter()
            .map(|d| (d.market_value, d.reserve));
        let repay = largest(borrows.collect())?;
        let withdraw = largest(deposits.collect())?;
        let close_factor = self
            .chain
            .get_account(market)
            .await
            .ok()
            .flatten()
            .and_then(|a| klend::LendingMarket::decode(&a.data))
            .map_or(0.2, |m| f64::from(m.close_factor_pct) / 100.0);
        let (_, owed) = reading.balance(&repay);
        let amount = owed * close_factor;
        let debt_price = reading.reserves.get(&repay.keys.address)?.price
            * ratio(reading, upstream, &repay.keys.address);
        let bonus_bps = reading
            .reserves
            .get(&withdraw.keys.address)?
            .min_liquidation_bonus_bps;
        Some(Opportunity {
            amount,
            repay_usd: amount * debt_price,
            bonus_bps,
            // Scope still reads it healthy: the liquidation pushes it first.
            pushes: obligation.health().is_none_or(|h| h >= 1.0),
            sol_usd: self.sol_usd(reading, upstream),
            repay,
            withdraw,
        })
    }

    /// The liquidation's instructions, as `payer`'s: the push if it needs
    /// one, its token accounts, the refreshes, the liquidation.
    fn liquidation(
        &self,
        payer: &Pubkey,
        address: &Pubkey,
        market: &Pubkey,
        obligation: &klend::Obligation,
        found: &Opportunity,
    ) -> Vec<Instruction> {
        let (repay, withdraw) = (&found.repay.keys, &found.withdraw.keys);
        let mut instructions: Vec<Instruction> = Vec::new();
        if found.pushes {
            instructions.extend(self.push_for(&obligation.reserves()));
        }
        instructions.extend(klend::liquidator_accounts(payer, repay, withdraw));
        let mut reserves = obligation.reserves();
        for extra in [repay.address, withdraw.address] {
            if !reserves.contains(&extra) {
                reserves.push(extra);
            }
        }
        for reserve in &reserves {
            if let Some(book) = self.books.at(reserve) {
                instructions.push(klend::refresh_reserve(&book.keys));
            }
        }
        instructions.push(klend::refresh_obligation(
            market,
            address,
            &obligation.reserves(),
        ));
        instructions.push(klend::liquidate(
            payer,
            address,
            repay,
            withdraw,
            atoms(found.amount, found.repay.decimals),
            0,
        ));
        instructions
    }

    /// Simulate liquidator `l`'s `instructions`, price them by its bid and
    /// send them into the layer `latency` slots late. Returns the
    /// signature, what it bid in lamports, and whether as a tip.
    async fn send_liquidation(
        &self,
        l: usize,
        market: &Pubkey,
        instructions: Vec<Instruction>,
        found: &Opportunity,
        latency: u64,
    ) -> Result<(String, u64, bool), String> {
        let liquidator = &self.liquidators[l];
        let mut tables: Vec<_> = self
            .books
            .lookup_tables
            .get(market)
            .cloned()
            .into_iter()
            .collect();
        tables.extend(self.tables.iter().cloned());
        let group = InstructionGroup::new("liquidate", instructions);
        let simulated = TransactionPlan::from_group(&group, LIQUIDATION_CU_LIMIT_MAX)
            .with_lookup_tables(&tables);
        let units = simulate(self.chain.as_ref(), &liquidator.keypair, &simulated).await?;
        let limit = ((units as f64 * (1.0 + UNITS_MARGIN)) as u64 + UNITS_FLOOR)
            .min(u64::from(LIQUIDATION_CU_LIMIT_MAX)) as u32;
        let plan = TransactionPlan::from_group(&group, limit).with_lookup_tables(&tables);
        let bid = liquidator.bid;
        let lamports = found.sol_usd.map_or(0, |price| {
            bid.lamports(found.repay_usd, found.bonus_bps, price)
        });
        let (plan, tip) = match bid.tips {
            true => (plan, Some(lamports.max(jito::MIN_TIP))),
            false => {
                let price = u64::try_from(u128::from(lamports) * 1_000_000 / u128::from(limit))
                    .unwrap_or(u64::MAX);
                (plan.with_compute_unit_price(price), None)
            }
        };
        let engine = self.engine.as_ref().expect("bidding under the layer");
        let signature = submit(
            engine,
            self.chain.as_ref(),
            &liquidator.keypair,
            &liquidator.name,
            &plan,
            tip,
            latency % SLOTS_PER_STEP,
        )
        .await?;
        Ok((signature, tip.unwrap_or(lamports), tip.is_some()))
    }

    /// Every pushed feed and its pushed entries, each once.
    fn pushed_feeds(&self) -> Vec<(crate::scope::Feed, Vec<u16>)> {
        let mut feeds: Vec<(crate::scope::Feed, Vec<u16>)> = Vec::new();
        for p in &self.pushed {
            let at = match feeds.iter().position(|(f, _)| *f == p.feed) {
                Some(at) => at,
                None => {
                    feeds.push((p.feed, Vec::new()));
                    feeds.len() - 1
                }
            };
            for e in &p.entries {
                if !feeds[at].1.contains(e) {
                    feeds[at].1.push(*e);
                }
            }
        }
        assert!(feeds.len() <= self.pushed.len());
        feeds
    }

    /// The push of every pushed entry `reserves` read, one refresh of
    /// their feed: Scope takes one refresh per transaction, at its head.
    fn push_for(&self, reserves: &[Pubkey]) -> Option<Instruction> {
        let mut feed = None;
        let mut entries: Vec<u16> = Vec::new();
        for p in self.pushed.iter().filter(|p| reserves.contains(&p.reserve)) {
            if feed.is_some_and(|f| f != p.feed) {
                continue;
            }
            feed = Some(p.feed);
            for e in &p.entries {
                if !entries.contains(e) {
                    entries.push(*e);
                }
            }
        }
        feed.map(|feed| crate::scope::refresh_fixed_prices(&feed, &entries))
    }

    /// A SOL's price in USD: the upstream of the reserve of wrapped SOL, or
    /// Scope's price there, or the layer's.
    fn sol_usd(&self, reading: &Reading, upstream: &BTreeMap<Pubkey, f64>) -> Option<f64> {
        let wrapped = self
            .books
            .reserves
            .iter()
            .find(|b| b.keys.mint == WRAPPED_SOL_MINT)
            .map(|b| b.keys.address);
        wrapped
            .and_then(|r| {
                upstream
                    .get(&r)
                    .copied()
                    .or_else(|| reading.reserves.get(&r).map(|x| x.price))
            })
            .or(self.sol_usd)
            .filter(|p| p.is_finite() && *p > 0.0)
    }

    /// Log every liquidation that resolved since the last turn; the
    /// liquidator may then try again where the obligation is still
    /// liquidatable, as a new episode.
    fn resolve_attempts(&mut self, engine: &TxEngine) {
        let mut still = Vec::new();
        for attempt in std::mem::take(&mut self.attempts) {
            let (landed_at, error) = match engine.fate(&attempt.signature) {
                None | Some(Fate::Pending) => {
                    still.push(attempt);
                    continue;
                }
                Some(Fate::Landed {
                    slot, ok, error, ..
                }) => (
                    Some(slot.saturating_sub(self.base_slot)),
                    if ok {
                        None
                    } else {
                        Some(error.unwrap_or_default())
                    },
                ),
                Some(Fate::Failed(reason)) => (None, Some(short(&reason))),
                Some(Fate::Expired) => (None, Some("expired".into())),
            };
            let liquidator = &mut self.liquidators[attempt.liquidator];
            liquidator.seen.remove(&attempt.obligation);
            self.log.lock().expect("log").push(ActorAction {
                step: attempt.step,
                by: liquidator.name.clone(),
                action: "liquidate",
                obligation: attempt.obligation.to_string(),
                amount: attempt.amount,
                error,
                landed_at,
                latency_slots: Some(attempt.latency_slots),
                bid_lamports: Some(attempt.bid_lamports),
                tipped: attempt.tipped,
                pushed: attempt.pushed,
            });
        }
        self.attempts = still;
    }
}

/// A liquidation one of the liquidators may make, as it reads it.
struct Opportunity {
    /// The reserve it repays and the one it takes collateral from.
    repay: kamino::Book,
    withdraw: kamino::Book,
    /// Units of the debt asset it repays, and their worth upstream.
    amount: f64,
    repay_usd: f64,
    /// The collateral's least bonus, which it expects.
    bonus_bps: u16,
    /// Scope still reads the obligation healthy: it pushes first.
    pushes: bool,
    sol_usd: Option<f64>,
}

/// A reserve's upstream over the price its refresh used in `reading`: 1
/// where it isn't pushed.
fn ratio(reading: &Reading, upstream: &BTreeMap<Pubkey, f64>, reserve: &Pubkey) -> f64 {
    match (upstream.get(reserve), reading.reserves.get(reserve)) {
        (Some(up), Some(stored)) if stored.price > 0.0 => up / stored.price,
        _ => 1.0,
    }
}

/// Most characters of a failure's reason an action keeps: a refused
/// bundle's reason carries the whole transaction's logs.
const REASON_CHARS_MAX: usize = 160;

/// `reason` up to the logs a refused bundle appends, at most
/// [`REASON_CHARS_MAX`] characters.
fn short(reason: &str) -> String {
    let head = reason
        .split(", TransactionMetadata")
        .next()
        .unwrap_or(reason);
    head.chars().take(REASON_CHARS_MAX).collect()
}

/// Wrapped SOL's mint.
const WRAPPED_SOL_MINT: Pubkey =
    solana_pubkey::pubkey!("So11111111111111111111111111111111111111112");

/// The units `plan` uses, signed by `signer`, from a simulation on the
/// chain as it is.
pub(crate) async fn simulate(
    chain: &dyn ChainClient,
    signer: &Keypair,
    plan: &TransactionPlan,
) -> Result<u64, String> {
    let blockhash = chain
        .get_latest_blockhash()
        .await
        .map_err(|e| e.to_string())?;
    let unsigned = plan
        .versioned(&signer.pubkey(), blockhash)
        .map_err(|e| e.to_string())?;
    let outcome = chain
        .simulate(&unsigned, &[])
        .await
        .map_err(|e| e.to_string())?;
    if let Some(error) = outcome.error {
        let tail = outcome.logs[outcome.logs.len().saturating_sub(4)..].join(" | ");
        return Err(format!("{error}: {tail}"));
    }
    Ok(outcome
        .units_consumed
        .unwrap_or(u64::from(LIQUIDATION_CU_LIMIT_MAX)))
}

/// Sign `plan` as `signer` and put it in the layer's pool as `sender`'s,
/// `delay_slots` late, through the block engine: alone, or with a tip
/// transaction as a bundle. Returns its signature.
pub(crate) async fn submit(
    engine: &TxEngine,
    chain: &dyn ChainClient,
    signer: &Keypair,
    sender: &str,
    plan: &TransactionPlan,
    tip: Option<u64>,
    delay_slots: u64,
) -> Result<String, String> {
    let blockhash = chain
        .get_latest_blockhash()
        .await
        .map_err(|e| e.to_string())?;
    let unsigned = plan
        .versioned(&signer.pubkey(), blockhash)
        .map_err(|e| e.to_string())?;
    let signed =
        VersionedTransaction::try_new(unsigned.message, &[signer]).map_err(|e| e.to_string())?;
    let signature = signed.signatures[0].to_string();
    let mut wires = vec![wincode::serialize(&signed).map_err(|e| e.to_string())?];
    if let Some(lamports) = tip {
        let tip_account: Pubkey = jito::TIP_ACCOUNTS
            [(signature.len() + wires.len()) % jito::TIP_ACCOUNTS.len()]
        .parse()
        .expect("a tip account");
        let mut data = 2u32.to_le_bytes().to_vec();
        data.extend(lamports.to_le_bytes());
        let pay = Instruction::new_with_bytes(
            pool::SYSTEM,
            &data,
            vec![
                solana_transaction::AccountMeta::new(signer.pubkey(), true),
                solana_transaction::AccountMeta::new(tip_account, false),
            ],
        );
        let message =
            solana_message::Message::new_with_blockhash(&[pay], Some(&signer.pubkey()), &blockhash);
        let tipped = VersionedTransaction::try_new(
            solana_message::VersionedMessage::Legacy(message),
            &[signer],
        )
        .map_err(|e| e.to_string())?;
        wires.push(wincode::serialize(&tipped).map_err(|e| e.to_string())?);
    }
    let bundle = wires.len() > 1;
    engine
        .submit(Submission {
            sender: sender.to_string(),
            wires,
            route: RouteKind::Jito,
            bundle,
            retry_until_slot: None,
            resign: false,
            delay_slots,
            dropped: false,
        })
        .await
        .map_err(|refusal| refusal.message)?;
    Ok(signature)
}

/// An obligation's health at upstream prices: `ratio` is each reserve's
/// upstream over the price its refresh used. The program's own values
/// give each side's weights, so an elevation group's thresholds and borrow
/// factors hold: the deposits at their liquidation threshold, over the
/// debt at its borrow factor. Below 1, liquidatable. `None` without debt.
pub fn upstream_health(
    obligation: &crate::venue::kamino::klend::Obligation,
    ratio: impl Fn(&solana_pubkey::Pubkey) -> f64,
) -> Option<f64> {
    if obligation.borrowed_assets_market_value <= 0.0 || obligation.deposited_value <= 0.0 {
        return None;
    }
    let threshold = obligation.unhealthy_borrow_value / obligation.deposited_value;
    let borrow_factor =
        obligation.borrow_factor_adjusted_debt_value / obligation.borrowed_assets_market_value;
    let deposits: f64 = obligation
        .deposits
        .iter()
        .map(|d| d.market_value * ratio(&d.reserve))
        .sum();
    let debt: f64 = obligation
        .borrows
        .iter()
        .map(|b| b.market_value * ratio(&b.reserve))
        .sum();
    (debt > 0.0).then(|| threshold * deposits / (borrow_factor * debt))
}

#[cfg(test)]
mod tests {
    use solana_pubkey::Pubkey;

    use super::*;
    use crate::venue::kamino::klend::{Borrow, Deposit, Obligation};

    /// The draws reproduce the research's shares over many episodes.
    #[test]
    fn latencies_follow_the_research() {
        let n = 20_000u64;
        let draws = |usd: f64| -> Vec<u64> {
            (0..n)
                .map(|i| latency_slots(draw(9, "t", &[i]), usd, None))
                .collect()
        };
        let share = |slots: &[u64], at_most: u64| {
            slots.iter().filter(|s| **s <= at_most).count() as f64 / slots.len() as f64
        };
        let small = draws(100.0);
        assert!((share(&small, 0) - 0.50).abs() < 0.02);
        assert!((share(&small, 2) - 0.71).abs() < 0.02);
        assert!((share(&small, 21) - 0.90).abs() < 0.02);
        let large = draws(5_000.0);
        assert!((share(&large, 0) - 0.93).abs() < 0.01);
        assert!(large.iter().chain(&small).all(|s| *s <= LATENCY_SLOTS_MAX));
        assert_eq!(latency_slots(0.0, 1.0, None), 0);
        assert_eq!(latency_slots(0.999_999, 1.0, None), LATENCY_SLOTS_MAX);
    }

    /// A session's same-slot share moves the lag and keeps its shape; at
    /// the research's 0.5 it is the research's lag.
    #[test]
    fn a_sessions_same_slot_share_keeps_the_lags_shape() {
        let n = 20_000u64;
        let share = |usd: f64, same: f64, at_most: u64| {
            (0..n)
                .filter(|i| latency_slots(draw(9, "t", &[*i]), usd, Some(same)) <= at_most)
                .count() as f64
                / n as f64
        };
        assert!((share(100.0, 0.3, 0) - 0.30).abs() < 0.02);
        assert!((share(100.0, 0.3, 2) - (0.3 + 0.7 * 0.42)).abs() < 0.02);
        assert!((share(100.0, 0.7, 21) - (0.7 + 0.3 * 0.8)).abs() < 0.02);
        assert!((share(5_000.0, 0.3, 0) - (1.0 - 0.07 * 1.4)).abs() < 0.01);
        for i in 0..2_000u64 {
            let u = draw(4, "u", &[i]);
            for usd in [5.0, 500.0, 5_000.0] {
                let research = latency_slots(u, usd, None);
                let at_half = latency_slots(u, usd, Some(0.5));
                assert!(research.abs_diff(at_half) <= 1, "{u} {usd}");
            }
        }
        assert_eq!(latency_slots(0.999_999, 1.0, Some(0.95)), LATENCY_SLOTS_MAX);
    }

    #[test]
    fn a_race_out_of_range_is_refused() {
        assert_eq!(Race::default().validate(), Ok(()));
        let bad = [
            Race {
                bid_share: Some([0.5, 0.4]),
                ..Race::default()
            },
            Race {
                bid_share: Some([0.0, 0.4]),
                ..Race::default()
            },
            Race {
                bid_share: Some([0.5, 1.2]),
                ..Race::default()
            },
            Race {
                tipping_share: Some(1.5),
                ..Race::default()
            },
            Race {
                same_slot_share: Some(0.0),
                ..Race::default()
            },
            Race {
                competition: Some(-1.0),
                ..Race::default()
            },
            Race {
                competition: Some(f64::NAN),
                ..Race::default()
            },
        ];
        for race in bad {
            assert!(race.validate().is_err(), "{race:?}");
        }
        let parsed: Race = serde_json::from_str(r#"{"bid_share": [0.2, 0.6]}"#).unwrap();
        assert_eq!(parsed.bid_share, Some([0.2, 0.6]));
        assert!(serde_json::from_str::<Race>(r#"{"bid": 1}"#).is_err());
    }

    #[test]
    fn a_refused_bundles_reason_drops_its_logs() {
        let reason = "Jito bundle couldn't be executed: Execution failed for transaction 1: (InstructionError(8, Custom(6016)), TransactionMetadata { logs: [] })";
        assert_eq!(
            short(reason),
            "Jito bundle couldn't be executed: Execution failed for transaction 1: (InstructionError(8, Custom(6016))"
        );
        assert_eq!(short(&"x".repeat(500)).len(), REASON_CHARS_MAX);
    }

    #[test]
    fn competition_grows_with_size() {
        let joined = |usd: f64, count: u32| {
            (0..10_000u64)
                .filter(|i| competes(draw(3, "c", &[*i]), usd, count, None))
                .count() as f64
                / 10_000.0
                * f64::from(count)
        };
        assert!((joined(5.0, 5) - 0.56).abs() < 0.05);
        assert!((joined(500.0, 5) - 2.6).abs() < 0.1);
        assert!((joined(5_000.0, 5) - 4.4).abs() < 0.1);
        assert!(
            (joined(5_000.0, 3) - 3.0).abs() < 1e-9,
            "all three, short of 4.4"
        );
        let scaled = (0..10_000u64)
            .filter(|i| competes(draw(3, "c", &[*i]), 500.0, 5, Some(0.5)))
            .count() as f64
            / 10_000.0
            * 5.0;
        assert!((scaled - 1.3).abs() < 0.1, "{scaled}");
        assert!(!competes(0.0, 500.0, 5, Some(0.0)), "no competition");
    }

    #[test]
    fn bids_are_a_share_of_the_bonus_kept() {
        let research = Race::default();
        let bids: Vec<Bid> = (0..2_000).map(|i| Bid::draw(5, i, &research)).collect();
        let tipping = bids.iter().filter(|b| b.tips).count() as f64 / bids.len() as f64;
        assert!((tipping - 0.46).abs() < 0.03, "{tipping}");
        assert!(bids.iter().all(|b| (0.25..=0.45).contains(&b.share)));
        assert_eq!(
            Bid::draw(5, 7, &research),
            Bid::draw(5, 7, &research),
            "a pure draw"
        );
        let session = Race {
            bid_share: Some([0.5, 0.7]),
            tipping_share: Some(1.0),
            ..Race::default()
        };
        assert!(
            (0..200)
                .map(|i| Bid::draw(5, i, &session))
                .all(|b| b.tips && (0.5..=0.7).contains(&b.share))
        );
        let bid = Bid {
            share: 0.4,
            tips: false,
        };
        // $1,000 repaid at 100 bp, half kept: $5, of which 40% is $2, at
        // $100 a SOL 0.02 SOL.
        assert_eq!(bid.lamports(1_000.0, 100, 100.0), 20_000_000);
        assert_eq!(bid.lamports(-1.0, 100, 100.0), 0);
    }

    fn obligation(collateral: (Pubkey, f64), debt: (Pubkey, f64)) -> Obligation {
        let (threshold, borrow_factor) = (0.8, 1.25);
        Obligation {
            last_update_slot: 0,
            lending_market: Pubkey::new_unique(),
            owner: Pubkey::new_unique(),
            deposits: vec![Deposit {
                reserve: collateral.0,
                amount: 1,
                market_value: collateral.1,
            }],
            borrows: vec![Borrow {
                reserve: debt.0,
                amount: 1.0,
                market_value: debt.1,
            }],
            deposited_value: collateral.1,
            borrow_factor_adjusted_debt_value: debt.1 * borrow_factor,
            borrowed_assets_market_value: debt.1,
            allowed_borrow_value: 0.0,
            unhealthy_borrow_value: collateral.1 * threshold,
        }
    }

    #[test]
    fn health_upstream_scales_each_side_by_its_move() {
        let (sol, usdc) = (Pubkey::new_unique(), Pubkey::new_unique());
        // 1,000 of SOL at 80% against 600 of USDC at a factor of 1.25:
        // 800 / 750.
        let o = obligation((sol, 1_000.0), (usdc, 600.0));
        let at_scope = upstream_health(&o, |_| 1.0).unwrap();
        assert!((at_scope - 800.0 / 750.0).abs() < 1e-12);
        assert!(
            (at_scope - o.health().unwrap()).abs() < 1e-12,
            "the program's own"
        );
        // SOL 7% lower upstream: 744 / 750, liquidatable.
        let down = upstream_health(&o, |r| if *r == sol { 0.93 } else { 1.0 }).unwrap();
        assert!((down - 744.0 / 750.0).abs() < 1e-12);
        assert!(down < 1.0);
        let mut empty = o.clone();
        empty.borrowed_assets_market_value = 0.0;
        assert_eq!(upstream_health(&empty, |_| 1.0), None);
    }

    #[test]
    fn a_keeper_out_of_range_is_refused() {
        assert_eq!(ScopeKeeper::default().validate(), Ok(()));
        let parsed: ScopeKeeper = serde_json::from_str("{}").unwrap();
        assert_eq!(parsed, ScopeKeeper::default());
        assert!(
            ScopeKeeper {
                every_slots: 0,
                ..ScopeKeeper::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            ScopeKeeper {
                cu_price: KEEPER_CU_PRICE_MAX + 1,
                ..ScopeKeeper::default()
            }
            .validate()
            .is_err()
        );
    }
}
