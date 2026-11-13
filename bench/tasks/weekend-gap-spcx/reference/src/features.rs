//! What the reference's forecaster reads, turn by turn: the same numbers
//! on chain as in the offline tooling that trains it. Plain Rust, no
//! dependencies.
//!
//! A tape holds the perp's price and Phoenix's index each turn, and every
//! taker order with its account. The features mix the structural model's
//! read (where it expects the open, who it takes for informed, how sure it
//! is) with plain flow statistics per account: what a trader watching the
//! tape would track.

use crate::model::{Model, Params, Trade};

/// Per account: id, net, gross, orders, first and last turn, the level
/// before its first order, and the summed price change across the turns its
/// orders landed in.
type Account = (u16, f64, f64, usize, u64, u64, f64, f64);

/// The turn-by-turn record of a session.
#[derive(Debug, Clone, Default)]
pub struct Tape {
    pub model: Model,
    /// (slot, perp level, index level) each turn, in percent of the start.
    pub obs: Vec<(u64, f64, f64)>,
    pub trades: Vec<Trade>,
}

/// How many features [`Tape::features`] returns, and which of them flip
/// sign when the market is mirrored (buys for sells, up for down).
pub const COUNT: usize = 37;
/// What the told rung is told: who the informed trader and the worker are,
/// the half-life of uninformed impact, and where SPCX opens (as the
/// flash-crash task's regime oracle is told where its session ends).
#[derive(Debug, Clone, Copy)]
pub struct Told {
    pub informed: Option<u16>,
    pub worker: Option<u16>,
    pub half_life: f64,
    pub open: f64,
}

/// The told rung's extra features, and which of them flip sign.
pub const COUNT_TOLD: usize = 49;
pub const SIGNED_TOLD_EXTRA: [usize; 6] = [41, 42, 43, 46, 47, 48];
pub const SIGNED: [usize; 20] = [
    2, 3, 4, 5, 6, 7, 8, 11, 15, 16, 19, 20, 23, 24, 25, 28, 29, 30, 31, 32,
];

impl Tape {
    pub fn observe(&mut self, slot: u64, level: f64, index: f64) {
        self.model.observe(slot, level);
        self.obs.push((slot, level, index));
    }

    pub fn trade(&mut self, trade: Trade) {
        self.model.trade(trade);
        let at = self.trades.partition_point(|t| t.seen <= trade.seen);
        self.trades.insert(at, trade);
    }

    fn level_back(&self, now: u64, back: u64) -> f64 {
        self.obs
            .iter()
            .rev()
            .find(|o| o.0 + back <= now)
            .map_or(0.0, |o| o.1)
    }

    /// The features at turn `now`, `reopen` slots into the session.
    pub fn features(&self, params: &Params, now: u64, reopen: u64) -> Vec<f64> {
        self.features_and_top(params, now, reopen).0
    }

    /// [`Tape::features`] for the ladder's told rung: four more, whether
    /// each of the two top accounts is the informed trader or the worker.
    pub fn features_told(&self, params: &Params, now: u64, reopen: u64, told: Told) -> Vec<f64> {
        let Told {
            informed,
            worker,
            half_life,
            open,
        } = told;
        let (mut x, top) = self.features_and_top(params, now, reopen);
        for account in top {
            let flag = |who: Option<u16>| f64::from(account.is_some() && account == who);
            x.push(flag(informed));
            x.push(flag(worker));
        }
        // The exact decomposition, given who is who and the half-life (and,
        // last, where SPCX opens, as the regime tells it): the
        // informed trader's impact (permanent), everyone else's still in the
        // price, and what's left (drift).
        let decay = 0.5_f64.powf(1.0 / half_life.max(1.0));
        let level = self.obs.last().map_or(0.0, |o| o.1);
        let mut permanent = 0.0;
        let mut passing = 0.0;
        for t in &self.trades {
            let j = self.jump_share(t);
            if Some(t.account) == informed {
                permanent += j;
            } else {
                passing += j * decay.powi(now.saturating_sub(t.seen) as i32);
            }
        }
        let active = |who: Option<u16>| {
            f64::from(
                self.trades
                    .iter()
                    .any(|t| Some(t.account) == who && t.seen + 24 > now),
            )
        };
        let worker_net: f64 = self
            .trades
            .iter()
            .filter(|t| Some(t.account) == worker)
            .map(|t| t.side * t.size)
            .sum();
        x.extend([
            permanent,
            passing,
            level - permanent - passing,
            active(informed),
            active(worker),
            worker_net,
            open - level,
            open,
        ]);
        debug_assert_eq!(x.len(), COUNT_TOLD);
        x
    }

    fn features_and_top(
        &self,
        params: &Params,
        now: u64,
        reopen: u64,
    ) -> (Vec<f64>, [Option<u16>; 2]) {
        let (level, index) = self.obs.last().map_or((0.0, 0.0), |o| (o.1, o.2));
        let estimate = self.model.estimate(params, now);
        let (open, odds, active, none, half_life, informed) = match &estimate {
            Some(e) => (
                e.open_pct,
                e.informed_odds,
                if e.active { 1.0 } else { 0.0 },
                e.hypotheses
                    .iter()
                    .filter(|h| !h.2)
                    .map(|h| h.0)
                    .sum::<f64>(),
                e.half_life,
                e.informed,
            ),
            None => (0.0, 0.0, 0.0, 1.0, 0.0, None),
        };
        // Per account: net, gross, orders, first and last turn, the level
        // before its first order, and the summed price change across the
        // turns its orders landed in.
        let mut accounts: Vec<Account> = Vec::new();
        let _ = &params;
        for t in &self.trades {
            let jump = self.jump_share(t);
            match accounts.iter_mut().find(|a| a.0 == t.account) {
                Some(a) => {
                    a.1 += t.side * t.size;
                    a.2 += t.size;
                    a.3 += 1;
                    a.5 = t.seen;
                    a.7 += jump;
                }
                None => accounts.push((
                    t.account,
                    t.side * t.size,
                    t.size,
                    1,
                    t.seen,
                    t.seen,
                    self.level_back(t.seen, 2),
                    jump,
                )),
            }
        }
        // The two accounts that trade most one way.
        let score = |x: &Account| x.1.abs() * (x.1.abs() / x.2);
        accounts.sort_by(|a, b| score(b).total_cmp(&score(a)));
        let flow = |back: u64| -> f64 {
            self.trades
                .iter()
                .filter(|t| t.seen + back > now)
                .map(|t| t.side * t.size)
                .sum()
        };
        let mut per_share: Vec<f64> = self
            .trades
            .iter()
            .map(|t| self.jump_share(t) / (t.side * t.size))
            .filter(|v| v.is_finite())
            .collect();
        per_share.sort_by(f64::total_cmp);
        let impact = per_share.get(per_share.len() / 2).copied().unwrap_or(0.0);
        let top_is_informed = match (accounts.first(), informed) {
            (Some(a), Some(i)) if a.0 == i => 1.0,
            _ => 0.0,
        };
        let mut x = vec![
            now as f64 / 600.0,
            reopen.saturating_sub(now) as f64 / 600.0,
            level,
            level - index,
            level - self.level_back(now, 8),
            level - self.level_back(now, 32),
            level - self.level_back(now, 64),
            level - self.level_back(now, 128),
            open - level,
            odds,
            active,
            open,
            none,
            half_life / 100.0,
            top_is_informed,
            flow(32),
            flow(128),
            self.trades.len() as f64 / 50.0,
            impact * 10.0,
            level * active,
        ];
        for slot in 0..2 {
            match accounts.get(slot) {
                Some(a) => {
                    let retention = if a.7.abs() > 1e-6 {
                        ((level - a.6) / a.7).clamp(-2.0, 3.0)
                    } else {
                        0.0
                    };
                    x.extend([
                        a.1,
                        a.3 as f64,
                        (now.saturating_sub(a.5) as f64 / 100.0).min(5.0),
                        a.7,
                        retention * a.1.signum(),
                    ]);
                }
                None => x.extend([0.0; 5]),
            }
        }
        // What moved the perp on turns with no order: decay and drift.
        let quiet = |back: u64| -> f64 {
            self.obs
                .windows(2)
                .filter(|w| w[1].0 + back > now)
                .filter(|w| !self.trades.iter().any(|t| t.seen == w[1].0))
                .map(|w| w[1].1 - w[0].1)
                .sum()
        };
        // The uninformed impact still in the price, at the model's half-life,
        // taking its informed candidate's for permanent.
        let decay = if half_life > 0.0 {
            0.5_f64.powf(1.0 / half_life)
        } else {
            0.0
        };
        let passing: f64 = self
            .trades
            .iter()
            .filter(|t| Some(t.account) != informed)
            .map(|t| self.jump_share(t) * decay.powi(now.saturating_sub(t.seen) as i32))
            .sum();
        x.extend([quiet(16), quiet(48), passing]);
        // How the two top accounts trade: their orders' limits (in
        // half-spreads) and sizes, on average.
        for slot in 0..2 {
            let mine: Vec<&Trade> = accounts
                .get(slot)
                .map(|a| self.trades.iter().filter(|t| t.account == a.0).collect())
                .unwrap_or_default();
            let n = mine.len().max(1) as f64;
            x.push(mine.iter().map(|t| t.limit).sum::<f64>() / n);
            x.push(mine.iter().map(|t| t.size).sum::<f64>() / n);
        }
        debug_assert_eq!(x.len(), COUNT);
        (
            x,
            [accounts.first().map(|a| a.0), accounts.get(1).map(|a| a.0)],
        )
    }

    /// The price change across the turn `t` landed in, split across that
    /// turn's orders by signed size.
    fn jump_share(&self, t: &Trade) -> f64 {
        let Some(k) = self.obs.iter().position(|o| o.0 == t.seen) else {
            return 0.0;
        };
        if k == 0 {
            return 0.0;
        }
        let dp = self.obs[k].1 - self.obs[k - 1].1;
        let total: f64 = self
            .trades
            .iter()
            .filter(|o| o.seen == t.seen)
            .map(|o| o.side * o.size)
            .sum();
        if total.abs() > 1e-9 {
            dp * t.side * t.size / total
        } else {
            0.0
        }
    }
}
