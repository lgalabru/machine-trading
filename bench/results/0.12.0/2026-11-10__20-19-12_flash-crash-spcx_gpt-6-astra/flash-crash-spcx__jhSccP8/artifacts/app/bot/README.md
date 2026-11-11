# SPCX shock bot

`run.sh` starts the prebuilt native executable `bin/bot`. It needs only the supplied `MTX_*` contract variables, standard Solana RPC, and the system C runtime. It does not build, download models, or write into the bot directory.

The strategy combines forecasts over 0.5, 2, 4, 8, 16, and 32 seconds. Its 58 features use observed oracle prices, past impulses and extrema, elapsed time, and time remaining. Signals are normalized by the latest impulse direction so crashes and squeezes share a model. A threshold keeps the position stable until the opposite forecast is strong enough.

The frozen gradient-boosted models were fitted to synthetic paths from 25,000 generated scenarios and evaluated on a separate 5,000. `src/forest.bin` is embedded in the executable at compile time. No scenario paths are available to the running strategy. The parity test checks Rust features and forecasts against the training implementation across 540 synthetic steps.

Orders are immediate-or-cancel market orders in slices of at most 10 SPCX, bounded by the mandate's position cap. There are no resting orders. Every turn also reads actual Phoenix collateral and unrealized PnL. Position size reserves $150 plus room for a 4.5% adverse next-step price move. A drawdown above 60% of the mandate's limit permanently flattens the strategy for that session.

To rebuild in this development environment, run `./build.sh`. The build replaces `bin/bot` atomically only after compilation and copying succeed. To run the numerical parity test, use `cargo test --release` with the SDK available.

Validation results are recorded in `validation.json`.

Final validation: 222 ordinary sessions and 24 stress sessions completed without disqualifications, refused RPC calls, cap breaches, liquidations, or turn overruns. The maximum drawdown across all sessions was $786.01.

The final 96-scenario holdout earned $32238.83, or 25.26% of its hindsight bound. Across all 222 ordinary sessions, PnL was $91248.20 (29.93% of the combined bound).
