//! Leaders: a synthetic, seeded validator set and its schedule.
//!
//! Each validator has a stake, runs Jito's client or not, and has a skip
//! rate; the schedule gives each leader [`super::calibration::Block::slots_per_leader`]
//! consecutive slots, picked by stake. The identities are synthetic, drawn
//! from the seed: no real validator is named. A leader that skips its slot
//! produces an empty block; a Jito leader is the only one that lands
//! bundles.

use serde::Serialize;
use solana_pubkey::Pubkey;

use super::{calibration::Calibration, draw, hash};

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Validator {
    pub identity: Pubkey,
    pub stake: u64,
    pub jito: bool,
    pub skip_rate: f64,
}

#[derive(Debug, Clone)]
pub struct Leaders {
    seed: u64,
    validators: Vec<Validator>,
    slots_per_leader: u64,
    total_stake: u64,
}

impl Leaders {
    pub fn new(seed: u64, count: u32, calibration: &Calibration) -> Self {
        let block = &calibration.block;
        let validators: Vec<Validator> = (0..count as u64)
            .map(|i| {
                let mut bytes = [0u8; 32];
                for (n, chunk) in bytes.chunks_mut(8).enumerate() {
                    chunk.copy_from_slice(
                        &hash(seed, "identity", &[i, n as u64], &[]).to_le_bytes(),
                    );
                }
                // Stake is heavy-tailed: a lognormal around 1M SOL.
                let z = super::normal(seed, "stake", &[i]);
                let stake = ((13.8 + 1.0 * z).exp()) as u64 + 1;
                // A validator's skip rate scatters around the network's.
                let skip_rate = (block.skip_rate
                    * (0.5 * super::normal(seed, "skip-rate", &[i])).exp())
                .min(0.5);
                Validator {
                    identity: Pubkey::new_from_array(bytes),
                    stake,
                    jito: draw(seed, "jito", &[i]) < block.jito_stake_share,
                    skip_rate,
                }
            })
            .collect();
        let total_stake = validators.iter().map(|v| v.stake).sum();
        Self {
            seed,
            validators,
            slots_per_leader: block.slots_per_leader,
            total_stake,
        }
    }

    pub fn validators(&self) -> &[Validator] {
        &self.validators
    }

    /// The leader of `slot`: its group of slots' draw, weighted by stake.
    pub fn leader(&self, slot: u64) -> &Validator {
        let group = slot / self.slots_per_leader;
        let target = (draw(self.seed, "leader", &[group]) * self.total_stake as f64) as u64;
        let mut acc = 0;
        for validator in &self.validators {
            acc += validator.stake;
            if target < acc {
                return validator;
            }
        }
        self.validators.last().expect("at least one validator")
    }

    /// Whether `slot`'s leader skips it.
    pub fn skipped(&self, slot: u64) -> bool {
        draw(self.seed, "skip", &[slot]) < self.leader(slot).skip_rate
    }

    /// `getLeaderSchedule` over the slots `from..to`, as offsets from
    /// `epoch_start`: each identity's slots.
    pub fn schedule(&self, epoch_start: u64, from: u64, to: u64) -> serde_json::Value {
        let mut schedule = serde_json::Map::new();
        for slot in from..to {
            let identity = self.leader(slot).identity.to_string();
            let entry = schedule
                .entry(identity)
                .or_insert_with(|| serde_json::Value::Array(Vec::new()));
            if let serde_json::Value::Array(slots) = entry {
                slots.push(serde_json::json!(slot.saturating_sub(epoch_start)));
            }
        }
        serde_json::Value::Object(schedule)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaders(seed: u64) -> Leaders {
        let calibration = Calibration::builtin("mainnet-fees.20261113.0000test").unwrap();
        Leaders::new(seed, 24, &calibration)
    }

    #[test]
    fn leaders_hold_four_slots_and_follow_the_seed() {
        let a = leaders(1);
        for group in 0..50u64 {
            let first = a.leader(group * 4).identity;
            assert!((1..4).all(|i| a.leader(group * 4 + i).identity == first));
        }
        let b = leaders(1);
        assert!((0..400).all(|s| a.leader(s) == b.leader(s)));
        let c = leaders(2);
        assert!((0..400).any(|s| a.leader(s).identity != c.leader(s).identity));
        let jito = a.validators().iter().filter(|v| v.jito).count();
        assert!(jito > 12, "{jito} of 24 run Jito");
        let skips = (0..20_000).filter(|s| a.skipped(*s)).count();
        assert!((50..=1_500).contains(&skips), "{skips} skips in 20k slots");
    }

    #[test]
    fn the_schedule_names_every_slot_once() {
        let a = leaders(3);
        let schedule = a.schedule(1_000, 1_200, 1_280);
        let total: usize = schedule
            .as_object()
            .unwrap()
            .values()
            .map(|v| v.as_array().unwrap().len())
            .sum();
        assert_eq!(total, 80);
    }
}
