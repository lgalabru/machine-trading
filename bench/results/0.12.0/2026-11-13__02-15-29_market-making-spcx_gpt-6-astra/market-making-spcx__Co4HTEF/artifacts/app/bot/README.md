# Phoenix SPCX market maker

`bot.wasm` is the ready-to-run `wasm32-wasip2` strategy component. The grader does not need to build it.

```sh
cd /app/bot
just build
mtx-practice
```

The build uses the supplied SDK at `/opt/mtx/sdk/crates/kit`. It copies a successful build to a temporary file and renames it atomically over `bot.wasm`.

## Strategy

- Quote both sides around the oracle, competing one tick inside visible liquidity where the inventory-adjusted spread permits. Avoid repeatedly undercutting our own quote.
- Read completed blocks to recover public SPCX market-order direction, including orders that did not fill against us. Exclude our own transactions.
- Use a fixed, compiled classifier of recent directional order counts, activity, volatility, and elapsed market time to identify concentrated flow. Its inputs contain no future prices or scenario identifiers.
- Require a stronger signal when crossing the spread costs more. During a strong signal, cancel passive quotes and take a bounded directional position. Resume making after the signal expires or a jump occurs.
- Estimate persistent drift separately and use it only to skew ordinary passive inventory.

## Risk controls

The strategy targets at most 19 SPCX during a burst. Normal inventory stays near a much smaller target. Every quote is sized against both inventory bounds and the session's cap; the SDK's planner provides an additional cap check.

Equity is estimated from the trader account at the oracle, with an adjustment for maker fills awaiting settlement. Position capacity shrinks with drawdown, reserving $35 for costs and valuation differences and sizing against an adverse 4.5% move. At $120 estimated drawdown the strategy cancels quotes, flattens, and stays halted. These buffers are conservative controls, not a guarantee against arbitrary price paths.

Only `getBlock` and `getAccountInfo` are called. Runtime data comes entirely through the strategy interface and permitted RPC reads. The strategy needs no files, environment variables, network access, or model service.

## Files

- `src/lib.rs`: quoting, inventory, execution, and risk management.
- `src/flow.rs`: bounded public transaction-history reader.
- `src/model.rs`: compiled decision trees and deterministic inference.
- `MODEL.md`: model provenance and feature definitions.
- `validation.json`: final independent scenario evaluation, when completed.

## Validation

The final component was evaluated on 72 separately generated scenarios (seed 6297443) and the six supplied practice scenarios. None were used to fit the classifier.

| Set | Total PnL (USDC) | Reward | Maximum drawdown (USDC) |
|---|---:|---:|---:|
| 72 fresh scenarios | 2912.12 | 0.1765 | 81.16 |
| 6 practice scenarios | 207.59 | 0.1445 | 28.90 |

All 78 sessions completed with no disqualifications, refused RPC calls, out-of-market positions, cap breaches, liquidatable accounts, or turn overruns. These are offline benchmark results, not live trading results. The full report is in `validation.json`; the earlier 23-session development evaluation is in `development.json`.

The Rust tree evaluator was also compared with the fitted Python model on 200 vectors; the largest probability difference was below `2e-14`.
