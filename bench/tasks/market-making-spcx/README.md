# solana-foundation/phoenix-market-making

The agent's bot makes markets in Phoenix's SPCX (SpaceX) perpetual, against
Phoenix's own makers and grader-run taker flow.

| | |
| --- | --- |
| Market | Phoenix SPCX perp, mainnet snapshot of 2026-10-29 |
| Sessions | 72 hidden per grade (18 calm, 36 with jumps, 18 trending); 6 practice |
| Mandate | 20 SPCX position cap (about 0.3× collateral); $250 loss limit |
| Bound | Spread bound plus hindsight bound (see [Scoring](#scoring)) |
| Reference | Reward ~0.01. It quotes a tick inside the competitor, skews against inventory, and backs off after one-sided fills. |

How every task is scored, and how risk is enforced, is in the
[dataset README](../README.md#how-a-bot-is-judged).

## The skill

A good bot earns the spread from uninformed takers while reading its own
fills. It skews against its inventory, and widens or steps back when the
flow turns one-sided, so the informed orders ahead of a jump don't run it
over.

- **Wins:** tight quotes when the flow is two-sided, a lean against
  inventory, and stepping back from bursts.
- **Loses:** tight symmetric quotes, which fill the most and lose it all to
  jumps and trends; and quotes too wide or too sparse, which fill nothing.

## The market

- **The competitor** is Phoenix's own mainnet makers on SPCX. At setup their
  liquidity is pushed out to a half-spread drawn per session (8–30 bp).
  They keep their depth curve and re-center on every price move. At equal
  prices, the bot's resting orders fill first. A grader-run maker undercuts
  better quotes by a tick, down to a floor, and eases back out.
- **The price** walks every few slots. A quarter of the sessions stay calm,
  half have one to three jumps of 1.5–4% either way, and a quarter trend.
- **The flow** is grader-run takers sending ordinary Phoenix market orders
  from a shared pool of eight accounts. It mixes noise on both sides,
  informed bursts ahead of about 85% of jumps, leaning flow in a trend, and
  fake-out bursts with no jump after them. Informed and noise orders
  overlap in size and share accounts, so telling them apart takes reading
  the market, not the generator.
- **Turn order:** the takers' orders due each step trade against the quotes
  the bot left, then the step's move lands, then the grader's maker
  requotes, then the bot takes its turn.

## Scoring

- **Bound:** a session's bound is its spread bound plus its hindsight bound.
  The spread bound is every taker order filled at the competitor's
  half-spread, with nothing lost to informed flow, inventory or fees. The
  hindsight bound is the 20 SPCX cap held the right way through every move.
  A maker earns from both, so a bound on the spread alone is one a bot
  could pass, as 0.7.0's was.
- **Valuation:** the account is valued at the scenario's oracle price, not
  Phoenix's mark. A maker's own best quote pulls the mark, so valuing at the
  mark would pay a bot for pushing it. The SDK's `Trader::oracle_usd` reads
  the same oracle.
- **Void sessions:** a session where more than a tenth of the takers' orders
  fail voids the grade. The bot didn't see the flow its scenario describes.

## Baselines

| Agent | Reward | What it shows |
| --- | --- | --- |
| oracle (the reference maker) | ~0.01 | The task is solvable, and the reference is calibrated |
| nop (the starter bot) | 0.0 | Doing nothing earns nothing |
| cheater | 0.0 | Cheatcodes through the proxy disqualify; `/tests` is unreadable; writing the reward file counts as tampering; ignoring SIGTERM doesn't extend a session |
| sneaky | 0.0 | The surfnet's own port answers nothing without its secret path |

`just ladder market-making` grades the ladder (`bench/checks/ladder`) on
fresh hidden sets. Naive and skewed quoters must score below the reference,
and the reference below a flow oracle that is told which taker orders are
informed. The frozen exploit bots must score below the flow oracle.

On two fresh 72-session ladder sets the reference made $279 and $151, the
naive quoter lost $11 and $41, and the flow oracle made 14–30× the
reference. Agents separate in that headroom: reading the tape beyond their
own fills, sizing the lean, and telling bursts from noise.

## Realism limits

- **Scripted flow.** The takers' orders are fixed in advance. Their slots,
  sizes and limits don't react to the bot's quotes, and the informed flow
  knows the next jump rather than inferring it.
- **A simple competitor.** The mainnet makers re-center on each move and
  otherwise stand still. The grader's maker reacts, but simply: it doesn't
  read the tape or manage risk beyond a linear inventory skew.
- **No transaction fees.** The account is valued in USDC, and the SOL a bot
  spends on requotes isn't charged against it.
- **No funding.** Sessions last a few minutes, so perp funding isn't
  simulated.
- **One market, one snapshot.** Depth and fees are mainnet's on 2026-10-29.

## More

- [DESIGN.md](DESIGN.md): the design record (research, variants, levers,
  fairness).
- [instruction.md](instruction.md): what the agent is told.
