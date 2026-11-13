# SPCX market maker

Rust strategy component for the Phoenix SPCX sandbox. The grader entry point is
`bot.wasm`; the source is `src/lib.rs`.

The strategy:

- Anchors quotes and account valuation to the oracle, rather than the mark.
- Reads Phoenix's recent-trade ring and trader accounts through allowed read RPCs.
  It identifies passive makers and measures changes in other traders' positions,
  so flow detection is not limited to fills against its own quotes.
- Detects concentrated directional flow over several short windows. Strong
  bursts change the inventory target; ordinary quoting uses smaller inventory.
- Estimates persistent drift with a mixture of flat, upward, and downward price
  processes, incorporating a discounted measure of order-flow imbalance.
- Restricts jump bets to the published session timing, with a cooldown after
  observed jumps and a permanent drawdown brake. The brake cancels
  quotes and closes exposure, with substantial room below the session limit.

Only the session's SPCX order sheet is used for trading. The component needs no
files, environment variables, network access, or runtime build.

## Build

```sh
just build
```

This builds `wasm32-wasip2` and copies the result to `bot.wasm`. It rebuilds the
strategy crate explicitly to avoid collisions in the shared Cargo target cache.

## Evaluation

`mtx-practice` runs the supplied practice sessions. Additional generated sessions
were used for strategy comparisons and separate validation. Evaluation reports
for the selected strategy are in `results/`. `validation-summary.json` records
the final component SHA-256 and all completed validation batches.

Final validation: 72 fresh generated sessions, **$1,202.59 total PnL**,
**0.06968 reward**, no disqualifications or refused RPC calls. Maximum
drawdown was **$75.87** and peak absolute inventory was **19.51 SPCX**.
