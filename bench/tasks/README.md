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
| `phoenix-flash-crash` | Phoenix SPCX (SpaceX) perpetuals | Crashes and squeezes in legs of 0.6–8%, most landing at once, among fizzles: most revert, some stall, some keep going |
| `phoenix-market-making` | Phoenix SPCX (SpaceX) perpetuals | Make markets against widened mainnet makers and grader-run takers, some informed, while the price walks, trends or jumps |

## Changelog

- **0.8.0.** Both tasks stop paying for knowing how their scenarios are
  made. In 0.6.0 and 0.7.0, Opus and Fable beat the reference about 6× on
  `phoenix-flash-crash`, profitable in every session, by riding legs that
  always ran at least 3% in several steps and by dating the aftermath from
  the session's end; on `phoenix-market-making` they scored the maximum by
  trading every informed burst, which always led a jump. Now:
  `phoenix-flash-crash` legs run 0.6–8% (log-uniform), about 70% of moves
  land at once, quiet stretches carry fizzles that give their move back, and
  a session's length is drawn before its path, so it can end mid-shock. The
  hidden set grows from 12 to 36 scenarios: a good bot's edge is about $50 a
  session against swings of about $240, and twelve ranked bots by the luck
  of the draw. `phoenix-market-making` announces about 60% of jumps, sends
  zero to three fake-out bursts a session in any regime, and sizes informed
  orders closer to noise; its bound adds the hindsight bound to the spread
  bound, so no bot can pass it. New release check, `just exploit-checks`:
  the bots that exploited past versions must lose some sessions and stay
  near the reference (see `bench/checks/exploits`).
- **0.7.0.** New task, `phoenix-market-making`: quote SPCX against Phoenix's
  own makers, widened to a half-spread drawn per session (8–30 bp), while
  the price walks, trends or jumps 0.8–3%. The grader sends taker flow
  from a shared pool of accounts: noise, informed bursts ahead of each jump,
  and fake-outs. Accounts are valued at the scenario's oracle, not
  Phoenix's mark, which a maker's own quotes pull. The reward is PnL over a
  spread bound: every taker order filled at the competitor's half-spread.
  The SDK adds `Trader::oracle_usd`. `phoenix-flash-crash` moves to 0.7.0
  unchanged but for the grader: a position outside the mandate's universe
  now counts only when a second read confirms it, so a bot that requotes
  between reads is no longer disqualified by mistake.
- **0.6.0.** Each scenario carries a [mandate](../../specs/mandate.md):
  the market the bot may trade, its position cap and its drawdown limit,
  with the same values as before. Bots get a copy in `MTH_MANDATE`, next to
  the variables they already read. New rule: a bot may hold positions or
  orders only in SPCX. Phoenix offline accepted orders on other markets,
  and nothing checked them. 0.6.0 also republishes the rename from `mt` to
  `mth` consistently: 0.5.0's agent image used `mth` while its task files
  and verifier image used `mt`.
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
