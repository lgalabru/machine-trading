# SPCX weekend strategy

`bot.wasm` is the built `wasm32-wasip2` strategy component. It trades only the session's SPCX market through the order sheet.

The strategy reads confirmed Phoenix market-order transactions, excludes its own orders, and tracks the eight taker authorities. A bank of Kalman filters estimates permanent price impact, temporary inventory pressure, and slow drift, while considering different informed accounts, impact sizes, and decay rates. Consistent directional flow drives weekend positions; estimated temporary pressure drives reversals. Trading has a dead band to avoid unnecessary spread and fee costs.

During the last 16 slots before reopening, the strategy switches to its estimated opening value. Position sizing stress-tests adverse opening prices, execution costs, and the difference between the book and the mark. The stress range includes a 2.7% reopening surprise allowance (4.5 times the largest scenario standard deviation); Gaussian surprises are not mathematically bounded. The bot reads its Phoenix collateral to track equity and drawdown, reduces exposure at $650 of drawdown, and permanently stops opening risk above $680. Orders are sliced into at most five shares, with an additional position-cap margin. After reopening, it holds the remaining position through valuation.

The component uses only permitted read RPC methods. It needs no files, environment variables, network service, or build step at runtime.

Build with `just build`; run the unit tests with `just test`. The build replaces `bot.wasm` atomically after successful compilation. Run the chain validation with `mtx-practice`. Validation results for the selected version are recorded in `validation.json`.

Validation: four unit tests passed. Across 78 chain sessions, total PnL was $21,040.74, reward was 0.2627, and maximum drawdown was $645.59, with no disqualifications. The final built artifact passed six additional smoke sessions and reproduced their earlier PnL exactly. An additional approximate simulation covered 10,000 generated weekends; it does not replace the chain checks.
