# SPCX shock trader

`bot.wasm` is a ready-to-run `wasm32-wasip2` strategy component. It trades only the session's SPCX perpetual, through `mtx-kit`'s order sheet.

The strategy estimates a discounted near-term return from observed index prices. Features describe recent returns, ranges, moving averages, impulse sizes and ages, directional episodes, elapsed time, and time remaining. Crash and squeeze features share a direction-normalized representation. The compiled regression forest was fitted on 45,000 independently generated scenarios; no scenario identifier, future price, or hidden evaluation data is available to the running strategy.

An estimated return above 0.10% selects a long position, and below -0.10% selects a short position. An opposing estimate of 0.05% exits an existing position. The different entry and exit thresholds limit trading costs. The model can follow an active move, fade a paused shock, or hold through a continuation depending on the observed history.

Risk uses the trader account's actual collateral plus unrealized PnL at the grading mark, including settled fees. With the supplied mandate:

- Absolute position is capped at 70 shares.
- Position size also reserves room for another 4.5% adverse step within an $850 drawdown budget.
- Drawdown above $620 permanently halts the session and repeatedly targets flat until filled.
- A failed account read permits only an attempt to flatten.
- Orders are immediate, in the SDK's 10-share slices. There are no resting orders.

The runtime makes one allowed `getAccountInfo` read per step. It uses no files, network services, environment variables, or wall clock.

## Build and test

```sh
just build
just test
mtx-practice
```

`just test` checks feature parity with the training pipeline, exported model predictions, crash/squeeze symmetry, drawdown halt persistence, missing account data, and the reserve for another price shock. Full chain validation reports are kept in `validation/`.

Source: `src/lib.rs` implements the strategy, `src/signal.rs` maintains causal features, `src/risk.rs` controls exposure, and `src/model.rs` contains the compiled forest. No training dependencies are needed to build or run the component.

## Completed chain validation

The 114-session comparison returned $39,140.13 PnL against a $148,472.03 hindsight bound (26.36%), with $651.46 maximum drawdown and no disqualifications. The previous checked strategy returned $37,576.78 on exactly those sessions.

Twelve additional stress sessions tested three 8% legs followed by extension and reversal, and repeated discontinuous whipsaws. All passed; their maximum drawdown was $642.65. These stress sessions are reported separately because they are intentionally constructed paths, not a random sample of the grading family.

The `validation/` JSON files contain the individual session results. These are local validation results, not the hidden grading score.

A final, separately generated 72-session check returned $23,037.24 PnL with no disqualifications. Across all 186 normal sessions, total PnL was $62,177.37 against a $237,097.72 bound (26.22%). All 12 additional stress sessions also passed. Across all 198 sessions, the largest position was 70 shares and the deepest drawdown was $651.46. Maximum per-call fuel was 10,766,746 instructions.

`validation/summary.json` records the combined results and SHA-256 of the exact tested WASM artifact.
