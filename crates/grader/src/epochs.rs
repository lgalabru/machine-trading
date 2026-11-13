//! Epoch boundaries inside a session: the clock jumps to the next epoch,
//! and the stake pools pay the epoch's rewards.
//!
//! A session is minutes of slots; an epoch is a day of them. A scenario
//! names the steps after which an epoch ends ([`EpochEvent`]): once that
//! step's blocks have landed, the clock moves to [`LANDING_SLOTS`] into the
//! next epoch, as mainnet would show it half an hour after the boundary
//! (Marinade pays a ticket no sooner), and before the bot's turn the pools
//! are where mainnet's keepers leave them:
//!
//! - **An SPL stake pool** (Jito's) earns the epoch's reward on its staked
//!   SOL, and its own `UpdateStakePoolBalance` crank sums it into the pool,
//!   takes the epoch fee in new tokens, and marks the pool current; until
//!   then the program refuses deposits. The reward is written to one
//!   validator's entry in the pool's validator list (a validator whose
//!   stake account the world doesn't hold, so no account can touch it), and
//!   every entry is marked updated, as `UpdateValidatorListBalance` would
//!   after a real epoch.
//! - **Marinade** earns it on its active stake: its state's active balance
//!   and `msol_price` move as its update crank would move them.
//!
//! Every write is scenario data, computed at setup from the snapshot's
//! pools and registered as a Surfpool override before the cheatcodes lock
//! (the fields written are ones no transaction changes within an epoch);
//! the crank is an ordinary transaction from a grader account. The clock
//! moves in the grader's own process, not through the locked RPC. Each
//! boundary is planned to the slot, and a session that reaches it anywhere
//! else fails rather than run on a different clock.

use std::{
    collections::BTreeMap,
    sync::{Arc, OnceLock},
};

use mtx_core::{Decimal, mandate::InstrumentRef};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_rpc_client::rpc_client::RpcClient;
use solana_rpc_client_api::config::RpcSendTransactionConfig;
use solana_signer::Signer;
use solana_transaction::Transaction;
use surfpool_sdk::{SimnetCommand, Surfnet};

use crate::{
    Error, Result,
    chain::{self, Override},
    session::{self, SLOTS_PER_STEP, grader_f64},
    snapshot::SLOT_TIME_MS,
    venue::{
        marinade::{self, MarinadeState},
        stake_pool::{
            self, CLOCK, NamedPool, POOLS, PoolState, VALIDATOR_LIST_HEADER, VALIDATOR_STAKE_LEN,
        },
    },
};

/// How far into the new epoch the clock lands: 7,500 slots of 250 ms, a
/// little over the 30 minutes Marinade holds a ticket past the boundary.
pub const LANDING_SLOTS: u64 = 7_500;
/// The blocks a boundary makes after the clock lands: one that applies
/// the epoch's writes, one that lands the cranks.
pub const WARP_BLOCKS: u64 = 2;
/// The largest reward an epoch can pay, in basis points: 10 is about 40%
/// a year at mainnet's epoch length.
pub const MAX_REWARD_BPS: f64 = 10.0;
/// What a scenario's override is filed under; Surfpool has no template by
/// that name, so the write goes through the registered layout ([`idl`]).
const TEMPLATE: &str = "mtx-epoch";

/// An epoch boundary in a scenario.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct EpochEvent {
    /// The step after whose blocks the epoch ends: the bot's turn at that
    /// step is the new epoch's first.
    pub step: u64,
    /// The epoch's staking reward on the pools' staked SOL, before their
    /// fees, in basis points.
    pub reward_bps: f64,
}

/// A scenario's boundaries come in order, each after a step that has a
/// step after it, and pay a reward in range.
pub fn validate(events: &[EpochEvent], steps: u64) -> std::result::Result<(), String> {
    for (i, event) in events.iter().enumerate() {
        if event.step + 1 >= steps {
            return Err(format!(
                "epochs: step {} has no step after it (the session has {steps})",
                event.step
            ));
        }
        if i > 0 && event.step <= events[i - 1].step {
            return Err("epochs: steps must increase".into());
        }
        if !(event.reward_bps.is_finite() && (0.0..=MAX_REWARD_BPS).contains(&event.reward_bps)) {
            return Err(format!(
                "epochs: reward_bps {} is outside [0, {MAX_REWARD_BPS}]",
                event.reward_bps
            ));
        }
    }
    Ok(())
}

/// One boundary, planned to the slot.
#[derive(Debug, Clone)]
struct Boundary {
    step: u64,
    /// The slot step `step`'s blocks end at, and where the clock lands.
    at_slot: u64,
    landing: u64,
    epoch: u64,
    first_slot: u64,
}

/// A pool whose epochs the session pays.
enum Paid {
    Spl {
        instrument: InstrumentRef,
        named: NamedPool,
        state: Box<PoolState>,
        /// The entry the rewards go to, and its active stake after each
        /// boundary.
        target: usize,
        active_after: Vec<u64>,
    },
    Marinade {
        instrument: InstrumentRef,
        /// The state's active balance and price after each boundary.
        after: Vec<(u64, u64)>,
    },
}

/// What a boundary did, for the session's report.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EpochRecord {
    pub step: u64,
    pub epoch: u64,
    /// The slot the clock landed at.
    pub slot: u64,
    pub reward_bps: f64,
    /// Each pool's SOL per token, before and after.
    pub rates: Vec<PoolRate>,
    /// A crank the pool refused: why. The bot shares the pool.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub refused: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PoolRate {
    pub instrument: InstrumentRef,
    pub before: f64,
    pub after: f64,
}

/// A session's planned boundaries, as the pools' seats tell a bot about
/// them: where each lands and the epoch it starts. Cheap to clone, and
/// filled when [`Epochs::schedule`] plans the session; empty before that,
/// and for a session without epochs.
#[derive(Clone, Default)]
pub struct EpochPlan(Arc<OnceLock<Vec<(u64, u64)>>>);

impl EpochPlan {
    /// The session's next boundary after `slot`: the slots until its step's
    /// blocks end there, which is when the clock jumps, and the epoch it
    /// starts. None once every planned boundary has passed.
    pub fn next(&self, slot: u64) -> Option<(u64, u64)> {
        self.0
            .get()?
            .iter()
            .find(|(at_slot, _)| *at_slot > slot)
            .map(|(at_slot, epoch)| (at_slot - slot, *epoch))
    }

    /// Plan boundaries at `(slot, epoch)`, in order. Only the first call
    /// counts.
    pub fn set(&self, boundaries: Vec<(u64, u64)>) {
        let _ = self.0.set(boundaries);
    }
}

/// A session's epoch boundaries.
pub struct Epochs {
    events: Vec<EpochEvent>,
    boundaries: Vec<Boundary>,
    paid: Vec<Paid>,
    crank: Keypair,
    records: Vec<EpochRecord>,
}

fn epoch_error(reason: String) -> Error {
    Error::Transaction {
        label: "an epoch boundary".into(),
        reason,
    }
}

impl Epochs {
    /// Plan `events` for a session whose first step starts at `base_slot`
    /// (the surfnet's slot now), on the pools of `instruments`, and
    /// register their writes. Setup only: the cheatcodes must still be
    /// open. `crank` sends the pools' cranks; it must hold SOL for fees.
    /// Returns the plan, and the slots the boundaries add to the session.
    pub fn schedule(
        rpc: &RpcClient,
        events: &[EpochEvent],
        instruments: &[InstrumentRef],
        base_slot: u64,
        crank: Keypair,
        plan: &EpochPlan,
    ) -> Result<(Self, u64)> {
        let schedule = rpc.get_epoch_schedule()?;
        let mut boundaries = Vec::new();
        let mut added = 0;
        for event in events {
            let at_slot = base_slot + (event.step + 1) * SLOTS_PER_STEP + added;
            let epoch = schedule.get_epoch(at_slot) + 1;
            let first_slot = schedule.get_first_slot_in_epoch(epoch);
            let landing = first_slot + LANDING_SLOTS;
            added += landing + WARP_BLOCKS - at_slot;
            boundaries.push(Boundary {
                step: event.step,
                at_slot,
                landing,
                epoch,
                first_slot,
            });
        }
        plan.set(boundaries.iter().map(|b| (b.at_slot, b.epoch)).collect());
        let mut paid = Vec::new();
        let mut layouts: BTreeMap<Pubkey, Vec<Layout>> = BTreeMap::new();
        let mut overrides = Vec::new();
        for instrument in instruments {
            if instrument.venue == marinade::VENUE {
                let data = rpc.get_account(&marinade::STATE)?.data;
                let state = MarinadeState::decode(&data)?;
                let virtual_lamports = state.total_virtual_staked_lamports();
                let (mut active, mut earned) = (state.total_active_balance, 0u64);
                let mut after = Vec::new();
                for (event, boundary) in events.iter().zip(&boundaries) {
                    let reward = reward(active, event.reward_bps);
                    active += reward;
                    earned += reward;
                    let price =
                        MarinadeState::price_for(virtual_lamports + earned, state.msol_supply);
                    after.push((active, price));
                    overrides.push(Override {
                        template_id: TEMPLATE,
                        account: marinade::STATE,
                        values: json!({
                            "total_active_balance": active,
                            "msol_price": price,
                        }),
                        slot: boundary.landing + 1 - base_slot,
                        label: format!("{instrument} epoch {}", boundary.epoch),
                    });
                }
                layouts
                    .entry(marinade::PROGRAM)
                    .or_default()
                    .push(Layout::marinade_state(&data));
                paid.push(Paid::Marinade {
                    instrument: instrument.clone(),
                    after,
                });
                continue;
            }
            let Some(named) = POOLS
                .iter()
                .find(|p| p.venue == instrument.venue && p.symbol == instrument.symbol)
            else {
                return Err(epoch_error(format!("{instrument} has no epochs to pay")));
            };
            let state = PoolState::decode(&rpc.get_account(&named.address)?.data)?;
            let list_data = rpc.get_account(&state.validator_list)?.data;
            let list = stake_pool::decode_validator_list(&list_data)?;
            let target = stake_pool::untouchable_validator(rpc, named, &list)?;
            // What the program's update would sum today, against what the
            // pool says it holds: the first boundary's reward makes up the
            // difference, so that the rate moves by the reward alone.
            let reserve = rpc.get_account(&state.reserve_stake)?;
            let reserve_rent = reserve
                .data
                .get(4..12)
                .map_or(0, |b| u64::from_le_bytes(b.try_into().expect("8 bytes")));
            let summed: u128 = u128::from(reserve.lamports.saturating_sub(reserve_rent))
                + list
                    .iter()
                    .map(|v| u128::from(v.active_stake_lamports + v.transient_stake_lamports))
                    .sum::<u128>();
            let drift = summed as i128 - i128::from(state.total_lamports);
            let (mut total, mut active) =
                (state.total_lamports, list[target].active_stake_lamports);
            let mut active_after = Vec::new();
            for (i, (event, boundary)) in events.iter().zip(&boundaries).enumerate() {
                let reward = reward(total, event.reward_bps);
                total += reward;
                let credited = i128::from(reward) - if i == 0 { drift } else { 0 };
                active = u64::try_from(i128::from(active) + credited).map_err(|_| {
                    epoch_error(format!(
                        "{instrument}: its validator list sums {drift} lamports away from the pool"
                    ))
                })?;
                active_after.push(active);
                let mut values = serde_json::Map::new();
                for i in 0..list.len() {
                    values.insert(
                        format!("validators.{i}.last_update_epoch"),
                        json!(boundary.epoch),
                    );
                }
                values.insert(
                    format!("validators.{target}.active_stake_lamports"),
                    json!(active),
                );
                overrides.push(Override {
                    template_id: TEMPLATE,
                    account: state.validator_list,
                    values: Value::Object(values),
                    slot: boundary.landing + 1 - base_slot,
                    label: format!("{instrument} epoch {}", boundary.epoch),
                });
            }
            layouts
                .entry(named.program)
                .or_default()
                .push(Layout::validator_list(&list_data, list.len()));
            paid.push(Paid::Spl {
                instrument: instrument.clone(),
                named: *named,
                state: Box::new(state),
                target,
                active_after,
            });
        }
        if !overrides.is_empty() {
            for (program, layouts) in layouts {
                chain::call(rpc, "surfnet_registerIdl", json!([idl(&program, &layouts)]))?;
            }
            chain::register_scenario(rpc, "epochs", overrides)?;
        }
        Ok((
            Self {
                events: events.to_vec(),
                boundaries,
                paid,
                crank,
                records: Vec::new(),
            },
            added,
        ))
    }

    /// After step `k`'s blocks landed at `slot`: if an epoch ends here,
    /// move the clock into the next one, let its writes land and crank the
    /// pools. Returns the slot the world is at.
    pub async fn warp(
        &mut self,
        surfnet: &Surfnet,
        rpc: &RpcClient,
        k: u64,
        slot: u64,
    ) -> Result<u64> {
        let Some(i) = self.boundaries.iter().position(|b| b.step == k) else {
            return Ok(slot);
        };
        let boundary = self.boundaries[i].clone();
        if slot != boundary.at_slot {
            return Err(epoch_error(format!(
                "step {k} ended at slot {slot}, not at {} as planned",
                boundary.at_slot
            )));
        }
        let before = self.rates(rpc)?;
        // The clock, as the sysvar holds it: slot, epoch start, epoch,
        // leader schedule epoch, unix time.
        let clock = rpc.get_account(&CLOCK)?.data;
        let unix = i64::from_le_bytes(clock[32..40].try_into().expect("8 bytes"));
        let seconds = |slots: u64| (slots * SLOT_TIME_MS / 1_000) as i64;
        let epoch_start = unix + seconds(boundary.first_slot - slot);
        surfnet.send_command(SimnetCommand::UpdateInternalClock(
            None,
            solana_clock::Clock {
                slot: boundary.landing,
                epoch_start_timestamp: epoch_start,
                epoch: boundary.epoch,
                leader_schedule_epoch: boundary.epoch + 1,
                unix_timestamp: epoch_start + seconds(LANDING_SLOTS),
            },
        ))?;
        // The clock moves before the next command: the block it makes is
        // the new epoch's.
        self.forward(surfnet, rpc, boundary.landing + 1).await?;
        self.check_writes(rpc, i)?;
        let mut sent = Vec::new();
        for paid in &self.paid {
            if let Paid::Spl {
                instrument,
                named,
                state,
                ..
            } = paid
            {
                let update = stake_pool::ix::update_stake_pool_balance(
                    &named.program,
                    &named.address,
                    state,
                );
                let blockhash = rpc.get_latest_blockhash()?;
                let tx = Transaction::new_signed_with_payer(
                    &[update],
                    Some(&self.crank.pubkey()),
                    &[&self.crank],
                    blockhash,
                );
                let signature = rpc.send_transaction_with_config(
                    &tx,
                    RpcSendTransactionConfig {
                        skip_preflight: true,
                        ..RpcSendTransactionConfig::default()
                    },
                )?;
                sent.push((instrument.clone(), signature));
            }
        }
        let now = self
            .forward(surfnet, rpc, boundary.landing + WARP_BLOCKS)
            .await?;
        let mut refused = Vec::new();
        for (instrument, signature) in sent {
            match rpc.get_signature_status(&signature)? {
                Some(Ok(())) => {}
                Some(Err(error)) => refused.push(format!("{instrument}: {error}")),
                None => refused.push(format!("{instrument}: the crank did not land")),
            }
        }
        let after = self.rates(rpc)?;
        self.records.push(EpochRecord {
            step: k,
            epoch: boundary.epoch,
            slot: boundary.landing,
            reward_bps: self.events[i].reward_bps,
            rates: before
                .into_iter()
                .zip(after)
                .map(|((instrument, before), (_, after))| PoolRate {
                    instrument,
                    before,
                    after,
                })
                .collect(),
            refused,
        });
        Ok(now)
    }

    async fn forward(&self, surfnet: &Surfnet, rpc: &RpcClient, to: u64) -> Result<u64> {
        let mut slot = rpc.get_slot()?;
        while slot < to {
            surfnet.send_command(SimnetCommand::SlotForward(None))?;
            slot = session::wait_for_slot(rpc, slot + 1).await?;
        }
        Ok(slot)
    }

    /// The `i`th boundary's writes are in the accounts: a surfnet that
    /// dropped them would pay no reward and say nothing.
    fn check_writes(&self, rpc: &RpcClient, i: usize) -> Result<()> {
        for paid in &self.paid {
            let (instrument, landed) = match paid {
                Paid::Spl {
                    instrument,
                    state,
                    target,
                    active_after,
                    ..
                } => {
                    let list = stake_pool::decode_validator_list(
                        &rpc.get_account(&state.validator_list)?.data,
                    )?;
                    let entry = &list[*target];
                    (
                        instrument,
                        entry.active_stake_lamports == active_after[i]
                            && list
                                .iter()
                                .all(|v| v.last_update_epoch == self.boundaries[i].epoch),
                    )
                }
                Paid::Marinade { instrument, after } => {
                    let state = MarinadeState::decode(&rpc.get_account(&marinade::STATE)?.data)?;
                    (
                        instrument,
                        (state.total_active_balance, state.msol_price) == after[i],
                    )
                }
            };
            if !landed {
                return Err(epoch_error(format!(
                    "{instrument}: the epoch's writes did not land"
                )));
            }
        }
        Ok(())
    }

    /// Each paid pool's SOL per token now.
    fn rates(&self, rpc: &RpcClient) -> Result<Vec<(InstrumentRef, f64)>> {
        self.paid
            .iter()
            .map(|paid| {
                Ok(match paid {
                    Paid::Spl {
                        instrument, named, ..
                    } => (
                        instrument.clone(),
                        grader_f64(
                            PoolState::decode(&rpc.get_account(&named.address)?.data)?.rate(),
                        ),
                    ),
                    Paid::Marinade { instrument, .. } => (
                        instrument.clone(),
                        grader_f64(
                            MarinadeState::decode(&rpc.get_account(&marinade::STATE)?.data)?.rate(),
                        ),
                    ),
                })
            })
            .collect()
    }

    /// What the boundaries did.
    pub fn records(&self) -> &[EpochRecord] {
        &self.records
    }
}

/// `bps` basis points of `lamports`, rounded down.
fn reward(lamports: u64, bps: f64) -> u64 {
    let reward = Decimal::from(lamports) * Decimal::try_from(bps).unwrap_or_default()
        / Decimal::from(10_000);
    reward.floor().try_into().unwrap_or(0)
}

/// An account's bytes as Surfpool's override reads them: its first 8
/// bytes, which pick the type, and the type's fields after them.
struct Layout {
    name: String,
    discriminator: [u8; 8],
    fields: Vec<Value>,
    types: Vec<Value>,
}

/// Bytes the layout skips over.
fn gap(name: &str, len: usize) -> Value {
    json!({ "name": name, "type": { "array": ["u8", len] } })
}

fn field(name: &str, ty: &str) -> Value {
    json!({ "name": name, "type": ty })
}

impl Layout {
    /// An SPL validator list: after the account type, the maximum and the
    /// first 3 bytes of the vector's length, its last byte and `len`
    /// entries. The vector can't grow within an epoch.
    fn validator_list(data: &[u8], len: usize) -> Self {
        debug_assert!(data.len() >= VALIDATOR_LIST_HEADER + len * VALIDATOR_STAKE_LEN);
        Self {
            name: "ValidatorList".into(),
            discriminator: data[..8].try_into().expect("8 bytes"),
            fields: vec![
                gap("length_high", 1),
                json!({
                    "name": "validators",
                    "type": { "array": [{ "defined": { "name": "ValidatorStakeInfo" } }, len] },
                }),
            ],
            types: vec![json!({
                "name": "ValidatorStakeInfo",
                "type": { "kind": "struct", "fields": [
                    field("active_stake_lamports", "u64"),
                    field("transient_stake_lamports", "u64"),
                    field("last_update_epoch", "u64"),
                    field("transient_seed_suffix", "u64"),
                    field("unused", "u32"),
                    field("validator_seed_suffix", "u32"),
                    field("status", "u8"),
                    field("vote_account_address", "pubkey"),
                ] },
            })],
        }
    }

    /// Marinade's state: its active balance and its price.
    fn marinade_state(data: &[u8]) -> Self {
        use marinade::offsets::{MSOL_PRICE, TOTAL_ACTIVE_BALANCE};
        Self {
            name: "State".into(),
            discriminator: data[..8].try_into().expect("8 bytes"),
            fields: vec![
                gap("before_total_active_balance", TOTAL_ACTIVE_BALANCE - 8),
                field("total_active_balance", "u64"),
                gap("before_msol_price", MSOL_PRICE - TOTAL_ACTIVE_BALANCE - 8),
                field("msol_price", "u64"),
            ],
            types: Vec::new(),
        }
    }
}

/// The layout a program's accounts are written through, as an Anchor IDL.
fn idl(program: &Pubkey, layouts: &[Layout]) -> Value {
    let mut types: Vec<Value> = Vec::new();
    for layout in layouts {
        types.push(json!({
            "name": layout.name,
            "type": { "kind": "struct", "fields": layout.fields },
        }));
        for ty in &layout.types {
            if !types.contains(ty) {
                types.push(ty.clone());
            }
        }
    }
    json!({
        "address": program.to_string(),
        "metadata": { "name": "mtx_epochs", "version": "0.1.0", "spec": "0.1.0" },
        "instructions": [],
        "accounts": layouts
            .iter()
            .map(|l| json!({ "name": l.name, "discriminator": l.discriminator }))
            .collect::<Vec<_>>(),
        "types": types,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A grade rescored from its report reads a stake session's section
    /// back, the epochs it crossed included.
    #[test]
    fn a_stake_report_reads_back() {
        use crate::stake::StakeReport;
        let report = StakeReport {
            epochs: vec![EpochRecord {
                step: 10,
                epoch: 1086,
                slot: 469_159_500,
                reward_bps: 1.4,
                rates: vec![PoolRate {
                    instrument: "marinade:stake-pool:MSOL".parse().unwrap(),
                    before: 1.4177,
                    after: 1.4179,
                }],
                refused: Vec::new(),
            }],
            ..StakeReport::default()
        };
        let text = serde_json::to_string(&report).unwrap();
        assert!(!text.contains("refused"));
        assert_eq!(serde_json::from_str::<StakeReport>(&text).unwrap(), report);
        let empty = serde_json::to_string(&StakeReport::default()).unwrap();
        assert_eq!(
            serde_json::from_str::<StakeReport>(&empty).unwrap(),
            StakeReport::default()
        );
    }

    fn event(step: u64, reward_bps: f64) -> EpochEvent {
        EpochEvent { step, reward_bps }
    }

    #[test]
    fn boundaries_come_in_order_before_the_last_step() {
        assert!(validate(&[event(3, 2.8), event(10, 2.8)], 20).is_ok());
        assert!(validate(&[event(19, 2.8)], 20).is_err(), "no step after it");
        assert!(validate(&[event(5, 2.8), event(5, 2.8)], 20).is_err());
        assert!(validate(&[event(5, -1.0)], 20).is_err());
        assert!(validate(&[event(5, 11.0)], 20).is_err());
        assert!(validate(&[event(5, f64::NAN)], 20).is_err());
    }

    #[test]
    fn a_reward_is_basis_points_of_the_stake_rounded_down() {
        assert_eq!(reward(10_213_089_853_649_410, 1.4), 1_429_832_579_510);
        assert_eq!(reward(9_999, 1.0), 0);
        assert_eq!(reward(1_000_000, 0.0), 0);
    }

    #[test]
    fn a_layout_reads_past_the_discriminator_to_its_fields() {
        let mut list = vec![2u8, 0xe8, 3, 0, 0, 2, 0, 0, 0];
        list.extend(vec![0u8; 2 * VALIDATOR_STAKE_LEN]);
        let idl = idl(&Pubkey::new_unique(), &[Layout::validator_list(&list, 2)]);
        assert_eq!(
            idl["accounts"][0]["discriminator"],
            json!([2, 0xe8, 3, 0, 0, 2, 0, 0])
        );
        assert_eq!(idl["types"][0]["type"]["fields"][1]["type"]["array"][1], 2);
        assert_eq!(idl["types"][1]["name"], "ValidatorStakeInfo");
        let state = vec![7u8; 700];
        let marinade = Layout::marinade_state(&state);
        // 8 + 368 bytes before the active balance, 128 between it and the price.
        assert_eq!(marinade.fields[0]["type"]["array"][1], 368);
        assert_eq!(marinade.fields[2]["type"]["array"][1], 128);
    }
}
