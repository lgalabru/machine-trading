//! An example: a strategy that trades on a small neural network.
//!
//! The network (`src/model.rs`: 8 features, two hidden layers of 32, one
//! output) predicts the return over the next 8 steps from the mark's recent
//! path. `just train` trains it with candle on synthetic prices and writes
//! `model.safetensors`; this strategy embeds that file, builds the same
//! features every step with `mtx_kit::features`, and holds a position in
//! proportion to the prediction.
//!
//! ```sh
//! just --justfile bots/examples/candle-mlp/Justfile train   # model.safetensors
//! just --justfile bots/examples/candle-mlp/Justfile build   # bot.wasm
//! mtx bot practice --bot bots/examples/candle-mlp
//! ```
//!
//! It shows the plumbing, not an edge: the network learned a made-up
//! market, and a real session owes it nothing. Train yours on scenarios
//! (`mtx scenario generate`). The guide is `specs/guides/bots/models.md`.

mod model;

use mtx_kit::prelude::*;

use crate::model::{Inputs, Mlp};

/// The trained weights, inside the component: a strategy has no files.
static WEIGHTS: &[u8] = include_bytes!("../model.safetensors");

/// The prediction at which the strategy holds the whole cap, in
/// volatilities of the horizon. On held-out training paths the predictions
/// have a standard deviation of 0.32 (`just train` prints it), so the cap
/// is reached about one step in eight.
const PREDICTION_AT_CAP: f64 = 0.5;

/// The smallest change of target worth trading, as a share of the cap:
/// following every flicker of the prediction would pay the spread each
/// step.
const REBALANCE_SHARE_MIN: f64 = 0.2;

/// The strategy: the network and the rolling state its features need. Both
/// are made in `start`, which can fail; before it they are `None`.
#[derive(Default)]
struct CandleMlp {
    model: Option<Mlp>,
    inputs: Option<Inputs>,
}

impl CandleMlp {
    /// Builds the network from the embedded weights, and empty features.
    /// It reads only those bytes: no file, clock or network.
    fn load(&mut self) -> BotResult {
        self.model = Some(Mlp::from_safetensors(WEIGHTS)?);
        self.inputs = Some(Inputs::new()?);
        Ok(())
    }
}

impl Strategy for CandleMlp {
    fn start(&mut self, session: &Session) -> BotResult {
        // An error here is logged, and every step then reports it again.
        self.load()?;
        println!("loaded the network for {}", session.symbol);
        Ok(())
    }

    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        let (Some(model), Some(inputs)) = (&self.model, &mut self.inputs) else {
            return Err("start did not load the network".into());
        };
        // Feed the mark first: this step's features include this step.
        inputs.push(market.mark)?;
        // While the window fills there is no prediction: stay as we are.
        let Some(features) = inputs.features() else {
            return Ok(());
        };
        let prediction = model.predict(&features)?;
        // The share of the cap to hold: long when a rise is predicted.
        let share = (prediction / PREDICTION_AT_CAP).clamp(-1.0, 1.0);
        let target = market.cap * share;
        // Sized from the cap, so it works on a market of any unit size.
        let change = (target - market.position.size).abs();
        if change >= market.cap * REBALANCE_SHARE_MIN {
            println!("predicted {prediction:.3}: target {target:.2}");
            orders.target_position(target);
        }
        Ok(())
    }
}

mtx_kit::export!(CandleMlp);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{FEATURE_ABS_MAX, FEATURE_COUNT};

    /// A market at `mark`, flat, with a cap of 70.
    fn market_at(mark: f64) -> Market {
        Market {
            mark,
            cap: 70.0,
            ..Market::default()
        }
    }

    /// A strategy with its network loaded, as `start` leaves it.
    fn started() -> CandleMlp {
        let mut bot = CandleMlp::default();
        bot.load().unwrap();
        bot
    }

    /// A deterministic path that wobbles around 100.
    fn wobble(step: u32) -> f64 {
        100.0 + (f64::from(step) * 0.7).sin() + 0.5 * (f64::from(step) * 0.13).cos()
    }

    #[test]
    fn the_embedded_weights_fit_the_network() {
        let model = Mlp::from_safetensors(WEIGHTS).unwrap();
        let prediction = model.predict(&[0.0; FEATURE_COUNT]).unwrap();
        assert!(prediction.is_finite());
        // At the clamp on every feature, the output is still a number.
        assert!(model.predict(&[FEATURE_ABS_MAX; FEATURE_COUNT]).is_ok());
        assert!(model.predict(&[-FEATURE_ABS_MAX; FEATURE_COUNT]).is_ok());
    }

    #[test]
    fn it_stays_flat_while_the_window_fills() {
        let mut bot = started();
        for step in 0..31 {
            let intents = step_offline(&mut bot, &market_at(wobble(step))).unwrap();
            assert!(intents.is_empty(), "step {step} traded without features");
        }
    }

    #[test]
    fn it_stays_flat_on_a_market_that_does_not_move() {
        let mut bot = started();
        for _ in 0..100 {
            assert!(
                step_offline(&mut bot, &market_at(100.0))
                    .unwrap()
                    .is_empty()
            );
        }
    }

    #[test]
    fn its_targets_stay_within_the_cap() {
        let mut bot = started();
        let mut targets = 0;
        for step in 0..300 {
            let market = market_at(wobble(step));
            for intent in step_offline(&mut bot, &market).unwrap() {
                let Intent::Target { position, .. } = intent else {
                    panic!("only targets are sent, not {intent:?}");
                };
                assert!(position.abs() <= market.cap);
                targets += 1;
            }
        }
        assert!(targets > 0, "the network never asked for a position");
    }

    #[test]
    fn a_mark_that_is_not_finite_skips_the_step_and_changes_nothing() {
        let (mut bot, mut twin) = (started(), started());
        for step in 0..40 {
            step_offline(&mut bot, &market_at(wobble(step))).unwrap();
            step_offline(&mut twin, &market_at(wobble(step))).unwrap();
        }
        assert!(step_offline(&mut bot, &market_at(f64::NAN)).is_err());
        // The bad mark left no trace: both now see the same features.
        let features = |bot: &CandleMlp| bot.inputs.as_ref().unwrap().features();
        assert_eq!(features(&bot), features(&twin));
    }

    #[test]
    fn step_before_start_is_an_error_not_a_panic() {
        let mut bot = CandleMlp::default();
        assert!(step_offline(&mut bot, &market_at(100.0)).is_err());
    }

    #[test]
    fn features_depend_only_on_the_past() {
        // The features at step 60 are the same whatever comes after: feed
        // one copy the path, and another the path then a crash.
        let features_at_60 = |later: &[f64]| {
            let mut inputs = Inputs::new().unwrap();
            let mut seen = None;
            for step in 0..=60 {
                inputs.push(wobble(step)).unwrap();
                seen = inputs.features();
            }
            for mark in later {
                inputs.push(*mark).unwrap();
            }
            seen.unwrap()
        };
        assert_eq!(features_at_60(&[]), features_at_60(&[50.0, 10.0]));
    }
}
