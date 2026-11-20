//! Rolling features a strategy feeds one value a step: what a model reads.
//!
//! A strategy sees one [`Market`](crate::Market) a step and keeps its own
//! history. These types keep it in fixed memory: each is made with a
//! capacity, holds at most that many samples, and forgets the oldest.
//!
//! - [`Window`]: the last N values, with returns over a lag
//!   ([`Window::simple_return`], [`Window::log_return`]), the mean and
//!   standard deviation, a z-score and the volatility of one-step log
//!   returns.
//! - [`Ema`]: an exponential moving average, in constant memory.
//! - [`Bars`]: open, high, low and close over every N steps.
//!
//! ```
//! use mtx_kit::features::{Bars, Ema, Window};
//!
//! let mut marks = Window::new(64)?;
//! let mut trend = Ema::new(20)?;
//! let mut bars = Bars::new(10, 32)?;
//! for mark in [100.0, 100.5, 99.8] {
//!     marks.push(mark)?;
//!     trend.update(mark)?;
//!     bars.push(mark)?;
//! }
//! assert_eq!(marks.simple_return(2), Some(99.8 / 100.0 - 1.0));
//! assert!(marks.z_score().is_some());
//! # Ok::<(), mtx_kit::features::FeatureError>(())
//! ```
//!
//! Every feature is causal: it reads only what was pushed. A value that is
//! not finite is refused and changes nothing, so one bad read cannot poison
//! a window for its whole length. A feature that cannot be computed yet
//! (too few samples, a zero base, the log of a value that is not positive,
//! a result too large for an `f64`) is `None`, never a made-up
//! number: decide in the strategy what the model sees then, and make the
//! training pipeline decide the same.
//!
//! The statistics walk the window when asked, so each costs time in
//! proportion to the samples held (at most [`WINDOW_CAPACITY_MAX`]), which
//! is small against a step's budget.

/// The most samples a [`Window`] or bars a [`Bars`] may hold. Sessions run
/// for hundreds to a few thousand steps, so this already covers a whole
/// session; at 8 bytes a sample it is 32 KiB a window.
pub const WINDOW_CAPACITY_MAX: usize = 4_096;

/// The most steps one [`Bar`] may span, and the longest [`Ema`] span. A bar
/// longer than this would not close within a session, so asking for one is
/// a mistake, not a setting.
pub const SPAN_STEPS_MAX: u32 = 65_536;

/// Why a feature was not made, or a value not taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum FeatureError {
    /// A capacity of zero holds nothing.
    #[error("the capacity must be at least 1")]
    CapacityZero,
    /// The capacity is over [`WINDOW_CAPACITY_MAX`].
    #[error("the capacity {capacity} is over the maximum, {WINDOW_CAPACITY_MAX}")]
    CapacityTooLarge {
        /// The capacity asked for.
        capacity: usize,
    },
    /// A span in steps is zero or over [`SPAN_STEPS_MAX`].
    #[error("the span, {span_steps} steps, must be between 1 and {SPAN_STEPS_MAX}")]
    SpanOutOfRange {
        /// The span asked for.
        span_steps: u32,
    },
    /// The value is NaN or infinite; nothing was changed.
    #[error("the value is not finite")]
    NotFinite,
}

/// The last `capacity` values pushed, oldest forgotten first.
#[derive(Debug, Clone)]
pub struct Window {
    ring: Ring<f64>,
}

impl Window {
    /// A window of at most `capacity` values, between 1 and
    /// [`WINDOW_CAPACITY_MAX`].
    pub fn new(capacity: usize) -> Result<Self, FeatureError> {
        Ok(Self {
            ring: Ring::new(capacity)?,
        })
    }

    /// Adds this step's value, forgetting the oldest when full. A value
    /// that is not finite is refused and the window is unchanged.
    pub fn push(&mut self, value: f64) -> Result<(), FeatureError> {
        if !value.is_finite() {
            return Err(FeatureError::NotFinite);
        }
        self.ring.push(value);
        Ok(())
    }

    /// How many values it holds, at most [`capacity`](Self::capacity).
    pub fn len(&self) -> usize {
        self.ring.len
    }

    /// Whether nothing was pushed yet.
    pub fn is_empty(&self) -> bool {
        self.ring.len == 0
    }

    /// Whether it holds `capacity` values, so the next push forgets one.
    pub fn is_full(&self) -> bool {
        self.ring.len == self.ring.capacity()
    }

    /// The most values it holds.
    pub fn capacity(&self) -> usize {
        self.ring.capacity()
    }

    /// The latest value.
    pub fn last(&self) -> Option<f64> {
        self.ago(0)
    }

    /// The value `lag` pushes before the latest: `ago(0)` is the latest.
    /// `None` when the window doesn't reach that far back.
    pub fn ago(&self, lag: usize) -> Option<f64> {
        self.ring.ago(lag)
    }

    /// The values held, oldest first.
    pub fn iter(&self) -> impl Iterator<Item = f64> + '_ {
        self.ring.iter()
    }

    /// The simple return over `lag` pushes: `latest / ago(lag) - 1`. `None`
    /// when the window doesn't reach that far back or the older value is
    /// zero.
    pub fn simple_return(&self, lag: usize) -> Option<f64> {
        let (latest, base) = (self.last()?, self.ago(lag)?);
        finite(latest / base - 1.0)
    }

    /// The log return over `lag` pushes: `ln(latest / ago(lag))`. `None`
    /// when the window doesn't reach that far back or either value is not
    /// positive, as a price always is.
    pub fn log_return(&self, lag: usize) -> Option<f64> {
        let (latest, base) = (self.last()?, self.ago(lag)?);
        log_ratio(latest, base)
    }

    /// The mean of the values held; `None` when empty.
    pub fn mean(&self) -> Option<f64> {
        // Values near `f64::MAX` can sum past it.
        mean(self.iter()).and_then(finite)
    }

    /// The sample standard deviation of the values held (divided by
    /// `n - 1`); `None` with fewer than two.
    pub fn std(&self) -> Option<f64> {
        std(|| self.iter())
    }

    /// The smallest value held; `None` when empty.
    pub fn min(&self) -> Option<f64> {
        self.iter().reduce(f64::min)
    }

    /// The largest value held; `None` when empty.
    pub fn max(&self) -> Option<f64> {
        self.iter().reduce(f64::max)
    }

    /// How many standard deviations the latest value is from the window's
    /// mean. `None` with fewer than two values, or when they are all equal.
    pub fn z_score(&self) -> Option<f64> {
        // Equal values have a rounded mean a hair off each of them, so a
        // standard deviation of 1e-17 instead of 0: test equality itself,
        // or the z-score of a flat window would be a large made-up number.
        if self.min()? == self.max()? {
            return None;
        }
        let (latest, mean, std) = (self.last()?, self.mean()?, self.std()?);
        finite((latest - mean) / std)
    }

    /// The volatility a step: the sample standard deviation of the
    /// one-step log returns between the values held. Not annualised.
    /// `None` with fewer than three values (two returns), or when a value
    /// is not positive.
    pub fn volatility(&self) -> Option<f64> {
        let is_positive = self.iter().all(|value| value > 0.0);
        if !is_positive {
            return None;
        }
        std(|| self.step_log_returns())
    }

    /// The log return between each value and the one before, oldest first.
    /// Only called on positive values.
    fn step_log_returns(&self) -> impl Iterator<Item = f64> + '_ {
        self.iter()
            .zip(self.iter().skip(1))
            .map(|(before, after)| (after / before).ln())
    }
}

/// An exponential moving average: each update moves it towards the value by
/// `2 / (span + 1)` of the gap, the usual N-step EMA. It holds one number.
#[derive(Debug, Clone, Copy)]
pub struct Ema {
    /// The weight of a new value, in (0, 1].
    alpha: f64,
    value: Option<f64>,
}

impl Ema {
    /// An average over about `span_steps` steps, between 1 and
    /// [`SPAN_STEPS_MAX`]. A span of 1 follows the value exactly.
    pub fn new(span_steps: u32) -> Result<Self, FeatureError> {
        check_span(span_steps)?;
        let alpha = 2.0 / (f64::from(span_steps) + 1.0);
        assert!(alpha > 0.0 && alpha <= 1.0, "alpha {alpha} is a weight");
        Ok(Self { alpha, value: None })
    }

    /// Takes this step's value and returns the average after it. The first
    /// value is the average. A value that is not finite is refused and the
    /// average is unchanged.
    pub fn update(&mut self, value: f64) -> Result<f64, FeatureError> {
        if !value.is_finite() {
            return Err(FeatureError::NotFinite);
        }
        // The weighted sum, not `average + alpha * (value - average)`: the
        // gap between two finite values can overflow (-1e308 to 1e308),
        // while a weighted sum of them lies between them.
        let average = match self.value {
            Some(average) => (1.0 - self.alpha) * average + self.alpha * value,
            None => value,
        };
        if !average.is_finite() {
            // Only when rounding takes a sum of values next to `f64::MAX`
            // over it: refuse the value, as for one that is not finite.
            return Err(FeatureError::NotFinite);
        }
        self.value = Some(average);
        Ok(average)
    }

    /// The average so far; `None` before the first value.
    pub fn value(&self) -> Option<f64> {
        self.value
    }
}

/// One bar: the first, highest, lowest and last price of its steps.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bar {
    /// The first price of the bar.
    pub open: f64,
    /// The highest.
    pub high: f64,
    /// The lowest.
    pub low: f64,
    /// The last.
    pub close: f64,
    /// How many prices it holds so far; a closed bar holds its full span.
    pub steps: u32,
}

impl Bar {
    /// A bar of one price.
    fn opened_at(price: f64) -> Self {
        Self {
            open: price,
            high: price,
            low: price,
            close: price,
            steps: 1,
        }
    }

    /// The bar with one more price.
    fn with(self, price: f64) -> Self {
        Self {
            open: self.open,
            high: self.high.max(price),
            low: self.low.min(price),
            close: price,
            steps: self.steps + 1,
        }
    }

    /// High minus low.
    pub fn range(&self) -> f64 {
        self.high - self.low
    }
}

/// Open, high, low and close over every `steps_per_bar` prices pushed. It
/// keeps the last `capacity` closed bars and the one still forming.
#[derive(Debug, Clone)]
pub struct Bars {
    steps_per_bar: u32,
    closed: Ring<Bar>,
    forming: Option<Bar>,
}

impl Bars {
    /// Bars of `steps_per_bar` prices each (between 1 and
    /// [`SPAN_STEPS_MAX`]), keeping the last `capacity` closed ones
    /// (between 1 and [`WINDOW_CAPACITY_MAX`]).
    pub fn new(steps_per_bar: u32, capacity: usize) -> Result<Self, FeatureError> {
        check_span(steps_per_bar)?;
        Ok(Self {
            steps_per_bar,
            closed: Ring::new(capacity)?,
            forming: None,
        })
    }

    /// Adds this step's price. Returns the bar it closed, on the step that
    /// fills one; otherwise `None`. A price that is not finite is refused
    /// and nothing changes: the bar then spans one step more of the
    /// session.
    pub fn push(&mut self, price: f64) -> Result<Option<Bar>, FeatureError> {
        if !price.is_finite() {
            return Err(FeatureError::NotFinite);
        }
        let bar = match self.forming {
            Some(bar) => bar.with(price),
            None => Bar::opened_at(price),
        };
        assert!(bar.low <= bar.high, "a bar's low is under its high");
        assert!(bar.steps <= self.steps_per_bar, "a bar closes when full");
        if bar.steps < self.steps_per_bar {
            self.forming = Some(bar);
            return Ok(None);
        }
        self.forming = None;
        self.closed.push(bar);
        Ok(Some(bar))
    }

    /// How many closed bars it holds.
    pub fn len(&self) -> usize {
        self.closed.len
    }

    /// Whether no bar has closed yet.
    pub fn is_empty(&self) -> bool {
        self.closed.len == 0
    }

    /// The closed bar `lag` bars before the latest closed one: `closed(0)`
    /// is the latest. `None` when that many haven't closed, or it was
    /// forgotten.
    pub fn closed(&self, lag: usize) -> Option<Bar> {
        self.closed.ago(lag)
    }

    /// The bar still forming; `None` right after a bar closed, or before
    /// the first price.
    pub fn forming(&self) -> Option<Bar> {
        self.forming
    }

    /// The closed bars held, oldest first.
    pub fn iter(&self) -> impl Iterator<Item = Bar> + '_ {
        self.closed.iter()
    }
}

/// A fixed-capacity ring: the last `capacity` items pushed.
#[derive(Debug, Clone)]
struct Ring<T> {
    /// Allocated once, to the capacity; never grows.
    slots: Box<[T]>,
    /// Where the next item goes.
    next: usize,
    /// How many slots hold an item.
    len: usize,
}

impl<T: Copy + Default> Ring<T> {
    fn new(capacity: usize) -> Result<Self, FeatureError> {
        if capacity == 0 {
            return Err(FeatureError::CapacityZero);
        }
        if capacity > WINDOW_CAPACITY_MAX {
            return Err(FeatureError::CapacityTooLarge { capacity });
        }
        Ok(Self {
            slots: vec![T::default(); capacity].into_boxed_slice(),
            next: 0,
            len: 0,
        })
    }

    fn capacity(&self) -> usize {
        self.slots.len()
    }

    fn push(&mut self, item: T) {
        assert!(self.next < self.capacity(), "the cursor is a slot");
        self.slots[self.next] = item;
        self.next = (self.next + 1) % self.capacity();
        self.len = (self.len + 1).min(self.capacity());
        assert!(self.len >= 1 && self.len <= self.capacity());
    }

    /// The item `lag` pushes before the latest.
    fn ago(&self, lag: usize) -> Option<T> {
        if lag >= self.len {
            return None;
        }
        // `next` is one past the latest; adding the capacity keeps the
        // subtraction above zero, since lag < len <= capacity.
        let index = (self.next + self.capacity() - 1 - lag) % self.capacity();
        Some(self.slots[index])
    }

    /// The items held, oldest first.
    fn iter(&self) -> impl Iterator<Item = T> + '_ {
        (0..self.len).rev().filter_map(|lag| self.ago(lag))
    }
}

impl Default for Bar {
    /// An empty slot of the ring; never read before a real bar replaces it.
    fn default() -> Self {
        Self {
            open: 0.0,
            high: 0.0,
            low: 0.0,
            close: 0.0,
            steps: 0,
        }
    }
}

fn check_span(span_steps: u32) -> Result<(), FeatureError> {
    if span_steps == 0 || span_steps > SPAN_STEPS_MAX {
        return Err(FeatureError::SpanOutOfRange { span_steps });
    }
    Ok(())
}

/// `ln(value / base)` for two positive numbers.
fn log_ratio(value: f64, base: f64) -> Option<f64> {
    if value > 0.0 && base > 0.0 {
        finite((value / base).ln())
    } else {
        None
    }
}

/// The number, unless a division made it NaN or infinite.
fn finite(value: f64) -> Option<f64> {
    value.is_finite().then_some(value)
}

fn mean(values: impl Iterator<Item = f64>) -> Option<f64> {
    let (count, sum) = values.fold((0u32, 0.0), |(count, sum), value| (count + 1, sum + value));
    (count > 0).then(|| sum / f64::from(count))
}

/// The sample standard deviation, in two passes: the mean, then the squared
/// distances from it, which stays accurate when the values are large and
/// close together, as prices are.
fn std<I: Iterator<Item = f64>>(values: impl Fn() -> I) -> Option<f64> {
    let mean = mean(values())?;
    let (count, squares) = values().fold((0u32, 0.0), |(count, squares), value| {
        (count + 1, squares + (value - mean) * (value - mean))
    });
    if count < 2 {
        return None;
    }
    let variance = squares / f64::from(count - 1);
    assert!(variance >= 0.0, "a sum of squares is not negative");
    finite(variance.sqrt())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window_of(capacity: usize, values: &[f64]) -> Window {
        let mut window = Window::new(capacity).unwrap();
        for value in values {
            window.push(*value).unwrap();
        }
        window
    }

    fn close(left: f64, right: f64) -> bool {
        (left - right).abs() < 1e-12
    }

    #[test]
    fn capacities_and_spans_are_checked() {
        assert_eq!(Window::new(0).unwrap_err(), FeatureError::CapacityZero);
        assert!(Window::new(1).is_ok());
        assert!(Window::new(WINDOW_CAPACITY_MAX).is_ok());
        assert_eq!(
            Window::new(WINDOW_CAPACITY_MAX + 1).unwrap_err(),
            FeatureError::CapacityTooLarge {
                capacity: WINDOW_CAPACITY_MAX + 1
            }
        );
        assert_eq!(
            Ema::new(0).unwrap_err(),
            FeatureError::SpanOutOfRange { span_steps: 0 }
        );
        assert!(Ema::new(SPAN_STEPS_MAX).is_ok());
        assert!(Ema::new(SPAN_STEPS_MAX + 1).is_err());
        assert!(Bars::new(0, 4).is_err());
        assert_eq!(Bars::new(5, 0).unwrap_err(), FeatureError::CapacityZero);
    }

    #[test]
    fn an_empty_window_has_no_features() {
        let window = window_of(8, &[]);
        assert!(window.is_empty());
        assert_eq!(window.len(), 0);
        assert_eq!(window.last(), None);
        assert_eq!(window.ago(0), None);
        assert_eq!(window.simple_return(1), None);
        assert_eq!(window.log_return(1), None);
        assert_eq!(window.mean(), None);
        assert_eq!(window.std(), None);
        assert_eq!(window.min(), None);
        assert_eq!(window.max(), None);
        assert_eq!(window.z_score(), None);
        assert_eq!(window.volatility(), None);
        assert_eq!(window.iter().count(), 0);
    }

    #[test]
    fn one_sample_has_a_mean_and_no_spread() {
        let window = window_of(8, &[100.0]);
        assert_eq!(window.last(), Some(100.0));
        assert_eq!(window.mean(), Some(100.0));
        assert_eq!(window.simple_return(0), Some(0.0));
        assert_eq!(window.simple_return(1), None);
        assert_eq!(window.std(), None);
        assert_eq!(window.z_score(), None);
        assert_eq!(window.volatility(), None);
    }

    #[test]
    fn two_samples_have_a_spread_and_no_volatility() {
        // Two prices are one return: a spread of prices, not yet of returns.
        let window = window_of(8, &[100.0, 102.0]);
        assert!(close(window.std().unwrap(), 2.0_f64.sqrt()));
        assert_eq!(window.volatility(), None);
    }

    #[test]
    fn returns_over_a_lag() {
        let window = window_of(8, &[100.0, 110.0, 121.0]);
        assert!(close(window.simple_return(1).unwrap(), 0.1));
        assert!(close(window.simple_return(2).unwrap(), 0.21));
        assert!(close(window.log_return(2).unwrap(), 1.21_f64.ln()));
        assert_eq!(window.simple_return(3), None);
        assert_eq!(window.log_return(3), None);
    }

    #[test]
    fn returns_refuse_a_base_that_is_not_positive() {
        let window = window_of(8, &[0.0, 5.0]);
        assert_eq!(window.simple_return(1), None);
        assert_eq!(window.log_return(1), None);
        let window = window_of(8, &[-2.0, 5.0]);
        assert!(close(window.simple_return(1).unwrap(), -3.5));
        assert_eq!(window.log_return(1), None);
        assert_eq!(window.volatility(), None);
    }

    #[test]
    fn statistics_match_values_worked_by_hand() {
        let window = window_of(8, &[2.0, 4.0, 4.0, 4.0, 5.0, 5.0, 7.0, 9.0]);
        assert!(close(window.mean().unwrap(), 5.0));
        // Sum of squared distances is 32, over n - 1 = 7.
        let std = (32.0_f64 / 7.0).sqrt();
        assert!(close(window.std().unwrap(), std));
        assert!(close(window.z_score().unwrap(), (9.0 - 5.0) / std));
        assert_eq!(window.min(), Some(2.0));
        assert_eq!(window.max(), Some(9.0));
    }

    #[test]
    fn a_flat_window_has_no_z_score() {
        let window = window_of(4, &[3.0, 3.0, 3.0]);
        assert_eq!(window.std(), Some(0.0));
        assert_eq!(window.z_score(), None);
        assert_eq!(window.volatility(), Some(0.0));
    }

    /// 0.1 three times has a rounded mean a hair off 0.1, so a standard
    /// deviation of about 1e-17: still flat, still no z-score.
    #[test]
    fn a_flat_window_of_inexact_values_has_no_z_score() {
        for count in 2..=7 {
            let window = window_of(8, &vec![0.1; count]);
            assert_eq!(window.z_score(), None, "{count} values of 0.1");
        }
    }

    #[test]
    fn a_mean_too_large_for_an_f64_is_none() {
        let window = window_of(4, &[1e308, 1e308]);
        assert_eq!(window.mean(), None);
        assert_eq!(window.z_score(), None);
    }

    #[test]
    fn volatility_is_the_spread_of_step_log_returns() {
        let window = window_of(8, &[100.0, 110.0, 99.0, 108.9]);
        let returns = [1.1_f64.ln(), 0.9_f64.ln(), 1.1_f64.ln()];
        let mean = returns.iter().sum::<f64>() / 3.0;
        let variance = returns.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / 2.0;
        assert!(close(window.volatility().unwrap(), variance.sqrt()));
    }

    #[test]
    fn a_full_window_forgets_the_oldest() {
        let mut window = window_of(3, &[1.0, 2.0, 3.0]);
        assert!(window.is_full());
        window.push(4.0).unwrap();
        assert_eq!(window.len(), 3);
        assert_eq!(window.capacity(), 3);
        assert_eq!(window.iter().collect::<Vec<_>>(), [2.0, 3.0, 4.0]);
        assert_eq!(window.ago(2), Some(2.0));
        assert_eq!(window.ago(3), None);
        // Wrap many times over: the cursor stays in range, the order holds.
        for value in 5..=100 {
            window.push(f64::from(value)).unwrap();
        }
        assert_eq!(window.iter().collect::<Vec<_>>(), [98.0, 99.0, 100.0]);
        assert_eq!(window.mean(), Some(99.0));
    }

    #[test]
    fn a_window_of_one_holds_the_latest() {
        let mut window = window_of(1, &[1.0, 2.0]);
        assert_eq!(window.iter().collect::<Vec<_>>(), [2.0]);
        window.push(3.0).unwrap();
        assert_eq!(window.last(), Some(3.0));
        assert_eq!(window.ago(1), None);
    }

    #[test]
    fn a_value_that_is_not_finite_is_refused_and_changes_nothing() {
        let mut window = window_of(4, &[1.0, 2.0]);
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(window.push(bad).unwrap_err(), FeatureError::NotFinite);
        }
        assert_eq!(window.iter().collect::<Vec<_>>(), [1.0, 2.0]);

        let mut ema = Ema::new(3).unwrap();
        assert_eq!(ema.update(f64::NAN).unwrap_err(), FeatureError::NotFinite);
        assert_eq!(ema.value(), None);
        ema.update(10.0).unwrap();
        assert!(ema.update(f64::INFINITY).is_err());
        assert_eq!(ema.value(), Some(10.0));

        let mut bars = Bars::new(2, 4).unwrap();
        bars.push(10.0).unwrap();
        assert!(bars.push(f64::NAN).is_err());
        assert_eq!(bars.forming().unwrap().steps, 1);
        assert!(bars.is_empty());
    }

    #[test]
    fn an_ema_starts_at_the_first_value_and_moves_by_alpha() {
        let mut ema = Ema::new(3).unwrap();
        assert_eq!(ema.value(), None);
        assert_eq!(ema.update(10.0).unwrap(), 10.0);
        // Alpha is 2 / (3 + 1): half the gap each update.
        assert_eq!(ema.update(20.0).unwrap(), 15.0);
        assert_eq!(ema.update(20.0).unwrap(), 17.5);
        assert_eq!(ema.value(), Some(17.5));
    }

    /// The gap between these two overflows an `f64`; the average must not.
    #[test]
    fn an_ema_of_values_at_the_ends_of_f64_stays_finite() {
        let mut ema = Ema::new(3).unwrap();
        ema.update(-1e308).unwrap();
        assert_eq!(ema.update(1e308).unwrap(), 0.0);
        let mut ema = Ema::new(3).unwrap();
        ema.update(f64::MAX).unwrap();
        assert_eq!(ema.update(f64::MAX).unwrap(), f64::MAX);
        assert_eq!(ema.update(f64::NAN), Err(FeatureError::NotFinite));
        assert_eq!(ema.value(), Some(f64::MAX));
    }

    #[test]
    fn an_ema_of_span_one_follows_the_value() {
        let mut ema = Ema::new(1).unwrap();
        ema.update(10.0).unwrap();
        assert_eq!(ema.update(42.0).unwrap(), 42.0);
    }

    #[test]
    fn bars_close_every_n_steps() {
        let mut bars = Bars::new(3, 4).unwrap();
        assert_eq!(bars.forming(), None);
        assert_eq!(bars.push(10.0).unwrap(), None);
        assert_eq!(bars.push(12.0).unwrap(), None);
        assert_eq!(bars.forming().unwrap().steps, 2);
        let bar = bars.push(9.0).unwrap().unwrap();
        assert_eq!(
            bar,
            Bar {
                open: 10.0,
                high: 12.0,
                low: 9.0,
                close: 9.0,
                steps: 3
            }
        );
        assert_eq!(bar.range(), 3.0);
        assert_eq!(bars.forming(), None);
        assert_eq!(bars.len(), 1);
        assert_eq!(bars.closed(0), Some(bar));
        assert_eq!(bars.closed(1), None);
        // The next bar opens at its own first price, not the last close.
        bars.push(11.0).unwrap();
        assert_eq!(bars.forming().unwrap().open, 11.0);
    }

    #[test]
    fn bars_of_one_step_close_every_push() {
        let mut bars = Bars::new(1, 2).unwrap();
        let bar = bars.push(5.0).unwrap().unwrap();
        assert_eq!(
            (bar.open, bar.high, bar.low, bar.close),
            (5.0, 5.0, 5.0, 5.0)
        );
        assert_eq!(bars.forming(), None);
    }

    #[test]
    fn closed_bars_are_forgotten_past_the_capacity() {
        let mut bars = Bars::new(1, 2).unwrap();
        for price in [1.0, 2.0, 3.0] {
            bars.push(price).unwrap();
        }
        assert_eq!(bars.len(), 2);
        let closes: Vec<f64> = bars.iter().map(|bar| bar.close).collect();
        assert_eq!(closes, [2.0, 3.0]);
        assert_eq!(bars.closed(0).unwrap().close, 3.0);
        assert_eq!(bars.closed(2), None);
    }
}
