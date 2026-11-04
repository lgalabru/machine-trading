# Changelog

Releases of the `solana-foundation/machine-trading` dataset. Names are as
each release shipped them: the CLI was `mth` from 0.5.0 until it became
`mtx`.

- **0.10.0.** Reports a reviewer can dig into.
  - **`mth` is now `mtx`:** the CLI, the SDK crate (`mtx-botkit`), the
    `MTX_*` variables, `/opt/mtx` in the images, the `mtx-bot*` users and
    the turn protocol, now `mtx-turn-1`.
  - **Session detail files.** Next to `report.json` the verifier writes
    `sessions/bot/<session>.json.gz` and `sessions/reference/<session>.json.gz`.
    Each holds the scenario as drawn, with what it was drawn as (a shock's
    shape, which taker orders were informed), and every step: mark, index,
    planned level, best bid and ask, position, value and PnL, and how long
    the turn took. They also hold every transaction the bot sent (where it
    executed, its instructions decoded with each order's side, price and
    size, compute units, and the logs of failures), every RPC method it
    called, its refused calls and late sends, and the takers' orders. They
    hold the hidden scenarios, so the grader writes them only once every bot
    has stopped, and clears whatever a bot left in their place first.
  - **Duplicate sends are flagged.** Within a turn the blockhash doesn't
    move, so the same order sent twice is the same transaction, and the
    chain runs it once. A detail file marks every repeat.
  - **Equal orders within a turn all run.** Phoenix orders went out with
    client order id 0, so two equal orders in a turn were the same
    transaction, and the chain ran it once. The flash-crash reference sends
    its position in equal slices and lost about 46% of them. Every order now
    carries its own id (counting from 1 per trader, so sessions still
    replay). The flash-crash practice set is recalibrated; market-making's
    reference sent no repeats and is unchanged.
  - **The verifier stays under its memory limit.** With glibc's allocator,
    the grader held on to what each session freed and peaked near 4.7 GiB
    on the reference's pass alone (four sessions at a time), so under load
    0.9.0's verifiers were killed at their 6 GiB limit and wrote no reward.
    `mtx` now runs on mimalloc: 2.1 to 3.1 GiB over the same 48 sessions.
  - Session reports gain `wall_ms`.
  - **`mtx bench review <path>`** browses a batch, a job or a trial;
    `mtx bench publish` keeps a batch's results in `bench/results/<version>`,
    and `mtx bench compare <a>...<b>` compares two (see
    [docs/development.md](../../docs/development.md)).

- **0.9.0.** Grades replay, hidden sets stay hidden, and a ladder checks
  that the tasks rank strategies the way a trader would.
  - **A lockstep world.** The grader moves the world one step (two slots) at
    a time: the step's blocks land the scenario's move, then its takers and
    its maker act, then the bot gets the step over a turn socket
    (`mth-turn-1`, `mth_botkit::Turns`) and the world waits for it, up to
    5 s a turn. The same bot on the same session trades the same way every
    time; in 0.8.0 the grader's maker raced the bot in real time, and the
    same market-making reference made $19.6 in one run and $49 in the next.
    Sessions also run as fast as the bot answers: a 72-session flash-crash
    grade takes minutes, not hours. A bot that never answers the hello runs the old way.
  - **Hidden sets drawn per batch.** No task ships its hidden scenarios. The
    verifier draws them from the batch's secret, passed by Harbor, and
    refuses to grade without one; reports carry only the secret's SHA-256.
    The reference runs first on the same sessions, and a reference that
    doesn't run cleanly voids the grade. Both tasks grade 72 sessions (up
    from 36 and 12): at fewer, the ladder couldn't reliably tell the
    references from simple bots. The verifier's limit is 4 h, and it
    stops at a bot's first disqualifying session.
  - **A private surfnet.** The surfnet serves RPC only at a secret path, so a
    bot that finds its port gets a 404 for everything, standard methods
    included: the guard is its only way in, and the guard holds its sends to
    its turn.
  - **`phoenix-market-making`'s informed flow bites.** On 0.8.0's flow, a
    naive quoter one tick inside the competitor, never stepping back, beat
    the reference on fresh sets: the bursts cost it less than backing off
    did. Now about 85% of jumps are announced (up from 60%), by 8–16 orders
    (6–12) of 2–4× the noise size (1.5–3×), and jumps run 1.5–4% (0.8–3%).
    The reference trades with a burst it reads in its fills, a few units the
    burst's way, and pulls the quote the burst was hitting; it no longer
    takes its own unwinding for a burst (it did, and leaned back and forth
    on its own).
  - **A stronger `phoenix-flash-crash` reference.** The old one, two fading
    tranches, made $525–$759 on 36 fresh sessions: too little for its ratio
    to mean anything. The new one rides each move while it runs and fades
    it once it stops ($647–$2,777 on the same sets).
  - **`skill`** joins the reward's diagnostics: the bot's PnL over the
    reference's, 0 if disqualified.
  - **The ladder** (`just ladder`, `bench/checks/ladder`) replaces `just
    exploit-checks`: blind bets, a dip-buyer, momentum, naive and skewed
    quoters, the reference, a regime oracle (flash-crash), a flow oracle
    (market-making: told which taker orders are informed) and the frozen
    exploit bots, graded on fresh 72-session hidden sets, with an order rule
    each set must keep; the exploit bots must stay below the oracles.
  - **The bench** (`just bench`) draws one secret per batch, pins the Claude
    Code harness (no web, no memory across trials), runs at a time-limit
    multiple, and reports skill with 90% bootstrap intervals over sessions
    and runs (`bench/report.py`). Trials with no reward are void, not 0.
  - Each task has a design record (`DESIGN.md`), and `just leak-audit`
    checks the agent's image holds no answer.

- **0.8.0.** Both tasks stop paying for knowing how their scenarios are
  made. In 0.6.0 and 0.7.0, Opus and Fable beat the reference about 6× on
  `phoenix-flash-crash`, profitable in every session, by riding legs that
  always ran at least 3% in several steps and by dating the aftermath from
  the session's end; on `phoenix-market-making` they scored the maximum,
  winning every session, by quoting a tick inside makers that never
  reacted and trading the informed bursts that led every jump. Now:
  `phoenix-flash-crash` legs run 0.6–8% (log-uniform), about 70% of moves
  land at once, quiet stretches carry fizzles that give their move back, and
  a session's length is drawn before its path, so it can end mid-shock. The
  hidden set grows from 12 to 36 scenarios: a good bot's edge is about $50 a
  session against swings of about $240, and twelve ranked bots by the luck
  of the draw. `phoenix-market-making` gets competition: the grader runs a
  maker of its own that undercuts a better quote by a tick, down to a floor
  of 2–6 bp, and eases back out when nobody is inside it; and every taker
  order carries its own limit, so a maker quoting wider than a taker will
  pay doesn't fill it. In 0.7.0 the makers were static and the takers
  price-blind, so quoting a tick inside collected most of the spread on
  every order, unopposed. It also announces about 60% of jumps, sends zero
  to three fake-out bursts a session in any regime, and sizes informed
  orders closer to noise; its bound adds the hindsight bound to the spread
  bound (now each order at its own limit or the competitor's half-spread,
  whichever is closer), so no bot can pass it. Against the grader's maker
  the reference roughly breaks even. New release check, `just
  exploit-checks`: the bots that exploited past versions must lose some
  sessions, and on flash-crash stay within 3× the reference (see
  `bench/checks/exploits`).
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

Since 0.9.0, no hidden scenario ships with a task: each batch's are drawn
inside the verifier from a secret its runner holds. Before 0.9.0 they
shipped in each task's `tests/`, and scores on those versions are a
development signal, not a held-out evaluation.
