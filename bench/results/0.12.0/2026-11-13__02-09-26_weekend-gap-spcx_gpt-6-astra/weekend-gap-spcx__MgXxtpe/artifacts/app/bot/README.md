# Phoenix SPCX weekend strategy

`bot.wasm` is the ready-to-run `wasm32-wasip2` strategy component.

## Strategy

- Reads SPCX transactions at `confirmed` commitment and decodes Phoenix market orders. Tracks each external taker's signed volume and timing; ignores its own orders.
- Maintains 441 state-space hypotheses over the informed account, impact per share, and temporary-impact half-life. Separates permanent information, temporary order pressure, and thin-book drift.
- Forecasts continuing directional flow and its eventual decay. Uses a no-trade band to avoid paying spreads repeatedly for small changes in the forecast.
- Blends toward an estimated opening value near the scheduled reopen. Holds the resulting position after reopening, avoiding an unnecessary closing spread.

## Controls

Orders are restricted to the session's SPCX market. The strategy trades in five-share slices, stays below the 70-share mandate, reads its collateral directly, and latches a stop after a $650 drawdown in either marked equity or estimated exit equity. Reopening exposure is constrained by remaining drawdown room and conservative adverse-opening estimates. These are model-based controls, not a guarantee against every possible price path.

All runtime inputs come from `Session`, `Market`, and the permitted read-only RPC methods. The strategy needs no files, environment variables, network access, or build tools at runtime. No scenario data is embedded.

## Build and check

```sh
just build
just test
mtx-practice
```

`just build` replaces `bot.wasm` atomically after a successful build. `just test` checks permanent-flow identification, transient-flow rejection, probability normalization, and mixed-side account rejection without a chain.

## Validation

The final strategy passed 72 fresh generated sessions with the grader's exact
26/17/17/12 regime mix, plus 20 scenarios selected for large reopening gaps.
Four sessions replayed with exactly identical PnL.

- Fresh-batch PnL: $14,950.23; reward: 0.207106.
- Fresh-batch maximum drawdown: $518.44.
- Large-gap stress maximum drawdown: $203.91.
- No disqualifications, refused RPC calls, position-limit breaches, or liquidatable ending accounts.
- Three state-estimator regression tests passed.

These are generated validation scenarios, not the hidden grading set.
