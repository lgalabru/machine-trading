# SPCX weekend strategy

`bot.wasm` is a built `wasm32-wasip2` strategy component. Rebuild with `just build`.

The strategy reads confirmed SPCX market-order transactions and keeps each taker's signed flow separate. A small Kalman-filter ensemble compares possible informed accounts, impact sizes, and decay rates. It forecasts continuing flow, the decay of temporary impact, and the reopening price. Trading uses a position target with a transaction-cost buffer.

Orders are immediate market orders in slices of at most five shares, with limits relative to the current book. No orders rest across turns. The strategy only trades the session's SPCX market.

Risk controls read actual Phoenix collateral and unrealized PnL, track peak equity, stop after a $550 observed drawdown, keep the position at or below 69.9 shares, and reduce exposure in the last 20 slots before reopening using opening-price stress bounds of ±15.5% and an $850 stress-loss budget. These controls reduce risk; they do not make unknown reopening prices predictable.

All inference runs inside the component. It requires no runtime files, network connection, environment variables, or external model service.

`validation.json` records completed chain tests and the SHA-256 of the built component. Generated-path research is separate from those chain-test results.
