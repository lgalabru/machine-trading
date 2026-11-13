# Embedded prediction model

The component forecasts price returns, expressed as percentages of the starting mark, over 1, 3, and 10 seconds. It uses squared-error histogram gradient boosting. Model fitting uses generated price paths; chain runs measure fills, fees, realized equity, drawdown, and the final score independently.

The saved model's training run used 50,000 freshly generated scenarios and 1.8 million sampled observations. Each regressor has 250 boosting rounds, at most 31 leaves per tree, maximum depth 6, minimum leaf size 1,800, learning rate 0.055, and L2 regularization 20. `models.rs` stores thresholds, child indices, leaf values, and intercepts directly. Leaves already include the learning-rate factor.

Every feature uses the current or a previous price. Signed features and labels are oriented to the latest move of at least 0.2%; the prediction is converted back to the market's direction before trading.

| Feature indices | Inputs |
| --- | --- |
| 0 | Price displacement from the session's starting mark |
| 1–8 | Returns over 1, 2, 4, 8, 16, 32, 64, and 128 steps |
| 9–16 | Distance from high and low over 8, 32, 80, and 160 steps |
| 17–20 | Steps since moves of at least 0.2%, 0.5%, 1.5%, and 3% |
| 21 | Latest impulse size |
| 22–23 | Elapsed and remaining market seconds |
| 24–25 | Total variation over 32 and 80 steps |
| 26–35 | Primary shock state: signal, direction, amplitude, retracement, ages, and phase flags |
| 36 | Average signal from four shock state machines |

The forecast weights are 16:16:4. The entry threshold is 0.1 percentage points; an existing position can remain while its forecast has the same sign and exceeds 0.005 percentage points. Risk controls apply after this decision.

Unit tests verify exported numerical predictions and the full history-to-score pipeline against trainer fixtures. Other tests check shock transitions, the latched halt, position caps, and adverse-step headroom. The component reads only its own trader account through `getAccountInfo`, once per turn, and emits only the session's market orders.
