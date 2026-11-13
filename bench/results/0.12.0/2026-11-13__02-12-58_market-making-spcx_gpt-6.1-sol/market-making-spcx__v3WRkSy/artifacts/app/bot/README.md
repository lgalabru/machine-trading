# SPCX market maker

`bot.wasm` is the built `wasm32-wasip2` strategy component. Rebuild with `just build`. The build replaces the component atomically.

The strategy quotes around the oracle, competes with visible best prices, and skews quotes against inventory. Ordinary inventory stays small. A directional signal can support a temporary position of 18 SPCX, within the session's 20-SPCX cap. Continued evidence refreshes its expiry; a large oracle move resets the signal and triggers an exit.

The signal reads Phoenix market-order instructions from newly completed blocks, including orders sent to competitors. It excludes the bot's own trades. A fixed tree ensemble estimates directional jump probabilities from buying and selling counts over 4–48 slots, observed flow rate, elapsed and remaining session time, time since the last observed jump, recent price drift, and observed jump count. The model was fitted on 1,800 generated scenarios and checked on a separate 300-scenario set. Its coefficients are compiled into `src/model.rs`; it needs no model files, network, or runtime dependencies beyond the strategy SDK.

Risk controls cancel existing orders before execution or quote replacement and size orders within inventory room. The strategy reads its Phoenix trader account and values collateral plus the position at the oracle. A drawdown above $110 cancels quotes and flattens, reserving room below the $250 mandate limit for a sudden move and exit costs. All trading uses the order sheet; RPC calls are read-only.

A fresh 72-session sandbox comparison produced $2,745.59 aggregate PnL, versus $2,595.57 for the rule-based candidate. The selected component and its source are included here.

The complete [validation report](validation.json) covers all 72 sessions: no disqualifications, refused RPC calls, traps, or liquidations. Maximum drawdown was $74.87 and peak position was 18.000 SPCX. Replays reproduced the original PnL exactly. The report records the tested component checksum.
