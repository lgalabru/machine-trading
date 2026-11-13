# SPCX funding and price strategy

`bot.wasm` is the built `wasm32-wasip2` strategy component. Build it with `just build` using the supplied `/opt/mtx/sdk`.

The strategy forecasts the next eight seconds of book-price return net of funding. Its embedded regression trees were trained on independently generated practice-family scenarios, with additional index-noise and fizzle augmentation. Inputs are causal: premium dynamics, index momentum and mean reversion, observed volatility, episode history, and the funding parameters read from Phoenix. It never reads scenario files at runtime.

Position selection compares positions between the mandate's short and long caps against estimated taker fees, book impact, and an execution hysteresis margin. Orders use ten-share slices. The price estimate accounts for depletion of one side of the book by the bot's previous trades.

When expected funding is large enough to cover execution costs and adverse price movement, the strategy temporarily reduces exposure to the paying side before settlement. It waits for the on-chain funding interval to advance before releasing that adjustment.

Risk checks use Phoenix collateral, position cost basis, and unsettled funding. Exposure decreases after drawdown reaches 35% of the desk's limit; trading stops and the position is reduced toward zero at 55%, leaving a buffer for discrete price jumps. The order planner also enforces the session's position cap. Only SPCX is traded, and the only RPC method used is the read-only `getAccountInfo`.

The final turn does not incur unnecessary closing trades: positions are valued by the session rather than forcibly liquidated. Model weights are embedded in `src/model.rs`; no runtime files, network access, or build tools are needed.

See [VALIDATION.md](VALIDATION.md) for the measured results and [validation.json](validation.json) for their machine-readable summary.
