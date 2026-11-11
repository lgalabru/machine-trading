# SPCX market maker

`bash run.sh` starts the prebuilt native executable at `bin/bot`. It uses the
`MTX_*` contract supplied by the grader and the SDK's turn protocol. Runtime
operation does not build, download, or write files in this directory.

The strategy anchors to the oracle, reads competing FIFO quotes after cancelling
its own orders, and quotes both sides with inventory skew. It detects unusually
concentrated directional taker flow from standard Solana transaction-history RPC
calls. A detected burst produces a temporary directional position and a wider
quote on the exposed side. A jump or timeout ends the signal. Ordinary exits are
sliced over turns to allow competing liquidity to replenish.

Positions remain below the mandate's cap, with small ordinary inventory and a
larger temporary burst allocation. Exposure shrinks with oracle-adjusted equity
drawdown, reserving room for another 5% price gap. A drawdown stop cancels quotes
and flattens. Hedge orders are lot-rounded, sliced, and checked against the actual
position after execution. Only the configured SPCX market is used.

For development, `./build.sh` builds and atomically installs `bin/bot`.
`cargo test --release --bin bot` checks SDK packet decoding and the exposure
budget. Session validation results are recorded in `validation.json` when present.

The selected strategy passed 78 market-family sessions (six provided and 72
generated): aggregate PnL +2,700.18 USDC, reward ratio 0.1426, maximum drawdown
128.67 USDC, and peak position 19.8 SPCX. There were no disqualifications, refused
RPC calls, late sends, or turn overruns. Mean turn time was 47.9 ms. A separate
adverse-flow stress case reached a 135.40 USDC drawdown and ended flat without
a violation. Full per-session results and artifact hashes are in `validation.json`
and `reports/`.
