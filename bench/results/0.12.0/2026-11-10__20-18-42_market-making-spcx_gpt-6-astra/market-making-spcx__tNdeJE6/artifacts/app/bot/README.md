# SPCX market maker

Run `bash /app/bot/run.sh`. The executable is already built at `bin/bot`.
It requires only the documented `MTX_*` environment and the inherited turn
socket. It does not build, download, or write files while trading. The GCC
unwind runtime is bundled in `lib/`; libc and libm come from Debian.

## Strategy

- Anchor valuation and quotes to the oracle, not the manipulable mark.
- Cancel stale quotes, read the competing BBO, and quote one tick inside it,
  with inventory skew and a minimum spread.
- Estimate drift with a zero-drift prior, excluding discontinuous moves.
- Read Phoenix's recent trade buffer using standard `getAccountInfo` data
  slices. Deduplicate partial fills by trade sequence. Exclude this bot's
  transactions and the competitor's trades after a price move.
- Estimate directional jump risk from short arrival-count windows and
  background flow. Enter on strong evidence; use a lower exit threshold to
  avoid unwinding during a short pause. Small observation quotes preserve
  information while avoiding substantial exposure against suspicious flow.
- Recycle inventory through passive quotes when a signal fades; retain
  aggressive execution for strong directional evidence and risk reduction.
- Use bounded market-order slices for inventory changes. Size all outstanding
  orders together, including observation quotes and possible partial fills.

The embedded predictor is a symmetric gradient-boosted classifier trained on
1,500 independently generated public-family scenarios, with 500 separate
synthetic calibration scenarios. Only its numerical coefficients are shipped;
it has no scenario files, future prices, account identities, or session lookup.
All runtime decisions use the current oracle, public on-chain trades, the
session clock, and this account's state.

## Risk controls

The internal position limit is at most 19 SPCX, below the mandate's 20-share
cap. It decreases with drawdown, reserving equity for a 4.5% adverse move and
execution costs. Equity is collateral plus virtual quote balance plus the
position valued at the oracle. A drawdown above 44% of the mandate's limit
latches a cancel-and-flatten state. Flattening uses oracle-bounded orders.
These controls reduce risk; unexpected paths can still lose money.

## Build and checks

`./build.sh` builds the release executable and replaces `bin/bot` atomically.
Development builds require Rust and the supplied `/opt/mtx/sdk` tree.
`cargo test --release` checks model-export parity and exposure sizing,
including simultaneous resting quotes and partial execution.

Completed grader reports are in `validation/`. Practice results are development
measurements, not a guarantee of hidden-session returns.

## Final validation

The final version completed 12 matched comparison sessions plus six stress
cases selected from the preceding version’s losses: **+$660.12**, versus
**+$411.03** for the preceding version on those same sessions. Maximum
drawdown was **$47.58**, maximum position **19 SPCX**, with no disqualifications,
refused RPC calls, or turn overruns. These are selected development cases.

The preceding version also completed a separate balanced 72-session batch
without violations. Its reports are explicitly labeled `prior-v8-*`; they
are historical evidence, not results attributed to the final exit policy.
See `validation/summary.json` for the final executable hash and results.
