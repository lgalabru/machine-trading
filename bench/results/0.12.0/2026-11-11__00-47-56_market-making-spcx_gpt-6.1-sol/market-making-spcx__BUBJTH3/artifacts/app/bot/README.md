# SPCX market maker

`bash /app/bot/run.sh` starts the prebuilt native executable at `bin/bot`.
It needs only the environment variables in the trading contract. It does not
build, download files, contact Phoenix's HTTP API, or write to its directory.

The strategy:

- Anchors prices and account equity to the oracle. Reads the competing order
  book and excludes its own orders before improving the best available quotes.
- Quotes small sizes with inventory skew and rounds prices and quantities to
  Phoenix ticks and lots. Normal inventory stays small.
- Reads completed blocks through standard `getBlock` RPC calls and counts SPCX
  market orders, including orders that did not fill against this bot. An embedded
  tree model estimates directional jump risk from public flow and price history.
- Withdraws the vulnerable quote during one-sided bursts. Strong signals permit
  bounded IOC trades; a separate price-and-flow drift forecast can retain a
  directional position when its estimated gain exceeds execution costs.
- Computes drawdown from collateral, virtual quote balance, and oracle-valued
  base position. It reduces exposure against a 4.5% shock estimate and stops
  opening risk after a $100 drawdown. The stop cancels quotes and flattens in slices
  with limits tied to the oracle. The SDK additionally clips orders to the
  mandate's position cap.

The model is embedded in the executable; `model.json` is its build-time source.
`config.json` contains the tested strategy parameters and is read once at startup.
Only standard Solana RPC methods are used, and orders are restricted to the
market supplied by the SPCX session.

## Development

With the supplied Rust toolchain and `/opt/mtx/sdk` available, `./build.sh`
rebuilds and atomically replaces `bin/bot`. `run.sh` never calls it.

Tests cover exported model prediction parity and oracle-bounded drawdown-stop
orders for both long and short positions. Run them with `cargo test --release`.
Validation reports for the selected executable are in `validation/`.

The six practice sessions returned **$315.18** total PnL (reward **0.2194**),
with **$20.83** maximum drawdown and no mandate or RPC violations.

An artificial false-burst stress test entered 18.4 SPCX before an adverse 4%
jump. The stop flattened the position before the next turn, stayed flat through
two further adverse jumps, and limited drawdown to **$111.61**.

On 24 fresh generated audit sessions, the selected version returned **$1,170.79**
PnL (reward **0.2039**), with 18 profitable sessions and **$108.46** maximum
drawdown. Every session passed the market, position, RPC, and liquidation checks.
Average turn time was **29 ms**, against the 500 ms per-step aggregate budget.
