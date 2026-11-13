# SPCX market maker

`bot.wasm` is the built `wasm32-wasip2` strategy component. It trades only the
session's SPCX market through the SDK order sheet.

## Strategy

- Quotes both sides around the oracle, on Phoenix's tick grid. Inventory and
  observed order flow shift the quotes; ordinary orders are at most 2 SPCX.
- Reads confirmed Phoenix IOC instructions to count attempted buys and sells,
  including orders whose limits prevented fills.
- Uses a single read-only Hawkeye simulation to observe live taker positions
  and the bot's margin. This accounts for Phoenix's active trader buffers,
  which can contain fills absent from the base trader account.
- Detects unusually concentrated directional flow over a short rolling window.
  A strong signal can acquire a position of up to 19 SPCX in bounded slices.
  The exposed quote becomes small and wide while that signal is active.
- Estimates persistent drift from price changes and buy/sell imbalance. A jump
  resets the burst signal, starts a cooldown, and disables the trend hypothesis.

The strategy uses only session data, current market data, `getBlock`, and
read-only `simulateTransaction` calls. It has no runtime files, external feeds,
network access, clock dependency, or embedded scenario paths.

## Risk controls

Resting orders are sized against a 19.8-SPCX internal inventory limit, with the
SDK planner enforcing the session's 20-SPCX cap. Directional trades use slices
of at most 8 SPCX. Quotes are cancelled before replacement or aggressive trades.

Equity is read through Phoenix's live margin view and adjusted from mark to
oracle. A $115 drawdown triggers a persistent cancel-and-reduce-only state,
leaving headroom beneath the session's $250 limit. Invalid oracle prices cause
cancellation. Positions need not be closed at the final oracle valuation.

## Build and test

```sh
just build
just test
mtx-practice
```

The build installs the component atomically, preserving the last successful
`bot.wasm` if compilation fails. The grader needs only the built component;
it does not compile this source.

The five native tests cover invalid-price handling, inventory boundaries,
bounded directional orders, persistent drawdown shutdown, and trend symmetry.
Evaluation reports are saved in `reports/`.

## Validation

| Set | Sessions | Total PnL (USDC) | Reward | Maximum drawdown (USDC) |
|---|---:|---:|---:|---:|
| Practice | 6 | 326.56 | 0.2273 | 21.38 |
| Generated comparison | 12 | 564.23 | 0.1917 | 43.04 |
| Generated holdout | 72 | 2459.57 | 0.1413 | 99.26 |

All evaluated sessions passed the position, drawdown, market, RPC and liquidation checks.
The final component reproduces the selected strategy’s six practice PnLs exactly.
`validation.json` records the component checksum and evaluation summary.
