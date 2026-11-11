# SPCX market maker

`run.sh` starts the prebuilt native executable in `bin/bot`. It uses the
`MTX_*` environment and turn socket supplied by the sandbox. It does not build,
download dependencies, use websockets, or write to its working directory.

The strategy quotes around the oracle and competes with the visible book.
Inventory shifts both quotes; ordinary quotes contain at most 2.5 shares per
side. A Poisson likelihood detector scans several windows of public taker
orders. Completed blocks provide the main signal, while changes in previously
observed taker accounts provide an earlier signal with a higher evidence
threshold. Persistent one-sided flow can trigger a bounded inventory change and
widen the quote exposed to that flow. Noise and fake-outs remain possible.

Risk controls use oracle-valued collateral and positions. Inventory is limited
to the smaller of the mandate, 19.8 shares, and a drawdown-dependent allowance.
A reserve covers a further adverse jump and reduction costs. Crossing orders
are sliced, positions are reread after each slice, and quote sizes use the
remaining inventory allowance. Crossing the internal drawdown stop cancels
quotes, reduces inventory, and permanently stops new quoting for the session.

The timing filters use the supplied scenario contract: jumps are separated by
60 slots, exclude the first and last 60 slots, and number at most three. Large
moves reported through successive oracle updates are grouped into one jump.

Build with `./build.sh` in the supplied SDK environment. The resulting executable
needs no Rust toolchain or SDK at runtime. Unit tests cover burst detection,
expiration, the stronger early-signal threshold, oracle-update grouping, and the
jump/exit drawdown reserve. Integration results are recorded in `validation.json`.
