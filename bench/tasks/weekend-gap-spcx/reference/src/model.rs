//! Where SPCX will open, from the perp's own weekend: the reference's model.
//!
//! Plain Rust with no dependencies, so the offline tooling that tunes it
//! runs the same code on generated sessions that the bot runs on chain.
//!
//! What it reads, each turn: the perp's price (the makers' mid, in percent
//! of the session's starting mark) and every taker order since the last
//! turn (which account, which way, how many shares). What it infers:
//!
//! 1. **Each order's impact**, from the price change across the turn it
//!    landed in.
//! 2. **Who might know something.** An account that only ever trades one way
//!    is either the informed trader or someone working a large order. The
//!    rest is noise.
//! 3. **Which of them it is.** Uninformed pressure fades as the makers lean
//!    back; informed flow doesn't. For each half-life on a grid, and each
//!    hypothesis (no informed trader, or one of the one-sided accounts), it
//!    rebuilds the price from the orders' impacts, keeps the informed
//!    account's impact and lets the rest decay, and scores how well the
//!    rebuilt price explains the observed one over 16-slot changes, where
//!    the thin-book drift averages out. The scores, with the instruction's
//!    odds as the prior, give each hypothesis its probability.
//! 4. **Where each hypothesis puts the open.** Without an informed trader,
//!    at the last external price: the reopen takes the rest back. With one,
//!    at the level its orders moved the perp to, and if it's still trading
//!    in the last moments before the reopen, beyond: by what the prior on
//!    the weekend's move says is left past that level.
//!
//! The target position is the expected gap between the open and the perp,
//! scaled, and capped by what the session's loss limit can carry through
//! the worst open any likely hypothesis allows.

/// One taker order, as the chain shows it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Trade {
    /// Slot, relative to the session's start, of the turn it was seen at.
    pub seen: u64,
    /// The account that sent it, numbered in order of first appearance.
    pub account: u16,
    /// +1 buys, -1 sells.
    pub side: f64,
    pub size: f64,
    /// How far from the perp's price the order would still fill, in
    /// half-spreads: how much of a hurry it's in.
    pub limit: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Params {
    /// From how many slots before the reopen it takes a position.
    pub enter_slots: u64,
    /// The gap, in percent, that takes the full position.
    pub scale_pct: f64,
    /// An account that traded within this many slots is still trading.
    pub active_slots: u64,
    /// One-sided accounts: at least this many orders, all one way.
    pub one_sided_orders: usize,
    /// The odds of an informed trader, from the instruction.
    pub informed_odds: f64,
    /// How much to trust the fit: the scores are divided by this before
    /// they become probabilities (overlapping changes aren't independent).
    pub temper: f64,
    /// The weekend's move, as the instruction gives it: log-uniform in size.
    pub weekend_lo: f64,
    pub weekend_hi: f64,
    /// The band around the last external price.
    pub band_pct: f64,
    /// What it will risk through the reopen, in USD; the slack past an
    /// informed level it sizes for; how far the open can be from the last
    /// external price when nobody knew; and the least likely hypothesis
    /// it sizes for.
    pub loss_budget_usd: f64,
    pub known_side_slack_pct: f64,
    pub unknown_side_pct: f64,
    pub worst_case_odds: f64,
    /// Before the last `enter_slots`, it trades the same gap without the
    /// reopen's risk: the open is what the perp converges to as uninformed
    /// impact decays. From this many slots in, with this scale, moving only
    /// when the target is this many shares away.
    pub trade_from_slots: u64,
    pub mid_scale_pct: f64,
    pub dead_band: f64,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            enter_slots: 24,
            scale_pct: 0.6,
            active_slots: 24,
            one_sided_orders: 3,
            informed_odds: 0.6,
            temper: 1.0,
            weekend_lo: 0.5,
            weekend_hi: 12.0,
            band_pct: 100.0 / 15.0,
            loss_budget_usd: 550.0,
            known_side_slack_pct: 2.0,
            unknown_side_pct: 6.0,
            worst_case_odds: 0.05,
            trade_from_slots: u64::MAX,
            mid_scale_pct: 1.0,
            dead_band: 5.0,
        }
    }
}

/// What the model makes of the session so far.
#[derive(Debug, Clone, PartialEq)]
pub struct Estimate {
    /// Where it expects SPCX to open, in percent of the starting mark.
    pub open_pct: f64,
    /// The hypotheses it holds: (probability, the open it implies, whether
    /// an informed trader is behind it).
    pub hypotheses: Vec<(f64, f64, bool)>,
    /// The most likely informed account, its probability, and whether it's
    /// still trading.
    pub informed: Option<u16>,
    pub informed_odds: f64,
    pub active: bool,
    pub half_life: f64,
}

#[derive(Debug, Clone, Default)]
pub struct Model {
    /// (slot, perp level) at each turn.
    obs: Vec<(u64, f64)>,
    trades: Vec<Trade>,
}

const HALF_LIVES: [f64; 7] = [40.0, 60.0, 90.0, 130.0, 180.0, 250.0, 350.0];

/// See [`Model::fit`].
#[derive(Debug, Clone)]
pub struct Fit {
    /// (account, its permanent level if informed, its last order) per
    /// one-sided account.
    candidates: Vec<(u16, f64, u64)>,
    /// Log-likelihood per (half-life, hypothesis): hypothesis 0 is "no
    /// informed trader", i > 0 is candidate i - 1.
    scores: Vec<Vec<f64>>,
    /// Residual variance of the best fit.
    sigma2: f64,
}

impl Model {
    pub fn observe(&mut self, slot: u64, level_pct: f64) {
        self.obs.push((slot, level_pct));
    }

    pub fn trade(&mut self, trade: Trade) {
        // Kept in the order they landed: a late listing goes back in place.
        let at = self.trades.partition_point(|t| t.seen <= trade.seen);
        self.trades.insert(at, trade);
    }

    /// Each trade's observed impact: the price change across the turn it
    /// was seen at, split across that turn's trades by signed size.
    fn jumps(&self) -> Vec<(u64, u16, f64)> {
        let mut jumps = Vec::new();
        let mut i = 0;
        while i < self.trades.len() {
            let seen = self.trades[i].seen;
            let mut j = i;
            while j < self.trades.len() && self.trades[j].seen == seen {
                j += 1;
            }
            let at = self.obs.iter().position(|(s, _)| *s == seen);
            if let Some(k) = at.filter(|&k| k > 0) {
                let dp = self.obs[k].1 - self.obs[k - 1].1;
                let total: f64 = self.trades[i..j].iter().map(|t| t.side * t.size).sum();
                for t in &self.trades[i..j] {
                    let share = if total.abs() > 1e-9 {
                        t.side * t.size / total
                    } else {
                        1.0 / (j - i) as f64
                    };
                    jumps.push((seen, t.account, dp * share));
                }
            }
            i = j;
        }
        jumps
    }

    pub fn estimate(&self, params: &Params, now: u64) -> Option<Estimate> {
        self.fit(params).map(|fit| estimate_from(&fit, params, now))
    }

    /// The scores of every hypothesis, which depend only on what the
    /// session showed (and on which accounts count as one-sided).
    pub fn fit(&self, params: &Params) -> Option<Fit> {
        if self.obs.len() < 16 {
            return None;
        }
        let jumps = self.jumps();
        // One-sided accounts, the three largest by volume.
        let mut by_account: Vec<(u16, f64, f64, usize, u64)> = Vec::new();
        for t in &self.trades {
            match by_account.iter_mut().find(|a| a.0 == t.account) {
                Some(a) => {
                    a.1 += t.side * t.size;
                    a.2 += t.size;
                    a.3 += 1;
                    a.4 = a.4.max(t.seen);
                }
                None => by_account.push((t.account, t.side * t.size, t.size, 1, t.seen)),
            }
        }
        let mut one_sided: Vec<(u16, f64, f64, usize, u64)> = by_account
            .into_iter()
            .filter(|a| a.3 >= params.one_sided_orders && (a.1.abs() - a.2).abs() < 1e-9)
            .collect();
        one_sided.sort_by(|a, b| b.2.total_cmp(&a.2));
        one_sided.truncate(3);
        let edge = params.band_pct - 0.15;
        let candidates: Vec<(u16, f64, u64)> = one_sided
            .iter()
            .map(|a| {
                let level: f64 = jumps.iter().filter(|j| j.1 == a.0).map(|j| j.2).sum();
                (a.0, level.clamp(-edge, edge), a.4)
            })
            .collect();

        let mut sses = Vec::with_capacity(HALF_LIVES.len());
        let mut rows = 0;
        for &h in &HALF_LIVES {
            let decay = 0.5_f64.powf(1.0 / h);
            let mut per = Vec::with_capacity(candidates.len() + 1);
            // Running sums over the turns: every impact decaying, and each
            // candidate's impact decaying and whole.
            let k = candidates.len();
            let mut all = 0.0;
            let mut mine_decayed = vec![0.0; k];
            let mut mine_whole = vec![0.0; k];
            let mut rebuilt: Vec<Vec<f64>> = vec![Vec::with_capacity(self.obs.len()); k + 1];
            let mut prev = self.obs.first().map_or(0, |o| o.0);
            let mut next_jump = 0;
            for &(slot, level) in &self.obs {
                let fade = decay.powi((slot - prev) as i32);
                all *= fade;
                for m in mine_decayed.iter_mut() {
                    *m *= fade;
                }
                while next_jump < jumps.len() && jumps[next_jump].0 <= slot {
                    let (_, a, j) = jumps[next_jump];
                    all += j;
                    if let Some(c) = candidates.iter().position(|c| c.0 == a) {
                        mine_decayed[c] += j;
                        mine_whole[c] += j;
                    }
                    next_jump += 1;
                }
                prev = slot;
                rebuilt[0].push(level - all);
                for c in 0..k {
                    rebuilt[c + 1].push(level - (all - mine_decayed[c] + mine_whole[c]));
                }
            }
            for residual in rebuilt {
                let mut sse = 0.0;
                rows = 0;
                let mut i = 0;
                while i + 8 < residual.len() {
                    sse += (residual[i + 8] - residual[i]).powi(2);
                    rows += 1;
                    i += 2;
                }
                per.push(sse);
            }
            sses.push(per);
        }
        let min = sses
            .iter()
            .flatten()
            .cloned()
            .fold(f64::MAX, f64::min)
            .max(1e-9);
        let sigma2 = min / rows.max(1) as f64;
        let scores = sses
            .iter()
            .map(|per| per.iter().map(|sse| -sse / (2.0 * sigma2)).collect())
            .collect();
        Some(Fit {
            candidates,
            scores,
            sigma2,
        })
    }
}

/// What a fit means for the open, under `params`.
pub fn estimate_from(fit: &Fit, params: &Params, now: u64) -> Estimate {
    let k = fit.candidates.len();
    // Priors: no informed trader, or one of the candidates.
    let prior = |hyp: usize| {
        if hyp == 0 {
            1.0 - params.informed_odds
        } else {
            params.informed_odds / k as f64
        }
    };
    let mut weights = vec![0.0; k + 1];
    let mut best_h = (f64::MIN, 0.0);
    let top = fit
        .scores
        .iter()
        .flatten()
        .cloned()
        .fold(f64::MIN, f64::max);
    for (hi, per) in fit.scores.iter().enumerate() {
        for (hyp, score) in per.iter().enumerate() {
            let w = prior(hyp) * ((score - top) / params.temper).exp();
            weights[hyp] += w;
            if *score > best_h.0 {
                best_h = (*score, HALF_LIVES[hi]);
            }
        }
    }
    let total: f64 = weights.iter().sum();
    let mut hypotheses = Vec::with_capacity(k + 1);
    let mut open = 0.0;
    let mut informed: Option<(f64, u16, bool)> = None;
    for (hyp, w) in weights.iter().enumerate() {
        let p = w / total;
        let (level, informed_here) = if hyp == 0 {
            (0.0, false)
        } else {
            let (account, permanent, last) = fit.candidates[hyp - 1];
            let active = now.saturating_sub(last) < params.active_slots;
            let level = if active {
                let p = permanent.abs().max(params.weekend_lo);
                let beyond = if p < params.weekend_hi - 1e-6 {
                    (params.weekend_hi - p) / (params.weekend_hi / p).ln()
                } else {
                    params.weekend_hi
                };
                beyond.copysign(permanent)
            } else {
                permanent
            };
            if informed.is_none_or(|(q, ..)| p > q) {
                informed = Some((p, account, active));
            }
            (level, true)
        };
        open += p * level;
        hypotheses.push((p, level, informed_here));
    }
    let _ = fit.sigma2;
    Estimate {
        open_pct: open,
        hypotheses,
        informed: informed.map(|i| i.1),
        informed_odds: informed.map_or(0.0, |i| i.0),
        active: informed.is_some_and(|i| i.2),
        half_life: best_h.1,
    }
}

/// The position to hold into the reopen, in shares: the expected gap
/// between the open and the perp, scaled, capped by `cap` and by what the
/// loss budget can carry through the worst open a likely hypothesis allows.
pub fn target(
    params: &Params,
    estimate: &Estimate,
    level_pct: f64,
    start_mark_usd: f64,
    cap: f64,
) -> f64 {
    let gap = estimate.open_pct - level_pct;
    let raw = cap * (gap / params.scale_pct).clamp(-1.0, 1.0);
    let side = gap.signum();
    // How far the open could land against the position, under each likely
    // hypothesis.
    let adverse_pct = estimate
        .hypotheses
        .iter()
        .filter(|h| h.0 >= params.worst_case_odds)
        .map(|&(_, open, informed)| {
            let slack = if informed {
                params.known_side_slack_pct
            } else {
                params.unknown_side_pct
            };
            ((level_pct - open) * side).max(0.0) + slack
        })
        .fold(params.known_side_slack_pct, f64::max);
    let per_share = start_mark_usd * adverse_pct / 100.0;
    let room = params.loss_budget_usd / per_share;
    raw.clamp(-room, room)
}
