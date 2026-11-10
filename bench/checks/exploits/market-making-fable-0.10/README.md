# SPCX market-making bot

`run.sh` starts `bin/bot`, a self-contained native executable built by
`build.sh` from `src/main.rs` (Rust, on `mtx-botkit`). Run `./build.sh` after
every change.

## What it does each turn

1. **Reads** the oracle price (perp asset map), the FIFO order book (to find
   the competing maker's quotes and its own), its position and equity
   (Hawkeye views), the market's trade buffer (fills against FIFO orders),
   and the positions of the session's other trader accounts.
2. **Taker flow.** The trade buffer does not record fills against Phoenix's
   splines, so the bot enumerates Phoenix trader accounts once at start
   (`getProgramAccounts`), keeps those funded during the session's setup
   (the eight takers and the competing maker), and treats each account's
   position change per step as one taker order. The competing maker is
   excluded (the last-funded account, confirmed by any fill naming it as
   maker).
3. **Burst detection.** Within a 12-slot window, four or more same-side
   orders of at least 1 SPCX totalling at least 8 SPCX, with little flow the
   other way, mark informed flow ahead of a jump (or a fake-out). The bot
   crosses the spread to lean 19 SPCX that way, stops quoting against the
   move, and holds until a jump lands or 30 slots pass after the last large
   order. Bursts inside 30 slots after a jump (none can follow within 60) or
   near the session's ends are ignored. Thresholds were tuned offline on 200
   generated scenarios.
4. **After a burst** the position is worked out passively: offers at the
   oracle on the reducing side, nothing on the other side. No crossing,
   unless drawdown exceeds 100 USD (position cut to ~8.5) or 150 USD (flat,
   no more leans).
5. **Trend.** A t-statistic of per-step oracle drift (jumps excluded) plus
   flow imbalance sets a slow target of up to 12 SPCX, acquired by crossing
   4 SPCX per step.
6. **Quoting.** One tick inside the competing maker on each side, never
   wider than the splines' innermost level, never tighter than 2 ticks,
   0.8 SPCX a side, skewed and resized by inventory. A side a large
   aggressive order just hit is pulled for 6 slots. Requotes (cancel-all plus
   a multi-order instruction in one transaction) happen only when the desired
   quotes change or an own order was filled.
7. **Limits.** Every resting size and market order is capped so no fill can
   take the position past 19.5 SPCX (mandate 20). Market orders cancel own
   quotes in the same transaction to avoid self-trade aborts.

Only standard RPC methods are used: `getAccountInfo`, `getMultipleAccounts`,
`getProgramAccounts`, `simulateTransaction`, `sendTransaction`,
`getSignatureStatuses`, `getLatestBlockhash`.
