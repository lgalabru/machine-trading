# SPCX weekend strategy

`bot.wasm` is the ready-to-run `wasm32-wasip2` strategy component. It needs only the session, market observations, and permitted read RPCs.

The strategy reads confirmed Phoenix transactions and decodes taker signers, sides, quantities, and limit prices. It combines repeated activity by account, order cadence, decaying signed volume, book prices, and time until the reopen in a fitted price forecast. The embedded model was fitted using 5,000 training scenarios and 1,000 validation scenarios independently generated from the supplied family. It contains no scenario identifiers or future session data.

Orders use transaction-cost hysteresis and slices of at most five shares. The strategy reads its Phoenix trader account directly to measure collateral, unrealized PnL, and peak equity, including execution costs. It stops after substantial drawdown and reduces exposure before the reopen using a 16% opening-price stress in either direction. The stress calculation accounts for a gap between the book and mark and for an equity peak created by a new position. Small required risk reductions bypass the normal turnover threshold.

After the reopen it leaves the remaining position in place, avoiding another spread crossing during the short regular session. It places no resting orders and trades only the session's SPCX market.

Build and check in the supplied SDK environment:

```sh
just build
just test
```

The build replaces `bot.wasm` atomically after successful compilation. `validation.json` records the final sandbox validation results.
