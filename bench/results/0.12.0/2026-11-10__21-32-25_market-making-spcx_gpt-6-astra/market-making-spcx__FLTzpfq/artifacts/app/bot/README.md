# SPCX market maker

`run.sh` starts the prebuilt native executable at `bin/bot`. Runtime inputs are the grader's `MTX_*` variables and turn socket. The bot needs no build tools, Python, exchange HTTP API, or writable application directory.

The strategy:

- Quotes both sides around the oracle, using competing book prices and an inventory-dependent skew.
- Estimates gradual drift from observed price changes while excluding jumps.
- Reads completed market orders through standard Solana `getBlock` calls. An exponentially weighted arrival count detects unusually dense directional flow.
- Temporarily hedges in the direction of a detected burst, then unwinds after a jump or signal expiry. Quiet-market quoting resumes afterward.
- Keeps ordinary inventory within 8 shares and burst exposure below the mandate's 20-share cap. A drawdown-dependent exposure limit reserves room for an adverse move; the drawdown stop cancels quotes and flattens permanently for that session.

Risk accounting uses actual Phoenix collateral plus unrealized PnL at the oracle. Orders are cancelled before replacing quotes, and market adjustments occupy their own turn so resting-order limits do not depend on assumed fills.

Rebuild in the development environment with `./build.sh`. The build publishes `bin/bot` by atomic rename. Focused strategy tests can be run with `cargo test --release --bin bot` when the supplied SDK and Rust toolchain are installed.

Validation reports are kept in `reports/`. These are offline test results, not forecasts of hidden-session performance. The executable does not read them.

The final executable passed five focused tests and completed 52 independent validation sessions with total PnL of **$1,526.94**. The fully reported 40-session batch earned **$1,286.19**, with no rule violations or turn overruns, a maximum position of **19 SPCX**, and maximum drawdown of **$101.62**. Twelve additional sessions completed before their larger batch was stopped to fit the time budget; those results are recorded separately. No strategy changes were made during independent validation.

Four additional adverse cases selected for high simulated drawdowns also passed all mandate checks. Maximum observed drawdown in those stress tests was **$153.21**; the stop flattened the two deepest-drawdown cases. Stress-case PnL and individual outcomes are reported separately in `reports/risk-stress-4.json`.
