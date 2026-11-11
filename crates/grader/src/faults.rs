//! Execution faults: what can go wrong between a bot's `sendTransaction` and
//! the chain, planned from the scenario's seed.
//!
//! A scenario opts in with [`Faults`]. The session resolves it once into a
//! [`FaultPlan`], a pure function of the spec and the seed: it reads no clock
//! and draws nothing at run time, so every bot meets the same faults.
//!
//! - **Latency.** A send is held and lands at the first step boundary at or
//!   after its delay, never before the next block. Held sends land in the order
//!   they were sent.
//! - **Congestion.** In a window of steps a transaction priced below the floor
//!   waits for a block that will take it. `getRecentPrioritizationFees` shows
//!   the floor, so a bot can read it.
//! - **Blockhash expiry.** The n-th send is refused as `Blockhash not found`.
//!   Nothing lands; the bot needs a fresh blockhash and a new signature.
//! - **Drop.** The n-th send is accepted, its signature returned, and never
//!   forwarded. Its status reads null for good.
//! - **Lost confirmation.** The n-th transaction to land lands, but its status
//!   reads null for a few steps; sending it again with a new blockhash would
//!   run it twice.
//! - **RPC unavailable.** From the n-th read, a few reads fail with HTTP 429.
//!
//! Faults are counted on the bot's own calls, so a fault set at an early number
//! fires at the same point of every run that gets that far, whatever the bot
//! does: sends count every `sendTransaction` the guard accepts for a turn,
//! landings count the transactions the chain ran, reads count every other
//! method. Anything the plan randomises (the floor, a delay, the fee samples)
//! comes from the scenario's `seed`.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::scenario::SplitMix64;

/// The scenario's fault spec. Everything but the seed is optional.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Faults {
    /// What the plan's draws come from.
    pub seed: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency: Option<Latency>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub congestion: Option<Congestion>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blockhash_expiry: Option<SendNumbers>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drop: Option<SendNumbers>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lost_confirmation: Option<LostConfirmation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rpc_unavailable: Option<RpcUnavailable>,
}

/// A send's delay in slots: uniform in `slots`, or, with probability `p`, in
/// the spike's range.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Latency {
    pub slots: (u64, u64),
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spike: Option<Spike>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Spike {
    pub p: f64,
    pub slots: (u64, u64),
}

/// A window of steps, `from_step` up to but not including `to_step`, in which
/// a transaction priced below the floor waits. The floor, in micro-lamports
/// per compute unit, is drawn once from the range.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Congestion {
    pub from_step: u64,
    pub to_step: u64,
    pub floor_micro_lamports: (u64, u64),
}

/// The bot's sends to fault, counting from 1.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SendNumbers {
    pub sends: Vec<u64>,
}

/// Of the bot's transactions the chain ran, the numbered ones (from 1) read as
/// unknown for `hide_steps` steps.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct LostConfirmation {
    pub landings: Vec<u64>,
    pub hide_steps: u64,
}

/// From each numbered read (from 1), `count` reads fail with HTTP 429.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RpcUnavailable {
    pub reads: Vec<u64>,
    pub count: u64,
}

impl Faults {
    /// A spec must make sense for a session of `steps` steps.
    pub fn validate(&self, steps: u64) -> Result<(), String> {
        let range = |what: &str, (lo, hi): (u64, u64)| {
            (lo <= hi)
                .then_some(())
                .ok_or_else(|| format!("{what}: {lo} is above {hi}"))
        };
        let numbered = |what: &str, numbers: &[u64]| {
            (numbers.iter().all(|n| *n >= 1))
                .then_some(())
                .ok_or_else(|| format!("{what}: numbers count from 1"))
        };
        if let Some(latency) = &self.latency {
            range("latency slots", latency.slots)?;
            if let Some(spike) = &latency.spike {
                range("latency spike slots", spike.slots)?;
                if !(0.0..=1.0).contains(&spike.p) {
                    return Err(format!("latency spike: p {} is outside [0, 1]", spike.p));
                }
            }
        }
        if let Some(congestion) = &self.congestion {
            range("congestion floor", congestion.floor_micro_lamports)?;
            if congestion.from_step >= congestion.to_step || congestion.to_step > steps {
                return Err(format!(
                    "congestion: steps {}..{} are empty or past the session's {steps}",
                    congestion.from_step, congestion.to_step
                ));
            }
        }
        if let Some(fault) = &self.blockhash_expiry {
            numbered("blockhash_expiry", &fault.sends)?;
        }
        if let Some(fault) = &self.drop {
            numbered("drop", &fault.sends)?;
        }
        if let Some(fault) = &self.lost_confirmation {
            numbered("lost_confirmation", &fault.landings)?;
            if fault.hide_steps == 0 {
                return Err("lost_confirmation: hide_steps is 0".into());
            }
        }
        if let Some(fault) = &self.rpc_unavailable {
            numbered("rpc_unavailable", &fault.reads)?;
            if fault.count == 0 {
                return Err("rpc_unavailable: count is 0".into());
            }
        }
        Ok(())
    }
}

/// The spec resolved against the seed.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct FaultPlan {
    seed: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    latency: Option<Latency>,
    /// Window and the floor drawn for it.
    #[serde(skip_serializing_if = "Option::is_none")]
    congestion: Option<(u64, u64, u64)>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    blockhash_expiry: Vec<u64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    drop: Vec<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    lost_confirmation: Option<LostConfirmation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    rpc_unavailable: Option<RpcUnavailable>,
}

/// What the plan does to a send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendFault {
    None,
    Expire,
    Drop,
}

const TAG_FLOOR: u64 = 1;
const TAG_SPIKE: u64 = 2;
const TAG_DELAY: u64 = 3;
const TAG_BACKGROUND_FEE: u64 = 4;
const TAG_FLOOR_FEE: u64 = 5;

impl FaultPlan {
    pub fn new(faults: &Faults) -> Self {
        let congestion = faults.congestion.as_ref().map(|c| {
            let floor = pick(draw(faults.seed, TAG_FLOOR, 0), c.floor_micro_lamports);
            (c.from_step, c.to_step, floor)
        });
        Self {
            seed: faults.seed,
            latency: faults.latency.clone(),
            congestion,
            blockhash_expiry: faults
                .blockhash_expiry
                .as_ref()
                .map_or_else(Vec::new, |f| f.sends.clone()),
            drop: faults
                .drop
                .as_ref()
                .map_or_else(Vec::new, |f| f.sends.clone()),
            lost_confirmation: faults.lost_confirmation.clone(),
            rpc_unavailable: faults.rpc_unavailable.clone(),
        }
    }

    /// What happens to the bot's `n`-th send. A refused send never goes on to
    /// be dropped.
    pub fn send(&self, n: u64) -> SendFault {
        if self.blockhash_expiry.contains(&n) {
            SendFault::Expire
        } else if self.drop.contains(&n) {
            SendFault::Drop
        } else {
            SendFault::None
        }
    }

    /// The `n`-th send's delay in slots.
    pub fn delay_slots(&self, n: u64) -> u64 {
        let Some(latency) = &self.latency else {
            return 0;
        };
        match &latency.spike {
            Some(spike) if draw(self.seed, TAG_SPIKE, n) < spike.p => {
                pick(draw(self.seed, TAG_DELAY, n), spike.slots)
            }
            _ => pick(draw(self.seed, TAG_DELAY, n), latency.slots),
        }
    }

    /// The compute-unit price, in micro-lamports, a transaction needs for the
    /// block of `step`; 0 outside the congestion window.
    pub fn floor(&self, step: u64) -> u64 {
        match self.congestion {
            Some((from, to, floor)) if (from..to).contains(&step) => floor,
            _ => 0,
        }
    }

    /// Whether the plan has a congestion window.
    pub fn congested(&self) -> bool {
        self.congestion.is_some()
    }

    /// Steps the `n`-th landed transaction's status stays hidden; 0 if it
    /// doesn't.
    pub fn hide_steps(&self, landing: u64) -> u64 {
        self.lost_confirmation
            .as_ref()
            .filter(|f| f.landings.contains(&landing))
            .map_or(0, |f| f.hide_steps)
    }

    /// Whether the bot's `n`-th read fails with a 429.
    pub fn unavailable(&self, read: u64) -> bool {
        self.rpc_unavailable.as_ref().is_some_and(|f| {
            f.reads
                .iter()
                .any(|from| (*from..*from + f.count).contains(&read))
        })
    }

    /// What `getRecentPrioritizationFees` says of `slot`: the ordinary
    /// background (0 to 2,000), and in a window the larger of that and a draw
    /// just over the floor, so a small floor hides in the background instead
    /// of showing as a tight band. `step` is the step whose block holds the
    /// slot, if the slot is in the session.
    pub fn recent_fee(&self, slot: u64, step: Option<u64>) -> u64 {
        let background = (draw(self.seed, TAG_BACKGROUND_FEE, slot) * 2000.0).round() as u64;
        match step.map(|step| self.floor(step)) {
            Some(floor) if floor > 0 => {
                let over = floor as f64 * (1.0 + 0.5 * draw(self.seed, TAG_FLOOR_FEE, slot));
                background.max(over.round() as u64)
            }
            _ => background,
        }
    }
}

/// A uniform draw in `[0, 1)` that is a pure function of the seed, a tag for
/// what it decides, and the number it decides it for.
fn draw(seed: u64, tag: u64, n: u64) -> f64 {
    let mut rng = SplitMix64(seed);
    rng.0 ^= tag;
    rng.next_u64();
    rng.0 ^= n;
    rng.next_f64()
}

fn pick(draw: f64, (lo, hi): (u64, u64)) -> u64 {
    lo + ((draw * (hi - lo + 1) as f64) as u64).min(hi - lo)
}

/// What the session says of the faults at its end.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct FaultsReport {
    pub plan: FaultPlan,
    /// Sends, landings and reads the bot made, as the plan counted them.
    pub sends: u64,
    pub landings: u64,
    pub reads: u64,
    /// Every fault that fired, in order.
    pub fired: Vec<Fired>,
    /// Sends still held when the session ended: they never landed.
    pub held_at_end: usize,
}

/// One fault that fired.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Fired {
    /// `latency`, `congestion`, `blockhash_expiry`, `drop`, `lost_confirmation`
    /// or `rpc_unavailable`.
    pub fault: &'static str,
    /// The bot's turn it fired in.
    pub step: u64,
    /// The send, landing or read number it fired on.
    pub n: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    /// For a held send, the step it landed at or, for a hidden status, the
    /// step it reads again.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub until_step: Option<u64>,
}

/// A send the guard accepted and is holding.
#[derive(Debug, Clone)]
pub struct Held {
    pub call: Value,
    /// Its send number.
    pub n: u64,
    unit_price: u64,
    /// The first step it may land at.
    due: u64,
    /// Where it sits in [`FaultState::fired`].
    fired: usize,
}

/// What the guard does with a send.
#[derive(Debug, PartialEq)]
pub enum SendAction {
    /// Forward it now, as without faults.
    Forward,
    /// Refuse it as an expired blockhash.
    Expired,
    /// Answer with its signature and forward nothing.
    Dropped(String),
    /// Answer with its signature; it lands later.
    Held(String),
}

/// A plan and what the bot has done under it so far.
#[derive(Debug)]
pub struct FaultState {
    plan: FaultPlan,
    base_slot: u64,
    slots_per_step: u64,
    sends: u64,
    landings: u64,
    reads: u64,
    held: Vec<Held>,
    /// Signatures whose status reads null until the step.
    hidden: Vec<(String, u64)>,
    fired: Vec<Fired>,
}

impl FaultState {
    pub fn new(plan: FaultPlan, base_slot: u64, slots_per_step: u64) -> Self {
        Self {
            plan,
            base_slot,
            slots_per_step,
            sends: 0,
            landings: 0,
            reads: 0,
            held: Vec::new(),
            hidden: Vec::new(),
            fired: Vec::new(),
        }
    }

    pub fn plan(&self) -> &FaultPlan {
        &self.plan
    }

    /// The bot sent `call` in `step`.
    pub fn on_send(&mut self, call: &Value, step: u64) -> SendAction {
        self.sends += 1;
        let n = self.sends;
        let fault = |fault, signature: Option<String>, until_step| Fired {
            fault,
            step,
            n,
            signature,
            until_step,
        };
        match self.plan.send(n) {
            SendFault::Expire => {
                self.fired.push(fault("blockhash_expiry", None, None));
                return SendAction::Expired;
            }
            SendFault::Drop => {
                // A transaction the guard can't read isn't dropped: the
                // surfnet refuses it as it would any other.
                let Some(tx) = WireTransaction::parse(call) else {
                    return SendAction::Forward;
                };
                self.fired
                    .push(fault("drop", Some(tx.signature.clone()), None));
                return SendAction::Dropped(tx.signature);
            }
            SendFault::None => {}
        }
        let delay = self.plan.delay_slots(n);
        // The block that would take it is the next one.
        let floor = self.plan.floor(step + 1);
        let Some(tx) = WireTransaction::parse(call) else {
            return SendAction::Forward;
        };
        let congested = floor > 0 && tx.unit_price < floor;
        if delay == 0 && !congested {
            return SendAction::Forward;
        }
        let due = step + delay.div_ceil(self.slots_per_step).max(1);
        let fault_name = if congested { "congestion" } else { "latency" };
        self.fired
            .push(fault(fault_name, Some(tx.signature.clone()), None));
        self.held.push(Held {
            call: call.clone(),
            n,
            unit_price: tx.unit_price,
            due,
            fired: self.fired.len() - 1,
        });
        SendAction::Held(tx.signature)
    }

    /// The held sends that land in the block of `step`, in the order they were
    /// sent: the ones whose delay has passed and whose price clears the floor.
    pub fn take_due(&mut self, step: u64) -> Vec<Held> {
        let floor = self.plan.floor(step);
        let (due, held): (Vec<Held>, Vec<Held>) = std::mem::take(&mut self.held)
            .into_iter()
            .partition(|h| h.due <= step && h.unit_price >= floor);
        self.held = held;
        for h in &due {
            self.fired[h.fired].until_step = Some(step);
        }
        due
    }

    /// A transaction the chain ran, in `step`: its status may read null for a
    /// while.
    pub fn on_landing(&mut self, signature: &str, step: u64) {
        self.landings += 1;
        let hide = self.plan.hide_steps(self.landings);
        if hide > 0 {
            self.hidden.push((signature.to_string(), step + hide));
            self.fired.push(Fired {
                fault: "lost_confirmation",
                step,
                n: self.landings,
                signature: Some(signature.to_string()),
                until_step: Some(step + hide),
            });
        }
    }

    /// Whether the bot's read in `step` fails with a 429.
    pub fn on_read(&mut self, method: &str, step: u64) -> bool {
        self.reads += 1;
        let failed = self.plan.unavailable(self.reads);
        if failed {
            self.fired.push(Fired {
                fault: "rpc_unavailable",
                step,
                n: self.reads,
                signature: Some(method.to_string()),
                until_step: None,
            });
        }
        failed
    }

    /// Whether `signature` reads as unknown in `step`.
    pub fn hides(&self, signature: &str, step: u64) -> bool {
        self.hidden
            .iter()
            .any(|(hidden, until)| hidden == signature && step < *until)
    }

    /// `getRecentPrioritizationFees` in `step`: the last 150 slots, as a
    /// cluster reports them.
    pub fn recent_fees(&self, step: u64) -> Value {
        let now = self.base_slot + (step + 1) * self.slots_per_step;
        let slots = (now.saturating_sub(149)..=now).map(|slot| {
            let step = slot
                .checked_sub(self.base_slot + 1)
                .map(|since| since / self.slots_per_step);
            json!({ "slot": slot, "prioritizationFee": self.plan.recent_fee(slot, step) })
        });
        Value::Array(slots.collect())
    }

    pub fn report(&self) -> FaultsReport {
        FaultsReport {
            plan: self.plan.clone(),
            sends: self.sends,
            landings: self.landings,
            reads: self.reads,
            fired: self.fired.clone(),
            held_at_end: self.held.len(),
        }
    }
}

/// What the guard reads of a `sendTransaction`: its signature, which is the
/// answer a send gets, and its compute-unit price.
#[derive(Debug, PartialEq)]
pub struct WireTransaction {
    pub signature: String,
    /// Micro-lamports per compute unit; 0 without a price instruction.
    pub unit_price: u64,
}

const COMPUTE_BUDGET: &str = "ComputeBudget111111111111111111111111111111";
const SET_COMPUTE_UNIT_PRICE: u8 = 3;

impl WireTransaction {
    /// The transaction in a `sendTransaction` call's params, if they hold one
    /// the guard can read.
    pub fn parse(call: &Value) -> Option<Self> {
        use base64::Engine;
        let wire = call["params"][0].as_str()?;
        let bytes = match call["params"][1]["encoding"].as_str() {
            Some("base64") => base64::engine::general_purpose::STANDARD
                .decode(wire)
                .ok()?,
            _ => bs58::decode(wire).into_vec().ok()?,
        };
        let mut at = 0;
        let signatures = shortvec(&bytes, &mut at)?;
        if signatures == 0 {
            return None;
        }
        let signature = bs58::encode(bytes.get(at..at + 64)?).into_string();
        at += signatures * 64;
        // A v0 message starts with a version byte.
        if bytes.get(at)? & 0x80 != 0 {
            at += 1;
        }
        // Header, account keys, recent blockhash.
        at += 3;
        let keys = shortvec(&bytes, &mut at)?;
        let keys_at = at;
        at += keys * 32 + 32;
        let compute_budget = bs58::decode(COMPUTE_BUDGET).into_vec().ok()?;
        let mut unit_price = 0;
        for _ in 0..shortvec(&bytes, &mut at)? {
            let program = *bytes.get(at)? as usize;
            at += 1;
            let accounts = shortvec(&bytes, &mut at)?;
            at += accounts;
            let len = shortvec(&bytes, &mut at)?;
            let data = bytes.get(at..at + len)?;
            at += len;
            let program_at = keys_at + program * 32;
            if program < keys
                && bytes.get(program_at..program_at + 32)? == compute_budget.as_slice()
                && data.first() == Some(&SET_COMPUTE_UNIT_PRICE)
            {
                unit_price = u64::from_le_bytes(data.get(1..9)?.try_into().ok()?);
            }
        }
        Some(Self {
            signature,
            unit_price,
        })
    }
}

/// A compact-u16 length, as Solana's wire format writes them.
fn shortvec(bytes: &[u8], at: &mut usize) -> Option<usize> {
    let mut value = 0usize;
    for shift in [0, 7, 14] {
        let byte = *bytes.get(*at)?;
        *at += 1;
        value |= ((byte & 0x7f) as usize) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
    }
    None
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn spec(seed: u64) -> Faults {
        Faults {
            seed,
            latency: Some(Latency {
                slots: (0, 6),
                spike: Some(Spike {
                    p: 0.25,
                    slots: (10, 20),
                }),
            }),
            congestion: Some(Congestion {
                from_step: 4,
                to_step: 9,
                floor_micro_lamports: (1_000, 90_000),
            }),
            blockhash_expiry: Some(SendNumbers { sends: vec![2] }),
            drop: Some(SendNumbers { sends: vec![3, 5] }),
            lost_confirmation: Some(LostConfirmation {
                landings: vec![1],
                hide_steps: 3,
            }),
            rpc_unavailable: Some(RpcUnavailable {
                reads: vec![4],
                count: 2,
            }),
        }
    }

    /// A legacy transaction: one signature, a compute-budget price
    /// instruction and a memo-like one.
    pub(crate) fn transaction(price: Option<u64>, seed: u8) -> Value {
        let budget = bs58::decode(COMPUTE_BUDGET).into_vec().unwrap();
        let mut message = vec![1, 0, 1];
        message.push(3);
        message.extend([seed; 32]);
        message.extend(&budget);
        message.extend([9u8; 32]);
        message.extend([7u8; 32]);
        let mut instructions: Vec<Vec<u8>> = Vec::new();
        if let Some(price) = price {
            let mut data = vec![SET_COMPUTE_UNIT_PRICE];
            data.extend(price.to_le_bytes());
            instructions.push([vec![1, 0, data.len() as u8], data].concat());
        }
        instructions.push(vec![2, 1, 0, 2, 1, 2]);
        message.push(instructions.len() as u8);
        instructions.iter().for_each(|ix| message.extend(ix));
        let mut wire = vec![1];
        wire.extend([seed.wrapping_add(100); 64]);
        wire.extend(message);
        json!({ "jsonrpc": "2.0", "id": seed, "method": "sendTransaction",
                "params": [bs58::encode(wire).into_string()] })
    }

    #[test]
    fn the_plan_is_a_function_of_the_seed() {
        let a = FaultPlan::new(&spec(7));
        assert_eq!(a, FaultPlan::new(&spec(7)));
        let delays = |plan: &FaultPlan| (1..=40).map(|n| plan.delay_slots(n)).collect::<Vec<_>>();
        let other = FaultPlan::new(&spec(8));
        assert_eq!(delays(&a), delays(&FaultPlan::new(&spec(7))));
        assert_ne!(delays(&a), delays(&other));
        let floors: std::collections::BTreeSet<u64> = (0..50)
            .map(|seed| FaultPlan::new(&spec(seed)).floor(5))
            .collect();
        assert!(floors.len() > 20, "the floor varies with the seed");
        assert!(floors.iter().all(|f| (1_000..=90_000).contains(f)));
        assert_ne!(a.recent_fee(100, Some(5)), other.recent_fee(100, Some(5)));
    }

    #[test]
    fn delays_stay_in_their_ranges_and_spike() {
        let plan = FaultPlan::new(&spec(3));
        let delays: Vec<u64> = (1..=400).map(|n| plan.delay_slots(n)).collect();
        assert!(delays.iter().all(|d| *d <= 6 || (10..=20).contains(d)));
        let spikes = delays.iter().filter(|d| **d >= 10).count();
        assert!((60..=140).contains(&spikes), "{spikes} spikes in 400");
        assert!(delays.contains(&0) && delays.contains(&6));
    }

    #[test]
    fn each_fault_fires_at_its_number_and_only_there() {
        let plan = FaultPlan::new(&spec(1));
        let sends: Vec<SendFault> = (1..=6).map(|n| plan.send(n)).collect();
        assert_eq!(
            sends,
            [
                SendFault::None,
                SendFault::Expire,
                SendFault::Drop,
                SendFault::None,
                SendFault::Drop,
                SendFault::None
            ]
        );
        let hidden: Vec<u64> = (1..=4).map(|n| plan.hide_steps(n)).collect();
        assert_eq!(hidden, [3, 0, 0, 0]);
        let failing: Vec<u64> = (1..=8).filter(|n| plan.unavailable(*n)).collect();
        assert_eq!(failing, [4, 5], "429s stop after the count");
        let floors: Vec<bool> = (0..12).map(|s| plan.floor(s) > 0).collect();
        assert_eq!(
            floors,
            [
                false, false, false, false, true, true, true, true, true, false, false, false
            ]
        );
    }

    #[test]
    fn a_spec_that_makes_no_sense_is_refused() {
        assert_eq!(spec(1).validate(20), Ok(()));
        assert!(spec(1).validate(8).is_err(), "window past the session");
        let mut bad = spec(1);
        bad.drop = Some(SendNumbers { sends: vec![0] });
        assert!(bad.validate(20).is_err());
        let mut bad = spec(1);
        bad.latency.as_mut().unwrap().slots = (5, 2);
        assert!(bad.validate(20).is_err());
        let mut bad = spec(1);
        bad.rpc_unavailable.as_mut().unwrap().count = 0;
        assert!(bad.validate(20).is_err());
        let json = serde_json::to_string(&spec(1)).unwrap();
        assert_eq!(serde_json::from_str::<Faults>(&json).unwrap(), spec(1));
        assert!(serde_json::from_str::<Faults>(r#"{"seed":1,"nope":2}"#).is_err());
    }

    #[test]
    fn a_wire_transaction_gives_its_signature_and_price() {
        let tx = WireTransaction::parse(&transaction(Some(12_345), 1)).unwrap();
        assert_eq!(tx.unit_price, 12_345);
        assert_eq!(tx.signature, bs58::encode([101u8; 64]).into_string());
        assert_eq!(
            WireTransaction::parse(&transaction(None, 2))
                .unwrap()
                .unit_price,
            0
        );
        assert!(WireTransaction::parse(&json!({ "params": ["not a transaction"] })).is_none());
        assert!(WireTransaction::parse(&json!({ "params": [] })).is_none());
    }

    fn state(spec: &Faults) -> FaultState {
        FaultState::new(FaultPlan::new(spec), 1_000, 2)
    }

    #[test]
    fn sends_are_refused_dropped_or_held_by_number() {
        let mut spec = spec(1);
        spec.latency = None;
        spec.congestion = None;
        let mut faults = state(&spec);
        let sig = |seed: u8| bs58::encode([seed.wrapping_add(100); 64]).into_string();
        assert_eq!(
            faults.on_send(&transaction(None, 1), 1),
            SendAction::Forward
        );
        assert_eq!(
            faults.on_send(&transaction(None, 2), 1),
            SendAction::Expired
        );
        assert_eq!(
            faults.on_send(&transaction(None, 3), 2),
            SendAction::Dropped(sig(3))
        );
        assert_eq!(
            faults.on_send(&transaction(None, 4), 2),
            SendAction::Forward
        );
        let report = faults.report();
        assert_eq!(report.sends, 4);
        let fired: Vec<_> = report
            .fired
            .iter()
            .map(|f| (f.fault, f.n, f.step))
            .collect();
        assert_eq!(fired, [("blockhash_expiry", 2, 1), ("drop", 3, 2)]);
    }

    #[test]
    fn a_held_send_lands_at_its_step_in_send_order() {
        let mut spec = spec(1);
        spec.congestion = None;
        spec.blockhash_expiry = None;
        spec.drop = None;
        spec.latency = Some(Latency {
            slots: (5, 5),
            spike: None,
        });
        let mut faults = state(&spec);
        // 5 slots at 2 per step: three steps on.
        for seed in 1..=2 {
            assert!(matches!(
                faults.on_send(&transaction(None, seed), 4),
                SendAction::Held(_)
            ));
        }
        assert!(faults.take_due(6).is_empty());
        let due = faults.take_due(7);
        let ids: Vec<_> = due.iter().map(|h| h.call["id"].clone()).collect();
        assert_eq!(ids, [json!(1), json!(2)]);
        assert_eq!(due.iter().map(|h| h.n).collect::<Vec<_>>(), [1, 2]);
        assert!(faults.take_due(8).is_empty(), "each lands once");
        assert!(
            faults
                .report()
                .fired
                .iter()
                .all(|f| f.until_step == Some(7))
        );
        assert_eq!(faults.report().held_at_end, 0);
    }

    #[test]
    fn congestion_holds_cheap_sends_until_the_window_ends() {
        let mut spec = spec(1);
        spec.latency = None;
        spec.blockhash_expiry = None;
        spec.drop = None;
        spec.congestion = Some(Congestion {
            from_step: 4,
            to_step: 9,
            floor_micro_lamports: (5_000, 5_000),
        });
        let mut faults = state(&spec);
        // Sent in turn 3, bound for the block of step 4, inside the window.
        assert!(matches!(
            faults.on_send(&transaction(Some(4_999), 1), 3),
            SendAction::Held(_)
        ));
        assert_eq!(
            faults.on_send(&transaction(Some(5_000), 2), 3),
            SendAction::Forward
        );
        assert!(matches!(
            faults.on_send(&transaction(None, 3), 5),
            SendAction::Held(_)
        ));
        for step in 4..9 {
            assert!(faults.take_due(step).is_empty(), "step {step} is congested");
        }
        let due = faults.take_due(9);
        assert_eq!(due.len(), 2);
        // Sent in the last turn of the window, the next block is clear.
        assert_eq!(
            faults.on_send(&transaction(None, 4), 8),
            SendAction::Forward
        );
        // The floor shows in the fees the bot reads, and nowhere else.
        let fees = |step| faults.recent_fees(step);
        let at = |fees: &Value, slot: u64| {
            fees.as_array()
                .unwrap()
                .iter()
                .find(|f| f["slot"] == slot)
                .unwrap()["prioritizationFee"]
                .as_u64()
                .unwrap()
        };
        let inside = fees(6);
        assert_eq!(inside.as_array().unwrap().len(), 150);
        assert!(at(&inside, 1_000 + 2 * 6) >= 5_000);
        assert!(at(&inside, 1_000 + 2 * 2) <= 2_000);
    }

    #[test]
    fn a_lost_confirmation_hides_the_status_for_its_steps() {
        let mut faults = state(&spec(1));
        faults.on_landing("first", 2);
        faults.on_landing("second", 2);
        assert!(faults.hides("first", 2) && faults.hides("first", 4));
        assert!(!faults.hides("first", 5), "back after the third step");
        assert!(!faults.hides("second", 2));
        assert!(!faults.hides("other", 2));
        assert_eq!(faults.report().landings, 2);
    }

    #[test]
    fn reads_fail_from_their_number_for_the_count() {
        let mut faults = state(&spec(1));
        let failed: Vec<bool> = (0..7).map(|_| faults.on_read("getSlot", 1)).collect();
        assert_eq!(failed, [false, false, false, true, true, false, false]);
    }
}
