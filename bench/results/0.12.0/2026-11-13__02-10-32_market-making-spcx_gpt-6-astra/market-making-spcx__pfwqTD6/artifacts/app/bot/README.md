# SPCX market maker

`bot.wasm` is the ready-to-run `wasm32-wasip2` strategy component. It uses only
SPCX order-sheet actions and the permitted `getProgramAccounts` and
`getMultipleAccounts` reads. It needs no runtime files or external services.

## Strategy

- Anchor quotes to the oracle, round to Phoenix ticks, and respond to the best
  quotes. Inventory and recent flow shift the quotes; normal quotes retain a
  minimum distance from the oracle.
- Estimate persistent drift with a mixture of stationary and drifting markets.
  Exclude jumps from that estimate.
- Discover a recent trader-account cohort from public account state. Monitor
  changes in its SPCX positions to estimate external buying and selling,
  without counting the bot's own aggressive orders as information.
- Compare directional order counts with the session's observed activity rate.
  A sufficiently unusual burst can acquire a position with the flow. Hold it
  while the signal is active, then exit when it expires or a jump arrives.
  Suppress burst entries during the stated jump cooldown and near session end.
- Value collateral and the SPCX position at the oracle. Use a 19.8-share internal
  cap, shrink exposure as drawdown grows, and latch a stop at $130 drawdown.
  Reserve drawdown budget for another adverse move before the next turn.

Fake-outs and unannounced jumps can lose money. The strategy uses observed
market state; it does not know future prices or orders.

## Build and check

```sh
cd /app/bot
just build
just test
mtx-practice
```

The build forces recompilation because the environment shares Cargo artifacts
between experiments. It installs the completed component atomically.

Seven unit tests cover invalid prices, quote limits, the drawdown stop and
reserve, burst activation, cooldown, terminal suppression, and burst expiry.

## Validation

On 72 generated market-making sessions (seed `492831`):

| Measure | Result |
|---|---:|
| Total PnL | $2,018.27 |
| Reward against the scenario bounds | 0.11737 |
| Maximum drawdown | $96.09 |
| Peak absolute position | 19.795 SPCX |
| Profitable sessions | 45 / 72 |
| Disqualifications | 0 |
| Refused RPC calls | 0 |
| Strategy exits | 0 |

Full per-session results and the tested component hash are in `evaluation.json`.
These are generated validation scenarios, not the hidden grading batch.
