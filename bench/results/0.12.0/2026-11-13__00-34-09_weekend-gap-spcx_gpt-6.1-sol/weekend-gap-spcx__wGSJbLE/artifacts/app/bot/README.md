# SPCX weekend strategy

`bot.wasm` is the ready-to-run `wasm32-wasip2` strategy component. It embeds every model weight and needs no runtime files, network access, environment variables, or build step.

Build with `just build`. Run the feature/forest parity test with `PATH=/usr/local/cargo/bin:$PATH cargo test --release --lib`.

The strategy reads confirmed SPCX market-order transactions and groups their signed flow by trader. Its inputs include order size, direction, limit aggressiveness, cadence, persistence, exponential flow decay, and quiet price relaxation. It reads the active maker splines to estimate the underlying quote midpoint without treating its own consumed book depth as new information.

Embedded tree ensembles forecast the closed period and the reopen separately. The models were fitted on generated weekend-gap scenarios, with a separate 1,500-scenario validation set. The runtime uses only the current session and current chain reads. The forest implementation and feature calculations are checked against exported training-runtime predictions.

Orders trade only the session's SPCX market. The closed-period target is at most 69 shares, with a hysteresis band to limit turnover. Market orders use five-share slices. The strategy holds its remaining position after the reopen rather than paying another spread.

Risk checks read actual collateral and entry prices, include fees and execution losses in equity, reconstruct the previous turn's marked equity, and track the equity peak. A $600 closed-period drawdown halts trading. Before the reopen, position size uses a $900 stress-loss budget and an absolute cap of 60 shares. Opposing persistent one-way flow invokes the wider reopen stress range. The order-sheet planner also applies the session mandate's position cap.

The model binary format is a little-endian header (`f32` base value, `u32` root count, `u32` node count), followed by `u32` root offsets and nodes containing `i32` feature/left/right fields and one `f32` split threshold or leaf value. A negative feature identifies a leaf.

Validation: 72 fresh generated sessions earned $17,515.14 total PnL (reward 0.240067), with no disqualifications. The worst session lost $231.92, maximum drawdown was $411.52, and 61 of 72 sessions were profitable. Full reports are in `validation/`.

The exact final artifact also passed two identical replays of four selected sessions, including the worst loss, deepest drawdown, largest profit, and largest instruction call. Builds use a distinct crate artifact name and atomically replace `bot.wasm`.
