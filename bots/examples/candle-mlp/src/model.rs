//! What training and trading share: the features and the network.
//!
//! The `train` binary and the strategy both build features with [`Inputs`]
//! and evaluate [`Mlp`], so the model is served exactly what it was trained
//! on. Change a feature here and train again: the weights only fit the
//! features they were trained with.

use mtx_kit::{
    candle::{self, Module, Tensor},
    candle_nn::{self, Linear, VarBuilder},
    features::{Bar, Bars, Ema, FeatureError, Window},
    nn,
};

/// How many features the network reads.
pub const FEATURE_COUNT: usize = 8;

/// The width of each of the two hidden layers. With 8 features this is
/// 1,377 parameters, 6 KB of weights: enough for a signal this simple.
pub const HIDDEN_WIDTH: usize = 32;

/// How many steps ahead the network predicts the return over.
pub const HORIZON_STEPS: usize = 8;

/// How many marks the window holds. No feature exists before it is full,
/// so the strategy stays flat for its first 32 steps.
const WINDOW_STEPS: usize = 32;

/// The spans of the fast and slow moving averages, in steps.
const EMA_FAST_STEPS: u32 = 4;
const EMA_SLOW_STEPS: u32 = 16;

/// How many steps one bar spans: the horizon, so the last closed bar is
/// the move just before the one predicted.
const BAR_STEPS: u32 = HORIZON_STEPS as u32;

/// How many closed bars are kept; only the latest is read.
const BARS_KEPT: usize = 2;

/// Every feature and the training target are clamped to plus or minus
/// this, in volatilities: one wild step must not push the network far
/// outside what it was trained on.
pub const FEATURE_ABS_MAX: f64 = 8.0;

/// The rolling state the features are computed from, fed one mark a step.
pub struct Inputs {
    /// The last `WINDOW_STEPS` marks.
    marks: Window,
    /// The fast and slow averages of the mark.
    fast: Ema,
    slow: Ema,
    /// The mark's bars of `BAR_STEPS` steps.
    bars: Bars,
}

impl Inputs {
    /// Empty state, at the start of a session or a training path.
    pub fn new() -> Result<Self, FeatureError> {
        Ok(Self {
            marks: Window::new(WINDOW_STEPS)?,
            fast: Ema::new(EMA_FAST_STEPS)?,
            slow: Ema::new(EMA_SLOW_STEPS)?,
            bars: Bars::new(BAR_STEPS, BARS_KEPT)?,
        })
    }

    /// Takes this step's mark. A mark that is not finite is refused before
    /// anything changes, so the four pieces of state never disagree.
    pub fn push(&mut self, mark: f64) -> Result<(), FeatureError> {
        if !mark.is_finite() {
            return Err(FeatureError::NotFinite);
        }
        self.marks.push(mark)?;
        self.fast.update(mark)?;
        self.slow.update(mark)?;
        self.bars.push(mark)?;
        Ok(())
    }

    /// The volatility of the mark a step over the window, when it is full
    /// and the market has moved: what every feature is measured in.
    pub fn volatility(&self) -> Option<f64> {
        if !self.marks.is_full() {
            return None;
        }
        self.marks
            .volatility()
            .filter(|volatility| *volatility > 0.0)
    }

    /// The features for this step, or `None` while the window fills or the
    /// market hasn't moved. Each is a move divided by the volatility, so
    /// the same network reads a calm market and a wild one alike.
    pub fn features(&self) -> Option<[f64; FEATURE_COUNT]> {
        let volatility = self.volatility()?;
        let (fast, slow) = (self.fast.value()?, self.slow.value()?);
        let bar = self.bars.closed(0)?;
        let raw = [
            // The returns over 1, 4 and 16 steps, each over the volatility
            // of that many steps (it grows with the square root of time).
            self.marks.log_return(1)? / volatility,
            self.marks.log_return(4)? / (volatility * 2.0),
            self.marks.log_return(16)? / (volatility * 4.0),
            // How far the mark is from the window's mean, in deviations.
            self.marks.z_score()?,
            // The mark against its fast average, and fast against slow.
            (self.marks.last()? / fast).ln() / volatility,
            (fast / slow).ln() / volatility,
            // Where the last bar closed within its range, and its return.
            close_within_range(bar),
            (bar.close / bar.open).ln() / (volatility * f64::from(BAR_STEPS).sqrt()),
        ];
        let is_finite = raw.iter().all(|feature| feature.is_finite());
        is_finite.then(|| raw.map(|feature| feature.clamp(-FEATURE_ABS_MAX, FEATURE_ABS_MAX)))
    }
}

/// Where a bar closed between its low (-0.5) and its high (0.5); 0 for a
/// bar that didn't move.
fn close_within_range(bar: Bar) -> f64 {
    let range = bar.range();
    if range > 0.0 {
        (bar.close - bar.low) / range - 0.5
    } else {
        0.0
    }
}

/// The network: features, two hidden layers with ReLU, one output, the
/// predicted return over `HORIZON_STEPS` in volatilities of that horizon.
pub struct Mlp {
    hidden1: Linear,
    hidden2: Linear,
    out: Linear,
}

impl Mlp {
    /// The layers, from weights named `hidden1`, `hidden2` and `out`. The
    /// same code builds it for training (weights from a `VarMap`) and for
    /// trading (weights from the embedded file).
    pub fn new(weights: VarBuilder) -> candle::Result<Self> {
        Ok(Self {
            hidden1: candle_nn::linear(FEATURE_COUNT, HIDDEN_WIDTH, weights.pp("hidden1"))?,
            hidden2: candle_nn::linear(HIDDEN_WIDTH, HIDDEN_WIDTH, weights.pp("hidden2"))?,
            out: candle_nn::linear(HIDDEN_WIDTH, 1, weights.pp("out"))?,
        })
    }

    /// The network from safetensors bytes, as the strategy embeds them.
    pub fn from_safetensors(bytes: &[u8]) -> candle::Result<Self> {
        Self::new(nn::var_builder(bytes)?)
    }

    /// The outputs for a batch: `features` is `[rows, FEATURE_COUNT]`, the
    /// result `[rows, 1]`.
    pub fn forward(&self, features: &Tensor) -> candle::Result<Tensor> {
        let hidden = self.hidden1.forward(features)?.relu()?;
        let hidden = self.hidden2.forward(&hidden)?.relu()?;
        self.out.forward(&hidden)
    }

    /// The prediction for one step's features.
    pub fn predict(&self, features: &[f64; FEATURE_COUNT]) -> candle::Result<f64> {
        // The network computes in f32, as it was trained.
        let row = features.map(|feature| feature as f32);
        let input = Tensor::from_slice(&row, (1, FEATURE_COUNT), &nn::DEVICE)?;
        let output = self.forward(&input)?.flatten_all()?.to_vec1::<f32>()?;
        match output.as_slice() {
            [prediction] if prediction.is_finite() => Ok(f64::from(*prediction)),
            _ => candle::bail!("the network's output is not one finite number"),
        }
    }
}
