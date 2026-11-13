# SPCX weekend strategy

`bot.wasm` is the prebuilt `wasm32-wasip2` strategy component. Rebuild with `just build`.

The strategy reads confirmed Phoenix transactions and tracks the eight taker accounts. A bank of causal Kalman filters compares hypotheses about which account has permanent price impact, while estimating the temporary-impact half-life and impact per share. Account direction, size, timing and limit prices inform the flow classification.

It follows continuing directional flow, fades estimated temporary pressure, and uses an inactive informed account's last execution price to estimate the reopening level. A return-based trading buffer limits turnover; orders are sliced into five-share transactions.

Risk checks use the on-chain trader account, including fees. Exposure is capped at 70 shares and is reduced before reopening using a conservative adverse-gap reserve. A separate drawdown guard accounts for the cost of liquidating against a book that differs from the mark. The bot holds surviving exposure after reopening to avoid paying a spread larger than the remaining modeled price wobble.

All runtime observations come from the strategy interface and permitted read-only RPC calls. The component needs no files, network, environment variables or external services.
