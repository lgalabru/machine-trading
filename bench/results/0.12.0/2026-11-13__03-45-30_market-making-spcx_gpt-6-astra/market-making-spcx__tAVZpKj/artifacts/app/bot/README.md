# SPCX market maker

`bot.wasm` is the ready-to-load `wasm32-wasip2` strategy component. It trades only the session's SPCX market through the `mtx-kit` order sheet.

The strategy anchors prices to the oracle. It maintains minimum-lot inner quotes and substantive, inventory-sensitive quotes, reads Phoenix's public order book and recent-trade ring, and removes its own aggressive executions from the flow signal. Six rolling flow windows feed a fixed, compact tree model trained on independently generated practice paths. A slow price-drift estimate also influences inventory.

Strong signals can establish a directional position, using market-order slices of at most 10 shares and explicit oracle-relative price limits. When a signal expires, the strategy first unwinds passively, then reduces excess inventory. Ordinary quote sizes account for both current inventory and pending aggressive orders.

Risk controls value collateral and the position at the oracle, including trading fees reflected in the trader account. The normal exposure ceiling is 18 shares, below the 20-share mandate; minimum-lot quotes have separate mandate checks. The exposure budget shrinks with drawdown, reserving for a 4.5% adverse move plus $35 of execution error. A $145 drawdown stop cancels orders, flattens, and remains latched for the session.

The component makes only read-only account RPCs. It needs no runtime files, network access, environment variables, or clock.

## Build and test

```sh
just build
just test
```

The build forces recompilation of the strategy when using the SDK's shared Cargo target directory, then replaces `bot.wasm` atomically. The five native tests cover invalid oracles, persistent drawdown stops, shrinking risk budgets, exposure under all fill combinations, and independence from a distorted mark price.

`src/model.rs` contains the fixed model; inference is entirely local and deterministic. `validation.json` records completed scenario checks and the delivered component's SHA-256.

## Validation

Across 72 fresh generated scenarios (18 calm, 36 with jumps, 18 trending), the selected strategy earned **$1314.13**, with a normalized reward of **0.074498**. There were **no disqualifications, refused RPCs, trapped calls, position-cap breaches, or drawdown-limit breaches**. Maximum observed drawdown was **$143.34** and maximum absolute position was **18.030 SPCX**.

The first 24 sessions compared two variants. After selecting this one, the final 48 sessions ran without further tuning and earned **$330.82**. Five native safety tests passed. These are generated-session results; the hidden grading scenarios were not available.
