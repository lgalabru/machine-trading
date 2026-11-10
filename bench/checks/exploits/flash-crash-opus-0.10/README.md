# SPCX shock bot

`run.sh` starts `bin/bot`, built by `build.sh` (run it after every change).

- `src/strategy.rs`: the trading logic, with no chain access. It rides moves
  while they land (big moves land in equal chunks of at most 4% per step,
  some ramp), fades every move that can still be a fizzle (fizzles return to
  the exact level they left within 1.5 to 10 s), and fades big shocks for
  their aftermath. A drawdown guard sizes positions so that one step's
  worst move stays inside a $750 budget.
- `src/main.rs`: the live loop. Each turn it reads the oracle and the
  account, asks the strategy for a target position, and trades to it with
  market orders.
- `src/bin/backtest.rs`: replays scenario files (`mtx scenario generate`)
  through the same strategy on a model of the market calibrated against live
  runs: `cargo build --release --bin backtest`, then
  `$CARGO_TARGET_DIR/release/backtest [name=value ...] FILES...`
  (`-v` for trades, `BREAKDOWN=1` for PnL by reason).
