# SPCX shock trader

A native Rust bot for the `mtx-turn-1` Phoenix environment. Run `bash run.sh`
with the supplied `MTX_*` variables. The executable and its price model are
already built; runtime requires no compiler, Python, downloads, or API service.

## Trading logic

The strategy observes the oracle, mark, position, and account equity each turn.
A local gradient-boosted price forecast distinguishes continuing moves from
likely reversals. Separate entry and holding thresholds reduce transaction
costs when the forecast is weak. All model inputs are causal price-history and
session-clock features; the bot never reads scenario files.

Market orders move toward a target in slices of at most 10 shares. The SDK
clips targets to the session's SPCX position mandate. There are no resting
orders or trades in other markets. Normal orders use a 60-basis-point price
bound; emergency flattening permits 200 basis points.

Risk control reads Phoenix account equity directly, including trading costs.
Exposure decreases as drawdown consumes the mandate's headroom, using a 5.5%
price-gap reserve. At 55% of the allowed drawdown, the bot permanently flattens.
An unavailable account-equity read also triggers flattening. The last turn
avoids an unnecessary closing trade because positions are valued at that mark.

## Build

`./build.sh` builds against the supplied SDK at `/opt/mtx/sdk/crates/kit` and
atomically installs `bin/bot`. For development, `BOT_BUILD_DEST=/some/path`
stages a candidate without replacing the installed executable. `run.sh` only
executes `bin/bot`; it never builds or writes files.

The model's tree nodes are embedded in the executable with `include_bytes!`.
`src/model.rs` implements inference without a machine-learning runtime.
Diagnostics go to stderr.

## Validation

See [VALIDATION.md](VALIDATION.md) and the reports in `validation/`.
The strategy passed 168 generated chain sessions with no rule violations.
