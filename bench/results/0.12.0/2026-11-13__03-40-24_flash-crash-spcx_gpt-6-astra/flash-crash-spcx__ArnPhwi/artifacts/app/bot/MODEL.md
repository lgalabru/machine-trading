# Model provenance

The deployed policy averages two gradient-boosted regression models, each with
280 trees and up to 63 leaves per tree. Their 70 inputs are computed solely from
prices already observed, past move segments, and the allowed session clock.
The runtime does not use scenario names, descriptions, future paths, or seeds.

Training used 28,000 public generated scenarios:

| Generator seed | Scenarios |
| --- | ---: |
| 715928 | 3,000 |
| 551273 | 5,000 |
| 8047435 | 20,000 |

After sorting filenames, every fifth complete scenario was held out: 22,400
training scenarios and 5,600 validation scenarios. No sandbox comparison or
final-validation scenarios were included in training.

The models estimate continuation value in basis points. The one-step model
uses the fitted Bellman target `return + 0.995 * clip(next_value, -10, 10)`;
the two-step model uses the sum of two returns, a 0.99 discount, and continuation
at the second following step. Episode boundaries have zero continuation.
Targets and features are oriented by the most recent significant move.
The retained models use ten and five fitting iterations, respectively.

LightGBM parameters: learning rate 0.07, 63 leaves, 280 boosting rounds per fit,
minimum 500 observations per leaf, feature fraction 0.9, L2 regularization 100,
and 127 histogram bins. All observations with nonzero target-period returns
were retained, along with a random 20% of zero-return observations weighted by
five. The sampling seeds were 51517 and 93171. Evaluation used complete paths,
including every step.

The models are averaged with equal weight. Long/short changes require the
result to cross +10/-10 basis points. Account risk is enforced independently
of the prediction. The built component contains the numeric tree tables and
needs no machine-learning package at runtime.

Offline evaluation approximated trading costs at 10 basis points per share
traded. The saved sandbox reports use actual Phoenix fills, fees, collateral,
and mark valuation instead. `features_golden.rs` checks the compiled model and
feature implementation against independent Python reference outputs.
