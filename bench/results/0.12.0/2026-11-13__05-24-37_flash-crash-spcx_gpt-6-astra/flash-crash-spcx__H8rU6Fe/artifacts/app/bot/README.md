# SPCX shock strategy

`bot.wasm` is the ready-to-run `wasm32-wasip2` strategy component. It uses only the session, market observations, and one allowed `getAccountInfo` read per step. The model is compiled into the component; there are no runtime file, environment, HTTP, or Python dependencies.

## Trading policy

The strategy forecasts the value of holding a position from 52 causal features: recent oracle returns, ranges and mean deviations, the size and age of observed impulses, elapsed time, and time remaining. Features are oriented by the most recent impulse so the same policy handles crashes and squeezes. The forecast can favor continuation, a reversal, or remaining flat; it does not assume every shock reverts.

The embedded tree model was fitted on independently generated scenarios from the published family. The primary training seeds were `419284` and `731845`; model selection also used a separate 3,000-scenario sample with seed `932817`. No scenario path is accessible to the running strategy. `src/model.rs` contains the complete inference model.

A hysteresis band avoids repeatedly crossing the book on weak forecasts. Orders target an absolute position through the kit's 10-share slices, with a 50-basis-point execution limit. There are no resting orders. The final turn preserves the position for the specified mark valuation instead of paying to close it unnecessarily.

## Risk controls

The bot reads its own Phoenix trader account and values its collateral and SPCX position at the current mark. It also checks the kit's drawdown estimate. Exposure is capped by both the mandate and:

```
floor((0.85 * max_drawdown - drawdown) / (0.05 * mark))
```

Exposure falls as drawdown consumes the budget. A drawdown above 78% of the mandate's limit permanently halts new trading and targets flat. A failed account read also targets flat. The reserve accounts for another adverse move and execution costs; the published family limits individual price steps to approximately 3.8%.

## Build and checks

```
just build
PATH=/usr/local/cargo/bin:$PATH cargo test --lib
mtx-practice
```

The build copies to a temporary file and atomically renames it to `bot.wasm`. Tests check forecast parity with the training implementation and stress the risk-sizing formula across prices, drawdowns, an adverse jump, and execution costs.

`validation/` contains actual chain replay reports, not the fast development simulator. The stress set deliberately includes the worst simulated drawdowns and largest shocks; its profit distribution is not representative of ordinary random sessions. Results on these generated scenarios do not determine results on unseen sessions.

## Checked results

The final component's 114-session development/validation suite earned $43,508.41, with no disqualifications. The separate 72-session validation subset earned $24,066.95 (reward 0.2566), with maximum drawdown $547.41. The component's busiest call used 11.1 million WebAssembly instructions, against the 10-billion call limit.

See `validation/summary.json` for the artifact hash and complete final test counts, including the deliberately difficult stress set.

The final fresh 36-session set (seed `1740392`, generated after model selection) earned $14,070.30, reward 0.2787. Including 41 selected stress cases, the identical component passed 191 sessions with no disqualifications. The deepest observed drawdown was $778.55.
