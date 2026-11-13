# SPCX shock strategy

`bot.wasm` is the ready-to-load `wasm32-wasip2` strategy component. The Rust source uses `mtx-kit`; it trades only the session's SPCX market.

The strategy uses oracle history to forecast short-term continuation and reversal. Its features describe recent returns, extrema, time since moves, successive legs, and time remaining. Negative price excursions are mirrored so crashes and squeezes share the same models. Two embedded regression-tree ensembles estimate discounted future price changes; entry and holding thresholds reduce turnover when the forecast is weak. No training data, model files, network access, or runtime filesystem is required. Model provenance is recorded in `model-info.json`.

Risk management reads the Phoenix trader account through one read-only `getAccountInfo` call per step. It values collateral and the SPCX position at the mark, tracks drawdown, reduces position size to leave room for another adverse move, and permanently halts after a $750 observed drawdown. Sizing reserves $150 for execution and valuation differences and budgets a 4.5% move against at least the starting mark. A turnover-cost estimate is available if account decoding fails.

Orders use the SDK's position-cap planner and 10-share market-order slices, with a 40-basis-point execution limit. The strategy leaves no resting orders and submits no new trade on the final step. Positions may remain open for the grader's mark valuation.

Build and test from this directory:

```sh
just test
just build
sha256sum -c bot.sha256
mtx-practice
```

The build forces recompilation because experimental crates share the SDK target directory. It replaces `bot.wasm` atomically only after compilation succeeds. The five unit tests check model-export agreement, tree integrity, direction symmetry, the risk reserve, and persistent halting.

Sandbox validation reports and their aggregate summary are saved under `validation/`. These are generated and practice scenarios, not the hidden grading sessions. Scenario tests include fees, slippage, account decoding, order execution, mandate checks, and component fuel accounting. Practice replay was checked twice for deterministic reports.

| Validation set | Sessions | PnL (USDC) | Hindsight-bound reward | Maximum drawdown |
| --- | ---: | ---: | ---: | ---: |
| Two generated sets | 144 | 55182.82 | 27.96% | 576.13 |
| Selected large shocks | 24 | 33520.17 | 42.02% | 736.85 |
| Practice (two identical replays) | 6 | 866.44 | 13.35% | 287.22 |

All sessions passed the position, market, drawdown, liquidation, RPC, and component-budget checks. The stress set selects unusually large moves; it is a risk test, not an estimate of average performance.
