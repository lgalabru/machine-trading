# SPCX shock strategy

`bot.wasm` is the built `wasm32-wasip2` strategy component. It trades only the session's SPCX market.

The strategy estimates near-term price changes from causal price history. Two embedded regression forests forecast returns over different horizons. Inputs include recent returns, moving averages, price ranges, and the sizes and ages of observed moves. The forecasts distinguish continuing moves from opportunities to fade them. A smoothed forecast and separate entry/exit thresholds limit turnover.

The forests were trained offline on 8,000 generated scenarios from the provided flash-crash family. Runtime inference uses only `Session`, current/past `Market` observations, and the bot's trader account; it has no dependency on training files, network access, environment variables, or wall-clock time.

## Risk and execution

- Position targets stay within `market.cap` and execute in 10-share market-order slices through `mtx-kit`.
- One allowed `getAccountInfo` read per turn obtains the Phoenix trader account. Collateral, virtual quote balances, and marked position value provide account equity including trading costs.
- Position size decreases as drawdown consumes the risk budget, reserving room for a further sudden move.
- At $650 drawdown, the bot flattens and stops trading for the session.
- If the trader account cannot be valued, the bot flattens and halts.
- The strategy leaves no resting orders.

## Build

```sh
cd /app/bot
just build
```

The SDK dependency is `/opt/mtx/sdk/crates/kit`. The build recipe sets the installed Rust toolchain's PATH and refreshes source timestamps to prevent stale artifacts when several experimental crates share the Cargo target directory.

## Validation

`validation-train.json` records the promoted strategy's 78-session sandbox check (72 fresh generated scenarios and six practice scenarios). Its PnL was $24,870.76, with a $381.62 maximum drawdown and no disqualifications. The preceding strategy made $19,710.88 on the same paths.

Rust feature extraction and forest outputs were checked against the training implementation. The largest call in the 78-session run used approximately 10.3 million WebAssembly instructions, below the instruction budget.

The separate 72-session validation batch is in `validation-test.json`: $26,050.08 PnL, 26.29% of its hindsight bound, 61 profitable sessions, $517.74 maximum drawdown, and no disqualifications. These paths were separate from model training and policy tuning.

A third independent 72-session sandbox batch is in `validation-final.json`: $21,988.44 PnL and $531.06 maximum drawdown. Across all 222 distinct scenarios, the bot had no disqualifications, refused RPC calls, position-cap breaches, or liquidatable endings. `validation-summary.json` aggregates the results.

After the final safety change, all six practice sessions were replayed twice with identical reports and unchanged PnL; see `validation-replay.json`.
