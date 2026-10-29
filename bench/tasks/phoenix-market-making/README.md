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
  half have one to three jumps of 0.6–2.5%, either way, and a quarter trend.
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
  receives only `/app/bot` and replays twelve hidden scenarios (three calm,
  six with jumps, three trending), one at a time, each on a fresh surfnet.
  Before the bot starts, the takers are onboarded, the makers widened, and
  every cheatcode locked for good. The bot runs as an unprivileged user
  behind an RPC proxy that forwards standard Solana methods only.
- **Reward:** `clamp(Σ bot PnL / Σ spread bound, 0, 1)`. A scenario's spread
  bound is every taker order filled at the competitor's half-spread with
  nothing lost to informed flow, inventory or fees. No bot reaches it. The
  reference is reported as a baseline.
- **Risk:** positions are capped at 20 SPCX (about 0.3× collateral), and a
  session may not draw down more than $250 from its peak. Breaking either,
  trading another market, a refused RPC call, or ending liquidatable scores
  0. A session where more than a tenth of the takers' orders fail fails the
  grade instead of scoring it: the bot didn't see the flow its scenario
  describes.

## Checks

| Agent | Reward | What it shows |
| --- | --- | --- |
| oracle (the reference maker) | ~0.02 | The task is solvable and the reference is calibrated |
| nop (starter bot) | 0.0 | Doing nothing earns nothing |
| cheater | 0.0 | Cheatcodes through the proxy disqualify; `/tests` is unreadable; writing the reward file counts as tampering; ignoring SIGTERM doesn't extend the session |
| sneaky | 0.0 | Cheatcodes, airdrops and `exit` on the surfnet's own port are refused |

The reference quotes 5 bp from the oracle, far inside a competitor that
quotes 8–30 bp, so it keeps only a fraction of the spread the bound counts:
it made $11.89 over the hidden set against a $593 bound. A bot that reads
the book and quotes just inside the competitor earns several times more per
fill; the headroom is the point. A passive baseline (the reference without
skew, drift or backing off) lost money in development on jump and trend
sessions, which shows the flow's adverse selection is real.

## Realism limits

- **Scripted flow.** The takers' orders are fixed in advance: they don't
  react to the bot's quotes, and informed flow knows the next jump rather
  than inferring it.
- **A simple competitor.** The mainnet makers re-center on each move and
  otherwise stand still: they don't manage inventory or react to fills.
- **Transaction fees aren't counted.** The account is valued in USDC; the
  SOL a bot spends on requotes isn't charged against it.
- **No funding.** Sessions last a few minutes; perp funding isn't simulated.
- **One market, one snapshot.** Depth and fees are mainnet's on 2026-10-29.

## Held-out data

The hidden scenarios ship in `tests/scenarios` with the task, so treat scores
on this public version as a development benchmark, not a held-out
evaluation.
