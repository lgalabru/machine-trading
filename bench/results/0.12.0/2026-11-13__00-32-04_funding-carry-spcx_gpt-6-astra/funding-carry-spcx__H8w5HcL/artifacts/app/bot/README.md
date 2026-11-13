# SPCX strategy component

`bot.wasm` is the ready-to-run `wasm32-wasip2` component. Rebuild with `just build`; the Phoenix SDK is supplied at `/opt/mtx/sdk`.

The strategy forecasts the book's return after funding, using an equal blend of four-second and eight-second gradient-boosted models. Its causal features track premiums, oracle movement, volatility, episode age, transient jumps, and evidence from completed episodes. The static models were fitted on 7,600 generated practice-family paths, including augmented oracle noise, with a separate 400-path validation set. No scenario path, future price, external service, file, or environment variable is read at runtime.

Each turn reads Phoenix's PerpAssetMap and the trader PDA with `getMultipleAccounts`. The session's on-chain funding period and cap feed the forecast. Equity includes collateral, unrealized PnL, and unsettled cumulative funding. Exposure shrinks as drawdown increases; a stop at 60% of the desk's drawdown allowance latches for the rest of the session. Targets never exceed the session's SPCX cap.

A 0.15% forecast hurdle and 0.10% exit hysteresis limit turnover. Ordinary adjustments are paced at 35 shares per turn; strong signals can move the full position. Transactions are sliced into at most 10 shares. There are no resting orders.

Source layout:

- `src/lib.rs`: account reads, portfolio risk, target selection, execution.
- `src/features.rs`: causal feature state and normalization.
- `src/predict.rs`: self-contained static forecast trees.

Validation results below are from public practice and generated/augmented scenarios, not the hidden grader sessions.

Execution limits use the current best bid and ask. The forecast horizon shortens near the known session end, and the last turn sends no trades.

| Set | Sessions | PnL (USDC) | Maximum drawdown | Profitable |
|---|---:|---:|---:|---:|
| Supplied practice | 6 | 2398.08 | 139.12 | 6 |
| Generated validation | 36 | 22069.08 | 236.26 | 36 |
| Fresh validation | 30 | 19253.11 | 220.53 | 30 |
| Total | 72 | 43720.27 | 236.26 | 72 |

All sessions completed without disqualification, refused RPCs, position-cap violations, other-market exposure, or liquidatable accounts. Detailed reports are in `validation/`.

Three offline tests cover the terminal turn, the persistent drawdown stop, and current-book execution within the position cap (`cargo test --lib`). Feature and forecast parity were also checked against the training implementation over three complete sessions.
