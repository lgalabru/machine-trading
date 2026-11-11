# Changelog

Releases of the `solana-foundation/machine-trading` dataset. Names are as
each release shipped them: the CLI was `mth` from 0.5.0 until it became
`mtx`.

- **Unreleased.**
  - **Bots are strategy components.** The verifier grades `/app/bot/bot.wasm`,
    a WebAssembly component the grader loads and calls (RFD 0002), and
    nothing else (`mtx grade --components-only`, in `test.sh` and
    `mtx-practice`). The starter is one (`src/lib.rs`; `build.sh` builds it
    for `wasm32-wasip2`, which the image now carries), and each task's
    instructions give the component's contract instead of the process's:
    no files, environment, network or clock, ten billion instructions a
    call, 256 reads a step, no turn deadline, and a trap ends the strategy
    for the session. The oracle solutions are
    the reference bots ported to components (`bots/*-wasm`): on each task's
    practice set, each earns its process reference's PnL to the cent. The
    references the hidden sets are scored against stay processes.
  - **What a component reads.** `Session` gains the venue's metadata
    (`venue`, read with `mtx_kit::venue::PhoenixVenue`, feature `venue`),
    the starting oracle, the trading key's public key (`authority`) and the
    task's variables (`var("MTX_REOPEN_SLOT")`). `Market` gains Phoenix's
    index and the book's best bid and ask, from the read the mark comes
    from. `rpc_account` and `rpc_accounts` return accounts' data, and
    `mtx_kit::log` writes the strategy's log, in a component or a process.
  - **One `Strategy`, synchronous.** `mtx_kit::Strategy`'s `start`,
    `step` and `end` are plain functions, and `step` takes `&mut Orders`.
    The same strategy runs as a process (`mtx_kit::run`) and as a
    WebAssembly component the grader calls (`mtx_kit::export!`, feature
    `component`; RFD 0002). `Orders::trader()` and `Session::env` are gone:
    `mtx_kit::rpc(method, params)` reads the chain, synchronously, at the
    step, for the reads in `mtx_kit::RPC_METHODS`. `Session` is plain data
    (steps, slots, symbol, start mark, cap, drawdown limit, mandate, seed),
    `Market::budget_left` is gone, and `mtx_kit::Side` is the kit's own,
    with `.into()` for Phoenix's builders.
  - **Reads are at the step.** The guard served a read at `processed` only
    when it named a commitment; one that named none got the node's
    default, 31 slots behind the world. Every read is now at `processed`.
  - **The order sheet counts resting orders.** `plan()` cut each resting
    order against the position alone, so two bids of 50 under a cap of 70
    both went out, and filling both broke the mandate. It now counts every
    order resting, from earlier steps (`Market::resting_bids` and
    `resting_asks`, the run loop's ledger since its last cancel) and earlier
    on the sheet, as filled. `room_to_buy()` and `room_to_sell()` count them
    too, by the mandate's own rule (`Allowance::room` in `mtx-core`), which
    the grader's audit now shares.
- **0.12.0.** New tasks on new scenario hooks in the engine (book
  premiums, a taker tape, per-session funding, off-hours markets; each
  opt-in, so flash-crash's and market-making's sessions are unchanged).
  - **`funding-carry-spcx`.** The SPCX book trades at a premium or a
    discount to its index, and Phoenix's funding pays the side against
    it. A good bot fades the premiums the crowd's positioning opened,
    follows the ones the index follows, and learns from the session's own
    earlier premiums which kind of day it is.
  - **`weekend-gap-spcx`.** SpaceX's own market is closed for the
    weekend, so Phoenix prices the SPCX index from its own book until the
    market reopens. A good bot reads which weekend flow knows where SPCX
    will open, and holds that gap into the reopen within its loss limit.

- **0.11.0.** Three leaks closed, tasks named for their skill, and OpenAI
  models. Design and evidence: [bench/design/0.11.0.md](../design/0.11.0.md).
  - **The SDK is `mtx-kit`.** `luce` and `mtx-botkit` are one crate,
    `mtx-kit` (`/opt/mtx/sdk/crates/kit`), on `mtx-core`
    (`/opt/mtx/sdk/crates/core`: the mandate, the turn protocol and the
    types they share). Bots write `mtx_kit::` for `mtx_botkit::` and
    `mtx_botkit::luce::`; `Trader::luce` is `Trader::inner`, and
    `Phoenix::luce_market` is `Phoenix::exact_market`. Nothing a bot sees at
    run time changes, and grades are unchanged.
  - **Renamed:** `phoenix-flash-crash` is now `flash-crash-spcx` and
    `phoenix-market-making` is `market-making-spcx`: the skill, then the
    instrument. `mtx bench review` and `compare` line a task up across the
    rename.
  - **flash-crash: big moves land in unequal parts.** Phoenix moves its
    price at most 4% a step, and 0.10.0 cut a bigger at-once move into
    equal steps, so the first step said how many followed. 0.10.0's best
    bots went full size after it. Such a move now lands in parts of unequal,
    random sizes on consecutive steps.
  - **flash-crash: fizzles give back 40% to 100% of their move**, drawn
    uniformly, not all of it to the exact level they left. Fading fizzles
    was most of 0.10.0's best bots' edge (about $9k of Opus's $15k gross on
    a ladder set), and nearly riskless.
  - **market-making: informed orders look like noise.** In 0.10.0 they ran
    2–4× noise sizes and limits, so each one stood out as it traded, and the
    best bots took the position cap with every burst (14–20× the
    reference). Each informed order now draws its size, limit and account
    as a noise order does; only its side and timing differ (6 to 14 orders,
    6 to 30 slots ahead of the jump). Fake-outs are drawn exactly like
    informed bursts, zero to two a session.
  - **The 0.10.0 winners join the exploit checks** (`*-0.10`), and the
    ladder grades them on fresh sets of the new families.
  - **The starter is a strategy on a run loop.** `/app/bot` is a
    `Strategy` on `mtx_botkit::run`, which answers the grader, reads the
    market before every step, and sends the step's order sheet cut to the
    position cap; the instructions show it instead of the hand-written
    `Turns` loop, which `mtx-botkit` still has. `step_offline` tests a
    strategy without a chain. Agents get the same trading primitives as
    before, with less plumbing to write, so scores may not compare with
    0.10.0's on this alone.
  - **`mtx-practice` runs three sessions at a time** (`MTX_JOBS` to
    change); grades are the same however many run. Practice sets are redrawn
    from the new families and recalibrated.
  - **Benchmarking:** `just bench` takes `provider/model` and runs OpenAI
    models in Codex (pinned, no web), five attempts per model by default,
    and writes the batch's plan (harnesses and versions, attempts, time,
    metric, commit) before the first trial. The review shows a 90% interval
    over attempts.
  - Image `0.11.0`: `mtx` with the new families, `mtx-practice`.

- **0.10.1.** Agents know their time budget. In 0.10.0, 11 of 18 runs
  used their whole hour, most of them waiting on a parameter sweep of
  practice grades when they were stopped: Claude Code doesn't know its
  time limit, and nothing said that the bot in `/app/bot` when time runs
  out is what's graded. Both instructions now say so, and to keep a
  working, built bot there all along. Nothing else changes: same images,
  graders and scenario families.

- **0.10.0.** Verifiers that finish, orders that all run, and reports a
  reviewer can dig into.
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
