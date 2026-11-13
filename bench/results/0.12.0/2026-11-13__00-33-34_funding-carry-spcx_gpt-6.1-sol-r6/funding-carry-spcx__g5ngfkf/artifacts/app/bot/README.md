SPCX Phoenix strategy, built as a `wasm32-wasip2` component on `mtx-kit`.

Run `just build` to rebuild `bot.wasm`. The component contains its complete forecasting models; it needs no runtime files, environment variables, or network access.

The strategy forecasts returns after funding over six and twelve seconds. It combines premium carry, observed index trends, temporary price dislocations, and evidence from completed episodes. The two regression ensembles were trained on generated funding-carry practice scenarios with additional oracle noise. All runtime features use observations available at the current turn or earlier.

A turn reads Phoenix's funding parameters, the trader account, and maker price centres through one allowed `getMultipleAccounts` call. Equity includes collateral, position value, and unsettled cumulative funding. The order sheet uses only the session's SPCX market.

Ordinary transitions trade at most 40 SPCX per turn, in slices no larger than 7 SPCX. Position targets respect the session cap and shrink as drawdown consumes the risk budget. A loss of half the mandate's drawdown allowance stops the strategy and triggers an immediate exit. No new trades are submitted on the final turn.

`src/features.rs` contains the causal feature state. `src/trees.rs` and `src/short.rs` contain the embedded twelve- and six-second models. `src/lib.rs` implements execution and risk controls.
