# SPCX shock trader

`run.sh` starts the prebuilt native `bin/bot`. Model weights and the Phoenix
client are embedded: runtime needs no compiler, Python, SDK, downloaded
files, or writable bot directory. It uses only the supplied Solana RPC.

Two regression-tree forecasts use causal oracle history, recent returns,
ranges, momentum, and the size and age of observed shocks. Their average
can follow a continuing crash or squeeze and then reverse as the evidence
changes. Entry requires 0.07 percentage points of predicted movement; an
existing position closes when the opposing forecast exceeds 0.05 points.
Weaker signals during quiet pauses require two turns of confirmation before
increasing exposure. Opposing exposure closes immediately. Fresh moves,
strong signals, and risk reductions execute without this entry delay.

Risk control reads actual Phoenix collateral plus unrealized PnL, including
trading costs. Exposure shrinks with drawdown, reserving 18% of the mandate's
loss budget and room for a further 5.5% adverse step. Drawdown above 65% of
the limit permanently halts new exposure. Orders pass the kit's position-cap
checks and execute as 10-share IOC slices. There are no resting orders or
trades in other markets. The final mark values any remaining position.

The predictors were fitted to generated scenarios, with separate scenarios
for model selection and validation. Runtime never reads scenario files.
`models/provenance.json` records model provenance and checksums.

`validation/summary.json` identifies the delivered executable and its direct
chain tests. Other reports in that directory document earlier execution
variants and their comparisons; they are labeled separately in the summary.
The direct tests include adverse-path stress scenarios, whose average PnL
is not representative of randomly drawn sessions. Binary-only, read-only
deployments were tested, as were crash/squeeze symmetry and loss controls.

Build in the supplied development environment with `./build.sh`, which
atomically replaces the executable after a successful build. Run unit tests
with `cargo test --release`. The grader runs the existing binary directly.

The delivered version passed all 74 direct chain sessions and four unit
tests, with no mandate or timing violations. Maximum drawdown was $698.31
against the $1,000 limit. The last 24-session check ran the exact delivered
binary from a read-only directory containing only the executable and launcher.
