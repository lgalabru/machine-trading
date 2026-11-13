# SPCX funding and discovery strategy

`bot.wasm` is the ready-to-run `wasm32-wasip2` strategy component. Rebuild it with `just build`.

The bot forecasts book returns after funding from the observed book/index history, premium decay, index trend, episode history, and the session's on-chain funding period, interval, and cap. Its fixed tree models were trained offline on independently generated scenarios, with additional mean-reverting noise and temporary price excursions. The models are compiled into the component; no training data or Python runtime is needed.

Position changes use entry and exit thresholds to limit turnover. Between oracle updates, the bot filters small book changes caused by consuming quotes. Orders trade only SPCX, in five-share market-order slices, through the SDK's position-cap planner.

Risk control reads the actual trader collateral, virtual quote balance, and unsettled cumulative funding. Exposure shrinks as drawdown increases, reserving room for a 4.5% adverse price move. A drawdown above $650 permanently flattens the strategy for that session. The strategy avoids new entries and reversals during the final four seconds and leaves open positions for final valuation.

- `src/lib.rs`: execution, live funding/account reads, and risk control.
- `src/features.rs`: causal price and episode features.
- `src/model.rs`: immutable forecast trees.

The component uses only `getAccountInfo` RPC reads. It makes no file, network, clock, or environment calls during a session.

## Validation

The selected component made $2,514.83 across the six provided practice sessions, and $45,281.03 across 72 fresh generated sessions with additional noise. Maximum observed drawdown across these runs was $319.44. There were no disqualifications. Reports and the matched comparison are in `validation/`.
