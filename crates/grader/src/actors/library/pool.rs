//! A shared wallet pool: wallets that several of the library's actors trade
//! from, so a reader of the chain meets the same addresses in different
//! roles, and an actor that wants several wallets rotates among them.
//!
//! Without a pool every actor has wallets of its own, and the address alone
//! says who acts. With one, the scenario lists the roles that draw on it
//! (`pool.roles`) and each of their actors takes its `wallets` from the pool
//! at random, from the actor's own seed, so a session replays the same. Two
//! actors may hold the same wallet. What a wallet did is then the reader's
//! to untangle: its fills are the sum of its roles'.

use serde::{Deserialize, Serialize};

use super::Rng;

/// A library role that can trade from the pool.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PoolRole {
    Noise,
    Informed,
    Chasers,
    Decoys,
    StopHunters,
}

impl PoolRole {
    /// The section of `actors` it is, for messages.
    pub fn section(self) -> &'static str {
        match self {
            Self::Noise => "noise",
            Self::Informed => "informed",
            Self::Chasers => "chasers",
            Self::Decoys => "decoys",
            Self::StopHunters => "stop_hunters",
        }
    }
}

/// The scenario's pool: `wallets` of them, and the roles that draw on it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Pool {
    pub wallets: u32,
    pub roles: Vec<PoolRole>,
}

impl Pool {
    pub fn validate(&self) -> Result<(), String> {
        if self.wallets == 0 {
            return Err("wallets must be at least 1".into());
        }
        if self.roles.is_empty() {
            return Err("roles must name at least one role".into());
        }
        for (i, role) in self.roles.iter().enumerate() {
            if self.roles[..i].contains(role) {
                return Err(format!("roles names {} twice", role.section()));
            }
        }
        Ok(())
    }

    /// Whether `role`'s actors trade from the pool.
    pub fn serves(&self, role: PoolRole) -> bool {
        self.roles.contains(&role)
    }
}

/// `n` distinct indices of `0..of`, in a random order (a partial
/// Fisher-Yates shuffle). `n` is capped at `of`.
pub(super) fn draw(rng: &mut Rng, of: usize, n: usize) -> Vec<usize> {
    let mut all: Vec<usize> = (0..of).collect();
    let n = n.min(of);
    for i in 0..n {
        let j = i + rng.below(of - i);
        all.swap(i, j);
    }
    all.truncate(n);
    all
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_draw_is_distinct_replays_and_covers_the_pool() {
        let draws = |seed: u8| draw(&mut Rng::new([seed; 32]), 6, 3);
        assert_eq!(draws(1), draws(1));
        for seed in 0..20 {
            let mut drawn = draws(seed);
            drawn.sort_unstable();
            drawn.dedup();
            assert_eq!(drawn.len(), 3);
            assert!(drawn.iter().all(|i| *i < 6));
        }
        let mut seen = [false; 6];
        for seed in 0..64 {
            for i in draws(seed) {
                seen[i] = true;
            }
        }
        assert!(seen.iter().all(|s| *s), "every wallet is drawn sometime");
        assert_eq!(draw(&mut Rng::new([3; 32]), 2, 9).len(), 2);
        assert!(draw(&mut Rng::new([3; 32]), 0, 2).is_empty());
    }

    #[test]
    fn a_pool_needs_wallets_and_roles_it_names_once() {
        let pool = |wallets, roles: Vec<PoolRole>| Pool { wallets, roles };
        assert!(
            pool(6, vec![PoolRole::Noise, PoolRole::StopHunters])
                .validate()
                .is_ok()
        );
        assert!(pool(0, vec![PoolRole::Noise]).validate().is_err());
        assert!(pool(2, vec![]).validate().is_err());
        let twice = pool(2, vec![PoolRole::Noise, PoolRole::Noise]);
        assert!(twice.validate().unwrap_err().contains("twice"));
        assert!(pool(2, vec![PoolRole::Noise]).serves(PoolRole::Noise));
        assert!(!pool(2, vec![PoolRole::Noise]).serves(PoolRole::Decoys));
    }
}
