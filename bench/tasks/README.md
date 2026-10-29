# solana-foundation/machine-trading

Trading tasks for agents on Solana. In each task the agent writes a bot,
and the bot is graded on market scenarios replayed on an offline mainnet
snapshot restored with [Surfpool](https://github.com/solana-foundation/surfpool).

The verifier runs with no network and receives only the bot. Every cheatcode
is locked before the bot starts, and the bot reaches the chain through a
proxy that forwards standard Solana RPC methods only. Rewards are the bot's
total PnL over the hidden set, as a fraction of a hindsight bound no bot
can reach, with a per-session drawdown limit.

| Task | Market | Scenario |
| --- | --- | --- |
| `phoenix-flash-crash` | Phoenix SPCX (SpaceX) perpetuals | Crashes and squeezes in legs of 3–6%: most revert, some stall, some keep going |

## Changelog

- **0.5.0.** The project is now Machine Trading: the dataset moves from
  `solana-foundation/open-trading` to `solana-foundation/machine-trading`,
  and the CLI, SDK and contract variables from `ott`/`OTT_*` to `mth`/`MTH_*`.
  `phoenix-flash-crash` trades SPCX, Phoenix's SpaceX perpetual, instead of
  SOL, on a snapshot from 2026-10-29, with a 70 SPCX position cap (about 1×
  collateral, as before). The family's paths no longer give the script
  away: moves ramp in uneven steps at uneven intervals, legs (3–6% each,
  up to three) are drawn independently, pauses and holds share one range,
  the session ends a random 10–40 s after the last move, and the mark
  wobbles between moves. Opus and Fable had reached 3.5–3.9× the reference
  on 0.4.0 by reading equal steps and a fixed tail.
- **0.4.0.** `phoenix-flash-crash`: shocks go both ways, crashes and
  squeezes 50/50 (when every scenario crashed, shorting the open made 2.4×
  the reference with no skill). The reward is PnL over a hindsight bound
  (the position cap held the right way through every move, at no cost), so
  it doesn't saturate; the reference is a baseline at ~0.2. A session may
  not draw down more than $1,000 from its peak, and risk diagnostics
  (worst session, tail loss, drawdown, profitable share, Sharpe) are
  reported. Bots build on `luce`, re-exported by `mth-botkit`.
- **0.3.0.** `phoenix-flash-crash`: a stronger reference that scales into the
  crash in two tranches and holds for the rebound, with a slide stop (the
  0.2.0 reference left most of the rebound on the table, and agents beat it
  5×). Verifiers grade one session at a time, on a slim 384 MB runtime
  image.
- **0.2.0.** `phoenix-flash-crash`: crashes now stall or keep sliding as
  well as rebound; twelve stratified hidden scenarios scored as a set
  (`Σ PnL / Σ reference PnL`); a 100 SOL position cap, with the reference
  trading at it; sessions restored at the snapshot's slot and time, with
  every cheatcode locked before the bot starts.
- **0.1.0.** `phoenix-flash-crash`: three rebounding crashes, scored per
  scenario.

The hidden scenarios ship inside each task's `tests/`. On this public
version, treat scores as a development signal, not a held-out evaluation.
