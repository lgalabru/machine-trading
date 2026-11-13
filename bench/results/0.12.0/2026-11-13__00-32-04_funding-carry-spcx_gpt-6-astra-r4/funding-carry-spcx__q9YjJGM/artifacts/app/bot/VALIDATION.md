# Validation

Results are from local Phoenix sandbox runs on practice and independently generated scenarios.

| Set | Sessions | Total PnL (USDC) | Maximum drawdown (USDC) |
| --- | ---: | ---: | ---: |
| Supplied practice | 6 | 2,119.24 | 181.86 |
| Additional generated | 12 | 7,490.29 | 155.02 |
| Augmented index noise | 12 | 8,707.00 | 311.94 |
| Held-out drawdown tails | 6 | 2,087.59 | 447.35 |
| High-funding cases | 3 | 1,266.11 | 142.71 |
| Fresh augmented comparison | 12 | 6,475.77 | 241.46 |

All 51 distinct sessions were profitable. Total PnL: 28,146.00 USDC. Worst session: 13.21 USDC.

- Peak absolute position: 70 SPCX.
- No disqualifications, refused RPC calls, outside-market positions, or liquidatable endings.
- No failed tape orders.
- Two-session, two-run replay: identical reports.
- Exported Rust predictions matched an independent Python implementation within 5e-13 on both original and augmented paths.
- Maximum measured instructions in one call: 76,338,071.

Component SHA-256: `71c501d5a94a6a27b17a2a48f0b73031d7f0b91236dd98ac54e417c0eeedf323`.

The component is already built. `just build` rebuilds it with locked dependencies for `wasm32-wasip2`.
