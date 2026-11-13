# SPCX shock strategy

`bot.wasm` is the built `wasm32-wasip2` strategy component. All inference runs inside the component; it needs no files, network, Python, or external model service.

The strategy uses the oracle to distinguish price moves from changes caused by its own fills. Three embedded tree regressors forecast returns over 1, 3, and 10 seconds. Their 37 inputs describe recent returns, ranges, variation, impulse ages, time remaining, and four independent shock/rebound state machines. Predictions are normalized to the latest impulse direction so crashes and squeezes use the same model. An entry threshold and a lower holding threshold limit unnecessary turnover.

Orders trade only the session's SPCX market. Targets stay within the mandate's cap and execute in slices of at most 10 shares. The strategy never leaves resting orders. A read of its Phoenix trader account includes realized trading costs in equity. Position size falls as drawdown rises; a $550 drawdown latches a halt and requests a flat position. The sizing calculation reserves room for another large adverse price step and execution costs.

Build with `just build`. Run unit tests with `just test`, and chain tests with `mtx-practice`. Reports for the saved component are in `results/`. The generated model arrays are in `src/models.rs`; numerical fixtures check inference against the trainer.

Models were trained on independently generated scenarios from the supplied family. Practice and chain validation scenarios were excluded from model fitting.
