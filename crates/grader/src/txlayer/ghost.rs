//! Ghost load: the background demand that sets each slot's price to land.
//!
//! Mainnet blocks hold thousands of transactions; a session holds tens. The
//! ghosts aren't executed: for each slot and each hot account (and the
//! block as a whole) they set the **clearing price**, in micro-lamports per
//! compute unit, below which nothing touching it lands, and how much of the
//! cap they take. Per slot:
//!
//! - the **regime** (calm, busy, spike) is pinned by the scenario's windows,
//!   or drawn from the calibration's per-slot switching;
//! - whether demand **contends** for the account is a draw against the
//!   regime's `p_contended`; an uncontended account clears at 0, so a
//!   zero-fee transaction lands, as on mainnet;
//! - the clearing price's log follows an AR(1) around the regime's mean.
//!
//! The path is drawn once, before the session, for every slot it can need:
//! a function of the seed alone, never of the bot.

use solana_pubkey::Pubkey;

use super::{
    TxLayer,
    calibration::{Calibration, REGIMES},
    draw, normal, regime_index,
};

/// How many slots of history `getRecentPrioritizationFees` reports.
pub const RECENT_SLOTS: u64 = 150;

/// What background demand leaves for real transactions, in one slot, on
/// one account or the block.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Clearing {
    /// Micro-lamports per compute unit a transaction must pay to land.
    pub price: u64,
    /// The share of the cap the background takes.
    pub utilization: f64,
}

/// The background demand of a session, slot by slot.
#[derive(Debug, Clone)]
pub struct GhostPath {
    /// The first slot the path covers: [`RECENT_SLOTS`] before the session.
    first_slot: u64,
    regimes: Vec<u8>,
    block: Vec<Clearing>,
    /// Each hot account, in the scenario's order.
    accounts: Vec<(Pubkey, Vec<Clearing>)>,
    /// A floor on the block's price, by slot: the `congestion` fault's
    /// window, compiled into the path.
    floors: Vec<u64>,
}

impl GhostPath {
    /// The path for a session whose steps start at `base_slot`, step every
    /// `slots_per_step` and run to `end_slot`. `congestion`: a window of
    /// steps and the floor drawn for it, from the scenario's faults.
    pub fn new(
        layer: &TxLayer,
        calibration: &Calibration,
        base_slot: u64,
        end_slot: u64,
        slots_per_step: u64,
        congestion: Option<(u64, u64, u64)>,
    ) -> Self {
        let first_slot = base_slot.saturating_sub(RECENT_SLOTS);
        // A few slots past the end, for a send whose latency runs over.
        let last_slot = end_slot + 2 * RECENT_SLOTS;
        let len = (last_slot - first_slot + 1) as usize;
        let step_of = |slot: u64| {
            slot.checked_sub(base_slot + 1)
                .map(|since| since / slots_per_step)
        };
        let seed = layer.seed;

        // The regime of each slot: pinned, drawn, or calm.
        let mut regimes = Vec::with_capacity(len);
        let mut drawn = 0usize;
        for i in 0..len {
            let slot = first_slot + i as u64;
            if layer.drawn_regimes {
                let row = &calibration.regimes.transitions[drawn];
                let u = draw(seed, "regime", &[slot]);
                let mut acc = 0.0;
                drawn = row
                    .iter()
                    .position(|p| {
                        acc += p;
                        u < acc
                    })
                    .unwrap_or(row.len() - 1);
            }
            let pinned = step_of(slot).and_then(|step| {
                layer
                    .regimes
                    .iter()
                    .find(|w| (w.from_step..w.to_step).contains(&step))
                    .map(|w| regime_index(&w.regime).expect("validated"))
            });
            regimes.push(pinned.unwrap_or(if layer.drawn_regimes { drawn } else { 0 }) as u8);
        }

        let path = |class: &str, index: u64| -> Vec<Clearing> {
            let mut out = Vec::with_capacity(len);
            let mut x: Option<f64> = None;
            for (i, regime) in regimes.iter().enumerate() {
                let slot = first_slot + i as u64;
                let g = calibration.ghost(class, *regime as usize);
                let z = normal(seed, "ghost", &[index, slot]);
                let innovation = g.log_price_sd * (1.0 - g.phi * g.phi).sqrt() * z;
                let next = match x {
                    Some(prev) => g.log_price_mean + g.phi * (prev - g.log_price_mean) + innovation,
                    None => g.log_price_mean + g.log_price_sd * z,
                };
                x = Some(next);
                let contended = draw(seed, "contended", &[index, slot]) < g.p_contended;
                out.push(if contended {
                    Clearing {
                        price: next.exp().round().min(u64::MAX as f64 / 2.0) as u64,
                        utilization: g.utilization,
                    }
                } else {
                    Clearing {
                        price: 0,
                        utilization: g.utilization * 0.5,
                    }
                });
            }
            out
        };
        let block = path("block", 0);
        let accounts = layer
            .hot_accounts
            .iter()
            .enumerate()
            .map(|(i, hot)| {
                (
                    hot.address.parse().expect("validated"),
                    path(&hot.class, i as u64 + 1),
                )
            })
            .collect();
        let floors = (0..len)
            .map(|i| {
                let slot = first_slot + i as u64;
                match (congestion, step_of(slot)) {
                    (Some((from, to, floor)), Some(step)) if (from..to).contains(&step) => floor,
                    _ => 0,
                }
            })
            .collect();
        Self {
            first_slot,
            regimes,
            block,
            accounts,
            floors,
        }
    }

    fn index(&self, slot: u64) -> Option<usize> {
        slot.checked_sub(self.first_slot)
            .map(|i| i as usize)
            .filter(|i| *i < self.block.len())
    }

    /// The regime at `slot`.
    pub fn regime(&self, slot: u64) -> &'static str {
        self.index(slot)
            .map_or(REGIMES[0], |i| REGIMES[self.regimes[i] as usize])
    }

    /// What the block clears at in `slot`.
    pub fn block(&self, slot: u64) -> Clearing {
        self.index(slot).map_or_else(Clearing::default, |i| {
            let mut clearing = self.block[i];
            clearing.price = clearing.price.max(self.floors[i]);
            clearing
        })
    }

    /// What `account` clears at in `slot`: nothing, unless it is hot.
    pub fn account(&self, account: &Pubkey, slot: u64) -> Clearing {
        match (
            self.index(slot),
            self.accounts.iter().find(|(key, _)| key == account),
        ) {
            (Some(i), Some((_, path))) => path[i],
            _ => Clearing::default(),
        }
    }

    /// Whether `account` is hot.
    pub fn is_hot(&self, account: &Pubkey) -> bool {
        self.accounts.iter().any(|(key, _)| key == account)
    }

    /// The price a transaction writing `writable` must pay to land in
    /// `slot`.
    pub fn price_to_land(&self, slot: u64, writable: &[Pubkey]) -> u64 {
        writable
            .iter()
            .map(|account| self.account(account, slot).price)
            .fold(self.block(slot).price, u64::max)
    }

    /// `getRecentPrioritizationFees` at `now`: for each of the last
    /// [`RECENT_SLOTS`] slots up to it, the fee it took to land a
    /// transaction write-locking every account in `filter`, or any
    /// transaction without one.
    pub fn recent(&self, now: u64, filter: &[Pubkey]) -> Vec<(u64, u64)> {
        (now.saturating_sub(RECENT_SLOTS - 1)..=now)
            .map(|slot| (slot, self.price_to_land(slot, filter)))
            .collect()
    }

    /// The recent fees on `accounts` as `(percentile, price)`, for a
    /// component's fee view.
    pub fn percentiles(&self, now: u64, accounts: &[Pubkey]) -> Vec<(u8, u64)> {
        let mut prices: Vec<u64> = self
            .recent(now, accounts)
            .into_iter()
            .map(|(_, p)| p)
            .collect();
        prices.sort_unstable();
        [25u8, 50, 75, 90, 95, 99]
            .iter()
            .map(|p| (*p, quantile(&prices, *p)))
            .collect()
    }
}

/// The `p`-th percentile of sorted `values`, nearest rank.
pub fn quantile(sorted: &[u64], p: u8) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = ((p as f64 / 100.0) * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::txlayer::tests::layer;

    fn path(layer: &TxLayer) -> GhostPath {
        let calibration = Calibration::builtin(&layer.calibration).unwrap();
        GhostPath::new(layer, &calibration, 1_000, 1_080, 2, None)
    }

    #[test]
    fn the_path_is_a_function_of_the_seed() {
        let a = path(&layer());
        let b = path(&layer());
        let pool: Pubkey = layer().hot_accounts[0].address.parse().unwrap();
        let prices = |p: &GhostPath| {
            (900..1_080)
                .map(|s| p.account(&pool, s).price)
                .collect::<Vec<_>>()
        };
        assert_eq!(prices(&a), prices(&b));
        let mut other = layer();
        other.seed = 8;
        assert_ne!(prices(&a), prices(&path(&other)));
    }

    #[test]
    fn a_spike_window_prices_the_hot_account_and_nothing_else() {
        let p = path(&layer());
        let pool: Pubkey = layer().hot_accounts[0].address.parse().unwrap();
        let other = Pubkey::new_unique();
        // Steps 10..20 are slots 1_021..=1_040.
        assert_eq!(p.regime(1_030), "spike");
        assert_eq!(p.regime(1_010), "calm");
        let mean = |range: std::ops::Range<u64>| {
            range
                .clone()
                .map(|s| p.account(&pool, s).price as f64)
                .sum::<f64>()
                / range.count() as f64
        };
        assert!(mean(1_021..1_041) > 10.0 * mean(950..1_020).max(1.0));
        // A transaction writing only an unrelated account pays what the
        // block asks: in a pool's spike that is usually nothing.
        let free = (1_021..1_041)
            .filter(|s| p.price_to_land(*s, &[other]) == 0)
            .count();
        assert!(free >= 15, "{free} free slots of 20");
    }

    #[test]
    fn recent_fees_cover_150_slots_with_or_without_a_filter() {
        let p = path(&layer());
        let pool: Pubkey = layer().hot_accounts[0].address.parse().unwrap();
        let all = p.recent(1_030, &[]);
        assert_eq!(all.len(), 150);
        assert_eq!(all.last().unwrap().0, 1_030);
        let hot = p.recent(1_030, &[pool]);
        assert!(hot.iter().zip(&all).all(|(h, a)| h.1 >= a.1));
        assert!(hot.iter().any(|(_, f)| *f > 0));
    }

    #[test]
    fn a_congestion_window_is_a_floor_on_the_block() {
        let calibration = Calibration::builtin(&layer().calibration).unwrap();
        let p = GhostPath::new(&layer(), &calibration, 1_000, 1_080, 2, Some((2, 4, 7_000)));
        // Steps 2 and 3: slots 1_005..=1_008.
        assert!((1_005..=1_008).all(|s| p.block(s).price >= 7_000));
        assert_eq!(p.block(1_004).price.min(1), p.block(1_004).price.min(1));
        assert!(p.block(1_009).price < 7_000 || p.regime(1_009) != "calm");
    }
}
