# solana-foundation/phoenix-market-making

The agent writes a bot that makes markets in Phoenix SPCX perpetuals (SpaceX
shares). The market is an offline copy of Solana mainnet: Phoenix's SPCX
market, snapshotted on 2026-10-29 and restored into a
[Surfpool](https://github.com/solana-foundation/surfpool) surfnet at the
snapshot's slot and time.

**What it measures.** A good bot earns the spread from uninformed takers
while reading its own fills: it skews against its inventory, and widens or
steps back when flow turns one-sided, so the informed orders ahead of a jump
don't run it over. A bot that quotes tight and symmetric fills the most and
loses it all to jumps and trends; a bot that quotes too wide or too little
fills nothing.

- **The competitor** is Phoenix's own mainnet makers on SPCX, their spline
  liquidity pushed out at setup to a half-spread drawn per scenario (8–30
  bp). They keep their depth curve and re-center on every price move. At
  equal prices, a resting limit order fills before them.
- **The market** walks every few slots. A quarter of the sessions stay calm,
  half have one to three jumps of 1.5–4%, either way, and a quarter trend.
- **The flow** is grader-run takers sending ordinary Phoenix market orders
  from a shared pool of eight accounts: noise either side, informed bursts
  ahead of each jump, leaning flow in a trend, and fake-out bursts with no
  jump after them. Informed and noise orders overlap in size and share
  accounts, so telling them apart takes reading the market, not the
  generator.
- **Valuation** is at the scenario's oracle price, not Phoenix's mark. The
  mark blends in the book, and a maker's own best quote pulls it: valuing at
  it would pay a bot for pushing its own mark. The SDK's
  `Trader::oracle_usd` reads the same oracle.

## Grading

- **Agent environment:** a Rust toolchain, the `mth-botkit` SDK, a starter
  bot that builds and never trades, six practice scenarios, the scenario
  generator (`mth scenario generate --family market-making`), and
  `mth-practice`, which grades the bot exactly as the verifier does.
- **Verifier:** the runtime image, with no toolchain and no network. It
  receives only `/app/bot`, draws twenty-four hidden sessions (six calm,
  twelve with jumps, six trending) from the batch's secret (`MTH_EVAL_SEED`;
  it refuses to grade without one), grades the reference maker on them, then
  the bot, one session at a time, each on a fresh surfnet. Before the bot
  starts, the takers are onboarded, the makers widened, and every cheatcode
  locked for good; the surfnet serves RPC only at a secret path, so the
  bot's only way in is an RPC proxy that forwards standard Solana methods
  only.
- **Turns:** the world moves in steps of two slots and waits for the bot
  between them (`mth-turn-1`): the takers' orders due trade against the
  quotes the bot left, then the step's move lands, then the grader's maker
  requotes, then the bot's turn. (Takers right after a move, before any
  maker could requote, would pick off every quote: a sniper the task
  doesn't describe.) The same bot on
  the same session trades the same way every time; before 0.9.0, the
  grader's maker raced the bot in real time, and the same reference made
  $19.6 in one run and $49 in the next.
- **Reward:** `clamp(Σ bot PnL / Σ bound, 0, 1)`. A scenario's bound is its
  spread bound (every taker order filled at the competitor's half-spread
  with nothing lost to informed flow, inventory or fees) plus its hindsight
  bound (the 20 SPCX cap held the right way through every move). A maker
  earns from both, the spread and the moves it trades or leans into, so a
  bound on the spread alone was one a bot could pass: 0.7.0's was. No bot
  reaches the sum. The reference is reported as a baseline.
- **Risk:** positions are capped at 20 SPCX (about 0.3× collateral), and a
  session may not draw down more than $250 from its peak. Breaking either,
  trading another market, a refused RPC call, or ending liquidatable scores
  0. A session where more than a tenth of the takers' orders fail fails the
  grade instead of scoring it: the bot didn't see the flow its scenario
  describes.

## Checks

| Agent | Reward | What it shows |
| --- | --- | --- |
| oracle (the reference maker) | ~0.01 | The task is solvable and the reference is calibrated (`skill` 1 by construction) |
| nop (starter bot) | 0.0 | Doing nothing earns nothing |
| cheater | 0.0 | Cheatcodes through the proxy disqualify; `/tests` is unreadable; writing the reward file counts as tampering; ignoring SIGTERM doesn't extend the session |
| sneaky | 0.0 | The surfnet's own port answers nothing without its secret path |

`just ladder market-making` grades the ladder (`bench/checks/ladder`) on
fresh hidden sets: naive and skewed quoters below the reference, the frozen
exploit bots within their limits.

The reference quotes one tick inside the competitor around the oracle,
skews against inventory, leans with the drift, and backs off after
one-sided fills. On the hidden set it made $19.62 against a $2,498 bound
(reward ~0.01); in the 0.8.0 Harbor check it made $49.17 and won 11 of 12
sessions. The grader's maker reacts in real time, so the same bot's PnL
moves with timing from run to run. Against that maker, simple quoting
roughly breaks even: the headroom (faster requotes, sizing, reading the
informed flow on the tape without being run over by it) is where agents separate.

## Realism limits

- **Scripted flow.** The takers' orders are fixed in advance: their slots,
  sizes and limits don't react to the bot's quotes, and informed flow knows
  the next jump rather than inferring it.
- **A simple competitor.** The mainnet makers re-center on each move and
  otherwise stand still. The grader's own maker reacts, but simply: it
  undercuts a better quote by a tick, down to a floor, and eases back out
  when nobody is inside it, at a fixed pace. It doesn't read the tape or
  manage risk beyond a linear inventory skew. Before 0.8.0 there was no
  grader maker, and with price-blind takers a bot one tick inside the
  splines collected most of the spread on every order, unopposed: 0.7.0's
  best bots won every session that way.
- **Transaction fees aren't counted.** The account is valued in USDC; the
  SOL a bot spends on requotes isn't charged against it.
- **No funding.** Sessions last a few minutes; perp funding isn't simulated.
- **One market, one snapshot.** Depth and fees are mainnet's on 2026-10-29.

## Held-out data

No hidden scenario ships with the task: each batch's are drawn inside the
verifier from a secret the batch's runner holds, and reports carry only the
secret's SHA-256. The generator is public (agents get it), so the family is
known and the draws aren't. Every trial in a batch is graded on the same
set.
