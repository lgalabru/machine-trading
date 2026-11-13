# SPCX weekend bot

`bot.wasm` is the built `wasm32-wasip2` strategy component. Rebuild with:

```sh
cd /app/bot
just build
```

The strategy reads confirmed Phoenix transactions, decodes IOC side and size,
and tracks the eight takers separately. Its causal features describe order-flow
persistence, timing, price trends, temporary impact decay, and quiet-book moves.
Three embedded tree ensembles forecast the remaining closed-period move and
the reopening return. The models were fitted on independently generated
weekend-gap scenarios, not on the hidden evaluation scenarios.

During the closed period the bot follows a smoothed forecast with entry/exit
hysteresis. In the last 20 slots it switches to the reopening forecast. Position
changes use five-share slices and at most 30 shares per turn. After the reopen,
it holds the position unless its risk stop applies.

Risk controls use decoded Phoenix collateral and position balances, including
execution costs, rather than the turn's approximate PnL. The bot stops at 60%
of the drawdown limit, or 70% using estimated liquidation value. Before the
reopen it limits exposure using a ±14.5% stress move and an 80% drawdown budget.
The order planner also enforces the session's position cap. It only trades SPCX.

The component has no runtime file or network dependencies. Its only extra
observations use the allowed `getSignaturesForAddress`, `getTransaction`, and
`getAccountInfo` RPC methods. Model weights are embedded at build time.

`src/lib.rs` contains observation, execution, and risk handling;
`src/features.rs` contains feature construction and model evaluation.

Run `just test` to check the Rust feature pipeline and all three embedded models
against an independently computed training replay. The test uses only `rustc`.

Final validation on 72 fresh generated scenarios (seed `50194726`) produced
$16,762.02 total PnL, reward `0.21106`, and maximum drawdown of $347.22.
There were no disqualifications, refused RPC calls, traps, other-market positions,
or liquidation events. Maximum position was 70 shares. These are generated
validation results; the hidden evaluation is separate. See `validation.json`
and `validation-report.json` for the recorded results, and `model-info.json`
for model provenance and weight hashes.
