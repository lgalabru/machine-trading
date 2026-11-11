//! Markout: what a session's fills earned against the scenario's fair value.
//!
//! A session's PnL mixes two things. One is how well the bot traded: whether
//! each fill cost less than the asset was worth when it happened. The other
//! is luck: the position it happened to hold while the price moved. Over a
//! few sessions the second swamps the first. When the scenario knows its own
//! fair value at every step, the two can be separated exactly.
//!
//! # Method
//!
//! At each step a [`MarkoutSample`] records the fair value `F`, the price `S` the
//! position is valued at, the signed position `p` and the account's value
//! `V = cash + p·S`. From one step to the next, with `q` the change in
//! position and `c` the change in cash (fills, fees and funding alike):
//!
//! ```text
//! ΔV = (c + q·F)                   markout: what the trades cost against fair value
//!    + p_prev · ΔF                 luck: the old position times the fair value's move
//!    + (p·D − p_prev·D_prev)       residual, with D = S − F
//! ```
//!
//! The terms hold for any step, so summed over a session
//! `pnl = markout + luck + residual` exactly. The residual telescopes to the
//! final position's distance between the valuation price and fair value, which
//! is zero when the bot ends flat or the valuation is the fair value. A score
//! free of luck is `markout + residual`, [`Decomposition::edge`].
//!
//! The meter reads the fair value, never a later one: it is the value the
//! scenario *plans* for each step, not a forecast built from the walk's
//! future. A bot that trades at random times has an expected markout of
//! zero, whatever the price does afterwards.
//!
//! Amounts are `f64`, as the session records they are read from are.

use serde::Serialize;

/// The market and the account at one step.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MarkoutSample {
    /// The scenario's fair value, in USD.
    pub fair: f64,
    /// The price the position is valued at, in USD.
    pub price: f64,
    /// The signed position, in base units.
    pub position: f64,
    /// The account's value, in USD, as the session values it.
    pub value: f64,
}

/// A session's PnL, split by [`MarkoutMeter`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Decomposition {
    /// The account's change in value over the session.
    pub pnl_usd: f64,
    /// What the fills, fees and funding cost against fair value.
    pub markout_usd: f64,
    /// The position held at each step times the fair value's move.
    pub luck_usd: f64,
    /// The final position's valuation against fair value, less the first's.
    pub residual_usd: f64,
}

impl Decomposition {
    /// PnL without the luck: `markout + residual`.
    pub fn edge_usd(&self) -> f64 {
        self.markout_usd + self.residual_usd
    }
}

/// Splits a session's PnL into markout, luck and a residual, step by step.
#[derive(Debug, Clone)]
pub struct MarkoutMeter {
    first: MarkoutSample,
    last: MarkoutSample,
    markout: f64,
    luck: f64,
}

impl MarkoutMeter {
    /// Starts a session at its first sample.
    pub fn open(first: MarkoutSample) -> Self {
        Self {
            first,
            last: first,
            markout: 0.0,
            luck: 0.0,
        }
    }

    /// Adds the next step's sample.
    pub fn record(&mut self, next: MarkoutSample) {
        let last = self.last;
        let traded = next.position - last.position;
        let cash =
            (next.value - next.position * next.price) - (last.value - last.position * last.price);
        self.markout += cash + traded * next.fair;
        self.luck += last.position * (next.fair - last.fair);
        self.last = next;
    }

    /// The split so far.
    pub fn decomposition(&self) -> Decomposition {
        let gap = |s: &MarkoutSample| s.position * (s.price - s.fair);
        Decomposition {
            pnl_usd: self.last.value - self.first.value,
            markout_usd: self.markout,
            luck_usd: self.luck,
            residual_usd: gap(&self.last) - gap(&self.first),
        }
    }

    /// The split of a whole session, or `None` for an empty one.
    pub fn measure(samples: &[MarkoutSample]) -> Option<Decomposition> {
        let (first, rest) = samples.split_first()?;
        let mut meter = Self::open(*first);
        for sample in rest {
            meter.record(*sample);
        }
        Some(meter.decomposition())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A small deterministic generator, so the tests need no dependency.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> f64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            ((z ^ (z >> 31)) >> 11) as f64 / (1u64 << 53) as f64
        }

        /// Uniform in [-1, 1).
        fn signed(&mut self) -> f64 {
            2.0 * self.next() - 1.0
        }
    }

    const STEPS: usize = 40;

    /// A session of a fair value that random-walks and a valuation price
    /// that sits a noisy distance from it. `trade` picks the position
    /// to hold after each step, and `fill` the price it trades at, given the
    /// previous step's fair value and the new one.
    fn session(
        rng: &mut Rng,
        trade: &mut dyn FnMut(&mut Rng, usize, f64, f64) -> f64,
        fill: &dyn Fn(f64, f64, f64) -> f64,
    ) -> Vec<MarkoutSample> {
        let mut fair = 100.0;
        let mut position = 0.0;
        let mut cash = 1_000.0;
        let mut samples = vec![MarkoutSample {
            fair,
            price: fair,
            position,
            value: cash,
        }];
        for k in 0..STEPS {
            let before = fair;
            fair += rng.signed() * 0.5;
            let target = trade(rng, k, before, fair);
            let traded = target - position;
            // The trade lands during the step, so it fills at a price set
            // from the fair values on either side of it.
            cash -= traded * fill(before, fair, traded);
            position = target;
            let price = fair + rng.signed() * 0.05;
            samples.push(MarkoutSample {
                fair,
                price,
                position,
                value: cash + position * price,
            });
        }
        samples
    }

    fn mean_and_se(values: &[f64]) -> (f64, f64) {
        let n = values.len() as f64;
        let mean = values.iter().sum::<f64>() / n;
        let var = values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (n - 1.0);
        (mean, (var / n).sqrt())
    }

    #[test]
    fn markout_luck_and_residual_make_the_pnl() {
        let mut rng = Rng(7);
        for _ in 0..200 {
            let samples = session(
                &mut rng,
                &mut |rng, _, _, _| (rng.signed() * 10.0).round(),
                // Fills at an arbitrary price, fees and funding included.
                &|before, fair, _| 0.5 * (before + fair) + 0.01,
            );
            let d = MarkoutMeter::measure(&samples).unwrap();
            let sum = d.markout_usd + d.luck_usd + d.residual_usd;
            assert!(
                (d.pnl_usd - sum).abs() < 1e-9 * (1.0 + d.pnl_usd.abs()),
                "pnl {} != {sum}",
                d.pnl_usd
            );
            assert!((d.edge_usd() + d.luck_usd - d.pnl_usd).abs() < 1e-9);
        }
    }

    #[test]
    fn a_bot_that_trades_at_random_times_has_no_markout() {
        let mut rng = Rng(11);
        let mut markouts = Vec::new();
        let mut pnls = Vec::new();
        for _ in 0..4_000 {
            // Flips to a random position at random steps.
            let mut held = 0.0;
            let samples = session(
                &mut rng,
                &mut |rng, _, _, _| {
                    if rng.next() < 0.2 {
                        held = (rng.signed() * 10.0).round();
                    }
                    held
                },
                &|before, _, _| before,
            );
            let d = MarkoutMeter::measure(&samples).unwrap();
            markouts.push(d.markout_usd);
            pnls.push(d.pnl_usd);
        }
        let (mean, se) = mean_and_se(&markouts);
        assert!(mean.abs() < 4.0 * se, "markout {mean} (se {se})");
    }

    #[test]
    fn holding_a_position_is_luck_not_markout() {
        let mut rng = Rng(5);
        let mut markouts = Vec::new();
        let mut pnls = Vec::new();
        for _ in 0..1_000 {
            // Buys 10 at the first step's fair value, then holds.
            let samples = session(&mut rng, &mut |_, _, _, _| 10.0, &|before, _, _| before);
            let d = MarkoutMeter::measure(&samples).unwrap();
            markouts.push(d.markout_usd);
            pnls.push(d.pnl_usd);
        }
        let sd = |values: &[f64]| mean_and_se(values).1 * (values.len() as f64).sqrt();
        // Only the first step's own move is in the markout; the other 39
        // steps' are luck.
        assert!(sd(&markouts) < 0.25 * sd(&pnls));
    }

    #[test]
    fn paying_the_spread_is_a_negative_markout_and_foresight_a_positive_one() {
        let mut rng = Rng(3);
        let half_spread = 0.04;
        let mut paying = Vec::new();
        let mut foresight = Vec::new();
        for _ in 0..400 {
            // Buys 1 unit at fair + half spread, then sells it back at fair
            // minus half spread: the markout is the spread it paid, twice.
            let samples = session(
                &mut rng,
                &mut |_, k, _, _| if k % 2 == 0 { 1.0 } else { 0.0 },
                &|_, fair, traded| fair + half_spread * traded.signum(),
            );
            let d = MarkoutMeter::measure(&samples).unwrap();
            let trades = (STEPS / 2) as f64;
            assert!((d.markout_usd + trades * 2.0 * half_spread).abs() < 1e-9);
            paying.push(d.markout_usd);

            // Knows which way the fair value moves in the step it trades in
            // and fills at the earlier fair value.
            let samples = session(
                &mut rng,
                &mut |_, _, before, fair| if fair > before { 1.0 } else { -1.0 },
                &|before, _, _| before,
            );
            foresight.push(MarkoutMeter::measure(&samples).unwrap().markout_usd);
        }
        assert!(mean_and_se(&paying).0 < 0.0);
        let (mean, se) = mean_and_se(&foresight);
        assert!(mean > 10.0 * se, "foresight markout {mean} (se {se})");
    }

    #[test]
    fn a_bot_that_ends_flat_at_fair_value_has_no_residual() {
        let samples = vec![
            MarkoutSample {
                fair: 10.0,
                price: 10.0,
                position: 0.0,
                value: 100.0,
            },
            MarkoutSample {
                fair: 11.0,
                price: 11.2,
                position: 2.0,
                value: 100.0 - 2.0 * 10.5 + 2.0 * 11.2,
            },
            MarkoutSample {
                fair: 11.5,
                price: 11.6,
                position: 0.0,
                value: 100.0 - 2.0 * 10.5 + 2.0 * 11.4,
            },
        ];
        let d = MarkoutMeter::measure(&samples).unwrap();
        assert_eq!(d.residual_usd, 0.0);
        assert!((d.pnl_usd - 1.8).abs() < 1e-12);
        // Bought 2 at 10.5 when fair was 11: +1. Sold 2 at 11.4 against
        // a fair value of 11.5: -0.2.
        assert!((d.markout_usd - 0.8).abs() < 1e-12);
        // Held 2 through a 0.5 rise in fair value.
        assert!((d.luck_usd - 1.0).abs() < 1e-12);
    }

    #[test]
    fn an_empty_session_has_no_decomposition() {
        assert_eq!(MarkoutMeter::measure(&[]), None);
    }
}
