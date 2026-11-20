//! Trains the strategy's network and writes `model.safetensors`.
//!
//! ```sh
//! just train          # or: cargo run --release --bin train [-- OUT]
//! just build          # the strategy, with the new weights inside
//! ```
//!
//! The prices are synthetic: a random walk with a mean-reverting wobble on
//! top, so there is something to learn (after a move away from the trend,
//! part of it comes back). Real training would read prices from scenarios
//! `mtx scenario generate` draws; the shape of the program is the same.
//!
//! Features come from the strategy's own [`Inputs`], fed one price at a
//! time as the grader would: training and trading cannot drift apart. The
//! target at step `t` is the return from `t` to `t + HORIZON_STEPS`, which
//! the features at `t` never see.
//!
//! Every random number comes from one seeded generator, so the same
//! machine writes the same file every run.

use std::{collections::HashMap, error::Error, path::PathBuf};

use model::{FEATURE_ABS_MAX, FEATURE_COUNT, HORIZON_STEPS, Inputs, Mlp};
use mtx_kit::{
    candle::{self, DType, Tensor},
    candle_nn::{self, AdamW, Optimizer, ParamsAdamW, VarBuilder, VarMap},
    nn,
};

/// The strategy's own features and network, compiled into this binary too.
mod model;

/// The seed of every random draw: the paths, then the starting weights.
const SEED: u64 = 7;

/// How many price paths are drawn, and how many steps each runs: 96 paths
/// of 600 steps give about 54,000 samples in all, forty for every
/// parameter of the network.
const PATH_COUNT: usize = 96;
const PATH_STEPS: usize = 600;

/// One path in five is held out, to see whether the network learned the
/// market or the training set.
const HELD_OUT_EVERY: usize = 5;

/// The volatility a step of a path, drawn between these: from a calm
/// market (0.03% a step) to a wild one (0.3%).
const VOLATILITY_STEP_MIN: f64 = 0.0003;
const VOLATILITY_STEP_MAX: f64 = 0.003;

/// How much of the wobble is left a step later. At 0.9, over half of it
/// is gone in 8 steps: the horizon the network predicts over.
const WOBBLE_PERSISTENCE: f64 = 0.9;

/// The sizes of a step's trend move and wobble move, in path volatilities.
const TREND_SHARE: f64 = 0.3;
const WOBBLE_SHARE: f64 = 1.0;

/// Passes over the whole training set, and the step size of each. The loss
/// is flat well before 400 passes at this rate.
const EPOCH_COUNT: usize = 400;
const LEARNING_RATE: f64 = 0.01;

/// The loss is printed every this many passes.
const REPORT_EVERY: usize = 50;

fn main() -> Result<(), Box<dyn Error>> {
    let out = match std::env::args().nth(1) {
        Some(path) => PathBuf::from(path),
        None => PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("model.safetensors"),
    };
    let mut random = Random::new(SEED);
    let (training, held_out) = draw_samples(&mut random)?;
    println!(
        "{} training samples, {} held out, from {PATH_COUNT} paths",
        training.len(),
        held_out.len()
    );

    let weights = VarMap::new();
    let model = Mlp::new(VarBuilder::from_varmap(&weights, DType::F32, &nn::DEVICE))?;
    seed_weights(&weights, &mut random)?;
    train(&model, &weights, &training)?;

    let (loss, spread) = loss_and_spread(&model, &held_out)?;
    let loss_of_zero = held_out.loss_of_zero();
    println!(
        "held out: loss {loss:.4} against {loss_of_zero:.4} for predicting no move ({:.1}% explained)",
        (1.0 - loss / loss_of_zero) * 100.0
    );
    println!("held out: predictions have a standard deviation of {spread:.3}");
    save(&weights, &out)?;
    check_saved(&model, &held_out, &out)?;
    println!("wrote {}", out.display());
    Ok(())
}

/// Features and targets, row for row.
#[derive(Default)]
struct Samples {
    features: Vec<f32>,
    targets: Vec<f32>,
}

impl Samples {
    fn len(&self) -> usize {
        self.targets.len()
    }

    fn tensors(&self) -> candle::Result<(Tensor, Tensor)> {
        assert_eq!(self.features.len(), self.len() * FEATURE_COUNT);
        let features =
            Tensor::from_slice(&self.features, (self.len(), FEATURE_COUNT), &nn::DEVICE)?;
        let targets = Tensor::from_slice(&self.targets, (self.len(), 1), &nn::DEVICE)?;
        Ok((features, targets))
    }

    /// The mean squared target: the loss of always predicting no move.
    fn loss_of_zero(&self) -> f64 {
        let squares: f64 = self.targets.iter().map(|y| f64::from(*y).powi(2)).sum();
        squares / self.len() as f64
    }
}

/// Draws the paths and turns each into samples: most for training, one in
/// `HELD_OUT_EVERY` held out.
fn draw_samples(random: &mut Random) -> Result<(Samples, Samples), Box<dyn Error>> {
    let (mut training, mut held_out) = (Samples::default(), Samples::default());
    for path in 0..PATH_COUNT {
        let prices = draw_path(random);
        let is_held_out = path % HELD_OUT_EVERY == HELD_OUT_EVERY - 1;
        let samples = if is_held_out {
            &mut held_out
        } else {
            &mut training
        };
        add_samples(&prices, samples)?;
    }
    assert!(training.len() > 0 && held_out.len() > 0);
    Ok((training, held_out))
}

/// One path of `PATH_STEPS` prices from 100: a trend that wanders, and a
/// wobble around it that fades.
fn draw_path(random: &mut Random) -> Vec<f64> {
    // Log-uniform, so calm and wild paths are equally common.
    let spread = (VOLATILITY_STEP_MAX / VOLATILITY_STEP_MIN).ln();
    let volatility = VOLATILITY_STEP_MIN * (random.uniform() * spread).exp();
    let (mut trend, mut wobble) = (100.0_f64.ln(), 0.0);
    let mut prices = Vec::with_capacity(PATH_STEPS);
    for _ in 0..PATH_STEPS {
        trend += random.normal() * volatility * TREND_SHARE;
        wobble = wobble * WOBBLE_PERSISTENCE + random.normal() * volatility * WOBBLE_SHARE;
        prices.push((trend + wobble).exp());
    }
    prices
}

/// Walks one path as the grader would walk a session, a price a step, and
/// records the features at each step with the return that followed.
fn add_samples(prices: &[f64], samples: &mut Samples) -> Result<(), Box<dyn Error>> {
    let mut inputs = Inputs::new()?;
    for (step, price) in prices.iter().enumerate() {
        inputs.push(*price)?;
        // The target needs the price `HORIZON_STEPS` later: the last steps
        // of a path have none.
        let Some(later) = prices.get(step + HORIZON_STEPS) else {
            break;
        };
        let (Some(features), Some(volatility)) = (inputs.features(), inputs.volatility()) else {
            continue;
        };
        // In volatilities of the horizon, like the features.
        let scale = volatility * (HORIZON_STEPS as f64).sqrt();
        let target = ((later / price).ln() / scale).clamp(-FEATURE_ABS_MAX, FEATURE_ABS_MAX);
        samples.features.extend(features.map(|x| x as f32));
        samples.targets.push(target as f32);
    }
    Ok(())
}

/// Sets every weight from the seeded generator, in name order, so a run
/// doesn't depend on candle's own unseeded draws: weights uniform within
/// the bound usual before a ReLU, biases at zero.
fn seed_weights(weights: &VarMap, random: &mut Random) -> candle::Result<()> {
    let mut variables: Vec<_> = {
        let data = weights
            .data()
            .lock()
            .expect("no other thread holds the weights");
        data.iter()
            .map(|(name, var)| (name.clone(), var.clone()))
            .collect()
    };
    variables.sort_by(|left, right| left.0.cmp(&right.0));
    for (_name, variable) in variables {
        let dims = variable.dims().to_vec();
        let values: Vec<f32> = match dims.as_slice() {
            [_outputs, inputs] => {
                let bound = (6.0 / *inputs as f64).sqrt();
                let count = dims.iter().product();
                (0..count)
                    .map(|_| ((random.uniform() * 2.0 - 1.0) * bound) as f32)
                    .collect()
            }
            [outputs] => vec![0.0; *outputs],
            _ => candle::bail!("a weight of shape {dims:?} is neither a matrix nor a bias"),
        };
        variable.set(&Tensor::from_vec(values, dims, &nn::DEVICE)?)?;
    }
    Ok(())
}

/// Fits the network to the training samples: the whole set every pass.
fn train(model: &Mlp, weights: &VarMap, samples: &Samples) -> candle::Result<()> {
    let (features, targets) = samples.tensors()?;
    let params = ParamsAdamW {
        lr: LEARNING_RATE,
        ..ParamsAdamW::default()
    };
    let mut optimizer = AdamW::new(weights.all_vars(), params)?;
    for epoch in 1..=EPOCH_COUNT {
        let loss = candle_nn::loss::mse(&model.forward(&features)?, &targets)?;
        optimizer.backward_step(&loss)?;
        if epoch % REPORT_EVERY == 0 {
            println!("pass {epoch}: loss {:.4}", loss.to_scalar::<f32>()?);
        }
    }
    Ok(())
}

/// The model's mean squared error on samples, and the standard deviation
/// of its predictions: how large a prediction the strategy should expect.
fn loss_and_spread(model: &Mlp, samples: &Samples) -> candle::Result<(f64, f64)> {
    let (features, targets) = samples.tensors()?;
    let predictions = model.forward(&features)?;
    let loss = candle_nn::loss::mse(&predictions, &targets)?.to_scalar::<f32>()?;
    let spread = predictions
        .flatten_all()?
        .var(0)?
        .sqrt()?
        .to_scalar::<f32>()?;
    Ok((f64::from(loss), f64::from(spread)))
}

/// Writes the weights as safetensors, under the names the layers read.
fn save(weights: &VarMap, out: &std::path::Path) -> candle::Result<()> {
    let tensors: HashMap<String, Tensor> = {
        let data = weights
            .data()
            .lock()
            .expect("no other thread holds the weights");
        data.iter()
            .map(|(name, var)| (name.clone(), var.as_tensor().clone()))
            .collect()
    };
    assert_eq!(tensors.len(), 6, "three layers, a weight and a bias each");
    candle::safetensors::save(&tensors, out)
}

/// Reads the file back the way the strategy will, from its bytes, and
/// checks it predicts what the trained network does: what was written is
/// what was trained.
fn check_saved(
    trained: &Mlp,
    samples: &Samples,
    out: &std::path::Path,
) -> Result<(), Box<dyn Error>> {
    let saved = Mlp::from_safetensors(&std::fs::read(out)?)?;
    let mut row = [0.0; FEATURE_COUNT];
    for (feature, value) in row.iter_mut().zip(&samples.features) {
        *feature = f64::from(*value);
    }
    let (written, read) = (trained.predict(&row)?, saved.predict(&row)?);
    assert_eq!(
        written, read,
        "the saved weights differ from the trained ones"
    );
    Ok(())
}

/// A seeded generator (SplitMix64): a few lines, and the same numbers on
/// every machine.
struct Random {
    state: u64,
}

impl Random {
    fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut mixed = self.state;
        mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        mixed ^ (mixed >> 31)
    }

    /// Uniform in [0, 1): the top 53 bits, as many as an f64 holds.
    fn uniform(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Standard normal, by Box-Muller.
    fn normal(&mut self) -> f64 {
        // 1 - uniform is in (0, 1], so its logarithm is finite.
        let radius = (-2.0 * (1.0 - self.uniform()).ln()).sqrt();
        radius * (std::f64::consts::TAU * self.uniform()).cos()
    }
}
