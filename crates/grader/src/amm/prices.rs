//! What an `amm` session's tokens are worth: SOL at its reference dollar
//! price (the scenario's start, moved by its path), USDC at a dollar, and
//! every other token at its fixed reference price in SOL
//! (`venues.amm.reference_sol`). The account is valued at them, the pool
//! searchers trade each pool back to them, and actors size their swaps in
//! SOL's worth by them. Never the pools' own prices, which a trade moves.

use std::collections::BTreeMap;

use solana_pubkey::Pubkey;

use crate::{scenario::Waypoint, venue::amm::Pool};

/// A session's reference prices.
#[derive(Debug, Clone)]
pub(crate) struct Prices {
    pub sol: Pubkey,
    pub usdc: Pubkey,
    /// SOL's dollar price at the start, and the path that moves it.
    pub start_usd: f64,
    pub path: Vec<Waypoint>,
    /// Every other token's price, in SOL a unit.
    pub others: BTreeMap<Pubkey, f64>,
}

impl Prices {
    /// SOL's dollar price `at` slots in.
    pub fn sol_usd(&self, at: u64) -> f64 {
        super::reference_at(self.start_usd, &self.path, at)
    }

    /// Whether it prices `mint`.
    pub fn knows(&self, mint: &Pubkey) -> bool {
        *mint == self.sol || *mint == self.usdc || self.others.contains_key(mint)
    }

    /// `units` of `mint` in SOL, SOL being worth `sol_usd`.
    pub fn in_sol(&self, mint: &Pubkey, units: f64, sol_usd: f64) -> f64 {
        assert!(sol_usd.is_finite() && sol_usd > 0.0);
        if *mint == self.sol {
            units
        } else if *mint == self.usdc {
            units / sol_usd
        } else {
            units * self.other(mint)
        }
    }

    /// `units` of `mint` in dollars, SOL being worth `sol_usd`.
    pub fn in_usd(&self, mint: &Pubkey, units: f64, sol_usd: f64) -> f64 {
        assert!(sol_usd.is_finite() && sol_usd > 0.0);
        if *mint == self.sol {
            units * sol_usd
        } else if *mint == self.usdc {
            units
        } else {
            units * self.other(mint) * sol_usd
        }
    }

    /// `pool`'s reference price, quote units a base unit: SOL's dollar price
    /// itself for SOL/USDC.
    pub fn pool_price(&self, pool: &Pool, sol_usd: f64) -> f64 {
        let price = self.in_usd(&pool.base.mint, 1.0, sol_usd)
            / self.in_usd(&pool.quote.mint, 1.0, sol_usd);
        assert!(price.is_finite() && price > 0.0, "a priced pool");
        price
    }

    /// Units of `pool`'s base worth `sol` SOL: `sol` itself where the base
    /// is SOL.
    pub fn base_units(&self, pool: &Pool, sol: f64, sol_usd: f64) -> f64 {
        sol / self.in_sol(&pool.base.mint, 1.0, sol_usd)
    }

    fn other(&self, mint: &Pubkey) -> f64 {
        *self
            .others
            .get(mint)
            .unwrap_or_else(|| panic!("checked: {mint} has a reference price"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sol_and_usdc_are_valued_exactly_as_before_and_others_at_their_reference() {
        let (sol, usdc, jto) = (
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        );
        let prices = Prices {
            sol,
            usdc,
            start_usd: 70.0,
            path: Vec::new(),
            others: BTreeMap::from([(jto, 0.003)]),
        };
        assert_eq!(prices.in_sol(&sol, 2.5, 70.0), 2.5);
        assert_eq!(prices.in_sol(&usdc, 140.0, 70.0), 140.0 / 70.0);
        assert!((prices.in_sol(&jto, 1_000.0, 70.0) - 3.0).abs() < 1e-12);
        assert!((prices.in_usd(&jto, 1_000.0, 70.0) - 210.0).abs() < 1e-9);
        assert!(prices.knows(&jto) && !prices.knows(&Pubkey::new_unique()));
        assert_eq!(prices.sol_usd(100), 70.0);
    }
}
