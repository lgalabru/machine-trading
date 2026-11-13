SPCX market maker for the Phoenix strategy-component interface.

The bot quotes around the oracle, competes with displayed prices, and skews its quotes toward a controlled inventory target. Small inside orders keep low-limit taker flow observable. It discovers active traders from public account headers, excludes observed makers, and measures directional flow from position changes, including trades against spline liquidity.

Concentrated flow can trigger a temporary directional position. Signals expire, reset after jumps, and respect the scenario's jump spacing and session boundaries. A Gaussian mixture estimates persistent drift and adjusts the ordinary inventory target. The bot uses neither the scenario seed nor future path data.

Directional position targets are capped at 18 SPCX, leaving room for small quotes under the 20 SPCX mandate. The limit shrinks as oracle-valued drawdown grows, reserving room for another jump and execution costs. A persistent drawdown stop cancels orders and flattens the account. Orders use only the session's SPCX market.

Build with `just build`. The included `bot.wasm` is the compiled `wasm32-wasip2` component. At runtime the bot uses only session data, market observations, and the allowed `getProgramAccounts`, `getAccountInfo`, and `getMultipleAccounts` reads. It requires no files, environment variables, clock, or external network service.

The selected logic was validated on 71 fresh generated scenarios: total PnL **+$2,001.66**, reward **0.11921**, maximum drawdown **$42.07**, and no disqualifications. In an 18-session paired comparison it earned more than the preceding version. `validation-generated.json` contains the full report. The final compiled component passed all six practice sessions with total PnL **+$146.85**, reward **0.10222**, maximum drawdown **$34.76**, and no disqualifications; see `validation-practice.json`.
