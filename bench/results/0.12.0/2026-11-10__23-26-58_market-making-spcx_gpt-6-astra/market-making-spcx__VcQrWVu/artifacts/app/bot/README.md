# SPCX market maker

`run.sh` executes the prebuilt native `bin/bot`. Runtime needs only the supplied
MTX environment, exchange metadata, keypair, turn socket and Solana RPC endpoint.
There is no runtime compilation, Phoenix HTTP API access, or scenario-file input.
The GCC unwinding library is bundled in `lib/`; libc and libm come from Debian.

The strategy centers on the oracle and competes one tick inside the external
best prices. Inventory changes quote widths and available size. It estimates
trend direction from oracle returns and public order flow. A hidden-state arrival
model tracks directional bursts at several rates and expected lifetimes. During
those windows it reduces adverse-side quoting and can acquire a bounded directional position.
Signals expire, reset after jumps, and respect the session's remaining horizon.

Risk controls:

- SPCX is the only accepted symbol; SDK orders remain within the mandate.
- Normal maximum exposure is 19.5 shares, below the 20-share mandate.
- Oracle equity includes collateral, realized PnL and fees through the margin
  account. Unrealized PnL is corrected from the mark to the oracle.
- Position capacity shrinks with drawdown, reserving a 4.5% initial-price shock
  and $25 for execution costs. At 55% of the drawdown allowance, the strategy
  cancels quotes, flattens and stops taking new exposure.
- Quotes reserve capacity for every partial execution of a preceding IOC.
- Market orders are sliced; cancel failures stop quote replacement.

For development, `./build.sh` builds and atomically installs the executable.
`cargo test --release` checks IOC decoding, market/account filtering, signal
expiry, partial-fill position limits and drawdown stress reserves. The source
uses the task-provided `mtx-kit` at `/opt/mtx/sdk/crates/kit` when building.

False bursts and unannounced jumps remain sources of loss.



## Validation

These results describe completed sandbox sessions of the exact deployed binary.
The hidden evaluation scenarios were not available.

| Set | Sessions | PnL (USDC) | Maximum drawdown | Violations |
|---|---:|---:|---:|---:|
| Provided practice | 6 | 375.37 | 27.57 | 0 |
| Generated comparison | 18 | 476.29 | 35.15 | 0 |
| Holdout | 36 | 1606.24 | 90.12 | 0 |

Five unit tests cover packet decoding, account/market filtering, burst decay,
partial-fill position limits, and drawdown stress reserves. Full summary metrics
and the binary checksum are in `validation.json`.
