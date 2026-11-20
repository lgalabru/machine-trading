# Guide: a strategy with a trained model

Status: guide

A strategy may carry a model: trees or a small neural network, trained
before the session and built into `bot.wasm`. Nothing in the bench asks for
one, and nothing forbids it. This guide shows how to train one offline, how
to embed it, what it costs to run, and where such strategies go wrong. It
assumes the [guide to writing a bot](writing-a-bot.md).

`mtx-kit` has three helpers for it. None changes what a strategy is: use
one, all, or your own code.

:::glance
- **Features:** `mtx_kit::features`, rolling windows fed one value a step
- **Trees:** `mtx_kit::trees`, `Trees::from_bytes(include_bytes!("../model.bin"))`
- **Networks:** the `candle` feature, with weights embedded as safetensors
- **You train:** offline, on scenarios `mtx scenario generate` draws
- **It costs:** under a million instructions a step, of two billion
- **It breaks on:** features that differ between training and trading
:::

## Trees or a network

Both turn a row of features into a number. Pick by the data you have.

| | Trees (`mtx_kit::trees`) | A small network (`candle`) |
| --- | --- | --- |
| Good at | Tables of hand-made features, thresholds, regimes | Smooth functions of many inputs, sequences |
| Needs | Hundreds of sessions | Thousands of sessions, and scaled features |
| Trained with | scikit-learn or LightGBM, in Python | candle in Rust, or PyTorch |
| Embedded as | `model.bin`, 32 bytes a node | `model.safetensors`, 4 bytes a weight |
| Extra dependency | None | `candle-core`, `candle-nn` (about 1 MB of component) |

Start with trees. They need no feature scaling, they train in seconds, and
they are hard to get wrong. Reach for a network when you can say what the
trees cannot express.

## Train offline

A session gives a strategy no files and no network, so the model is trained
before, on your machine, and shipped inside the component.

1. **Draw sessions.** `mtx scenario generate --out more --prefix s --count
   500 --seed 1` draws 500 scenarios of a task's family (`--family`). Keep
   another seed's set aside for testing.
2. **Record what `step` sees.** Run a strategy that prints its features every
   step: `mtx bot practice more/*.json` (in a task's container,
   `mtx-practice more/*.json`). Each session's log is in
   `target/mtx-practice-logs` (there, `/tmp/mtx-practice-logs`). This is the
   surest training data, because the strategy's own code made it. A scenario
   file also holds its scripted path (`path`: a `slot` and a `level_pct`
   from the starting mark), if you would sooner simulate than replay.
3. **Choose a target.** The return over the next N steps, for example. The
   target may read the future. The features may not.
4. **Fit, and test on sessions the model has not seen.** Split by session,
   not by step: neighbouring steps of one session are nearly the same row.

## Features

`mtx_kit::features` keeps the history a strategy needs, in fixed memory.
Each type is made with a capacity and forgets its oldest value.

| Type | Feed it | Read from it |
| --- | --- | --- |
| `Window` | `push(value)` | `simple_return(lag)`, `log_return(lag)`, `mean()`, `std()`, `z_score()`, `volatility()`, `min()`, `max()`, `ago(lag)` |
| `Ema` | `update(value)` | `value()`: the exponential moving average |
| `Bars` | `push(price)` | `closed(lag)`: a `Bar` with `open`, `high`, `low`, `close`; `forming()` |

```rust
use mtx_kit::features::Window;

let mut marks = Window::new(64)?;        // at most 64 marks
marks.push(market.mark)?;                // every step
let momentum = marks.log_return(16);     // None until 17 marks are in
let stretch = marks.z_score();           // None on a flat window
```

A value that is not finite is refused, and the window is unchanged. A
feature that cannot be computed yet is `None`. Decide what the strategy does
then (the example stays flat), and do the same when you build training rows.

## Embed trees

Export the fitted model with `crates/kit/tools/export_trees.py` (in the task
image: `/opt/mtx/sdk/crates/kit/tools/export_trees.py`):

```python
from export_trees import export, check

model = HistGradientBoostingRegressor().fit(X.astype(np.float32), y)
data = export(model)          # raises Unsupported, never exports wrongly
check(model, data, X)         # the bytes predict as the model does
open("model.bin", "wb").write(data)
```

It takes a scikit-learn `HistGradientBoostingRegressor` or a LightGBM
regression model. It refuses categorical splits, missing-value routing,
losses with a link (`poisson`, `gamma`), classifiers, linear trees and
random-forest averaging.

```rust
use mtx_kit::trees::Trees;

let trees = Trees::from_bytes(include_bytes!("../model.bin"))?;   // in start
let prediction = trees.predict(&features)?;                       // in step
```

`Trees::from_bytes` checks the whole file and returns an error on bad bytes.
`predict` returns an error on a slice shorter than `feature_count()` and on
a NaN feature. Return those errors from `start` and `step`: an error skips a
step, a panic ends the strategy.

`predict` rounds each feature to `f32` before it compares it to a threshold.
Train on `float32` features (`X.astype(np.float32)`), and the Rust
prediction equals the Python one. The format, and this rule, are in the
module's documentation.

## Embed a network

Turn the feature on in the bot's `Cargo.toml`:

```toml
mtx-kit = { path = "...", default-features = false, features = ["component", "venue", "candle"] }
```

`mtx_kit::candle` and `mtx_kit::candle_nn` are candle's crates, CPU only.
`mtx_kit::nn::var_builder` reads weights from safetensors bytes:

```rust
use mtx_kit::{candle::Module, candle_nn, nn};

let weights = nn::var_builder(include_bytes!("../model.safetensors"))?;
let hidden = candle_nn::linear(8, 32, weights.pp("hidden"))?;     // in start
let out = candle_nn::linear(32, 1, weights.pp("out"))?;
let prediction = out.forward(&hidden.forward(&features)?.relu()?)?;   // in step
```

The file holds one tensor a name: `hidden.weight` (shape `[32, 8]`, as
PyTorch stores it) and `hidden.bias`. Write it from candle with
`candle::safetensors::save`, or from PyTorch with
`safetensors.torch.save_file(model.state_dict(), "model.safetensors")`.

Loading embedded weights needs no file, clock or network, so it works in the
grader's sandbox. What reads a path does not: `VarMap::load`,
`VarBuilder::from_mmaped_safetensors`, `candle::safetensors::load`.

`bots/examples/candle-mlp` is the whole loop in one crate: `just train`
trains an 8-32-32-1 network with candle on synthetic prices and writes
`model.safetensors`, and `just build` embeds it. The training binary
compiles the strategy's own `src/model.rs`, so both sides share one feature
code and one network. It shows the plumbing, not an edge.

## What it costs

A call to `start`, `step` or `end` may run ten billion WebAssembly
instructions, and a session two billion a step in all. Measured with `mtx
grade` on two generated flash-crash sessions (272 and 360 steps), features
included, in millions of instructions:

| Strategy | Largest call | All calls, a step | `bot.wasm` |
| --- | --- | --- | --- |
| The starter, which prints a line | 0.05 | 0.01 | 0.3 MB |
| Network, 8-32-32-1, 8 features (`bots/examples/candle-mlp`) | 0.13 | 0.09 | 1.2 MB |
| Network, 8-64-64-64-1, weights drawn at random in `start` | 4.6 | 0.22 | 1.2 MB |
| 280 trees, 17,080 nodes, 8 features | 5.1 | 0.10 | 0.9 MB |
| 1,900 trees, 188,082 nodes, 46 features | 52 | 0.73 | 6.3 MB |
| **The budget** | **10,000** | **2,000** | |

The largest call is `turns.fuel_max_call` in the grader's report, and the
last column but one is `turns.fuel_used` over the session's steps (`mtx bot
practice --json`). For trees the largest call is `start`, where
`Trees::from_bytes` checks every node, once; their steps alone cost 0.08
and 0.55 million. Drawing a network's weights at random works in the
sandbox too (the randomness is the session's), though a strategy has no
use for an untrained network.

The largest model here spends 0.04% of a step's budget. The budget is not
what limits a model: the data is.

## Where it goes wrong

- **Training and trading compute different features.** This is the usual
  failure, and it is silent: the model still returns a number. Build
  training rows with the strategy's own Rust code, as above. If you compute
  features in Python too, print a session's rows from both and compare them
  to the last digit.
- **A feature reads the future.** A rolling mean centred on the step, a
  scaler fitted on the whole session, a label shifted the wrong way. In
  training it looks like skill. Feed features one step at a time, in order,
  and nothing can leak.
- **The first steps.** A window of 64 has no value for 63 steps. Decide what
  the strategy does until then, and drop or fill those rows the same way in
  training.
- **`f32` and `f64`.** Trees compare features as `f32`. A network computes
  in `f32`. Features that are returns and ratios keep their meaning. Raw
  prices next to a threshold do not.
- **NaN.** `Trees::predict` refuses it, and a network turns it into a NaN
  prediction. Check the prediction before sizing an order from it.
- **The model only knows what it was shown.** A session hands the strategy
  `start`, `step` and `rpc`, and nothing else. A model trained on one task's
  family has seen that family's markets. Size from `market.cap`, keep the
  mandate's drawdown in mind, and let the strategy stay flat when the
  features are outside what the model was trained on.
- **Overfitting.** Five hundred sessions are few. Prefer a small model, test
  on sessions drawn with another seed, and trust the grade on those over the
  training loss.

## Glossary

Every term this guide uses is in the [glossary](../glossary.md). In the text,
an underlined term shows its definition when you hover it or tap it.
