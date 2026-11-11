# Design record: weekend-gap-spcx

Written to `skills/trading-task-design/references/design-record.md`. The
README says what the task is; this says why it's built this way, and what
each version learned.

## Thesis

A good bot profits here because it works out where SPCX will open from the
perp's own weekend: which off-hours flow knows something, how far it has
taken the perp, and whether it's done. It holds that gap into the reopen,
sized so that a wrong read can't take it past its loss limit, and it doesn't
mistake Phoenix's closed-period index, a worker's order or thin-book drift
for information.

## Expert research

- **Why it's a real expert task.** Equity perps on Solana venues trade
  around the clock while their underlyings don't. Phoenix prices an equity
  perp's index from external markets 24/5 (Sunday 8 PM to Friday 8 PM ET);
  over weekends and holidays it switches to its own pricing, an EMA of its
  own book's impact price with a one-hour time constant, bounded at 1/max
  leverage around the last external price (6.67% for SPCX), and at the
  reopen the index snaps to the external price
  ([index price](https://docs.phoenix.trade/phoenix/real-world-assets/index-price.md),
  [impact pricing](https://docs.phoenix.trade/phoenix/real-world-assets/impact-pricing.md),
  [mark price and bounds](https://docs.phoenix.trade/phoenix/real-world-assets/mark-price-and-bounds.md),
  [market specs](https://docs.phoenix.trade/phoenix/real-world-assets/market-specs.md)).
  Over the weekend the perp is the only price discovery there is, and
  desks that trade it are making the market's call on Monday's open. The
  same problem is old in equities: after-hours trading is thin but carries
  real price discovery, and per trade it is more informative and noisier
  than the day's (Barclay and Hendershott, *Price Discovery and Trading
  After Hours*, RFS 16(4), 2003; *Liquidity Externalities and Adverse
  Selection: Evidence from Trading after Hours*, JF 59(2), 2004). French and
  Roll (*Stock Return Variances*, JFE 17(1), 1986) showed that most of a
  stock's variance arrives through trading, not through the calendar: what
  the off-hours trade knows is what the open will price.
- **The expert's method.** Separate the off-hours move into the part that
  will survive the open and the part that won't. A trade's impact that
  persists is information; impact that decays is price pressure from
  uninformed or liquidity-motivated flow (Kraus and Stoll, *Price Impacts of
  Block Trading on the NYSE*, JF 27(3), 1972; Hasbrouck, *Measuring the
  Information Content of Stock Trades*, JF 46(1), 1991, the permanent and
  transitory decomposition; Kyle, *Continuous Auctions and Insider Trading*,
  Econometrica 53(6), 1985, for how an informed trader moves the price toward
  its value). On chain every order is signed, so the decomposition can be
  made per account. Then: where the informed flow has taken the perp is a
  floor on the open, not the open, while that flow is still trading; and
  the venue's own index, in a closed period, is the perp's price smoothed,
  not a second opinion. Size the position into the reopen by what the loss
  limit can carry through the worst open the evidence allows.
- **Where non-experts go wrong.**
  - Taking Phoenix's index (the oracle) for fair value, and selling the
    perp's premium over it: in a closed period the index converges to the
    perp, not the other way, until the reopen moves it to the open.
  - Taking the perp's off-hours price as the market's best guess of the
    open, and following every move into the reopen.
  - Fading every off-hours move because "weekend moves revert at the open".
  - Reading net order flow without asking who sent it: a worker's one-sided
    order looks exactly like the informed trader's, until the price shows
    whose impact stays.
  - Treating the informed flow's level as the open while it's still buying
    (or selling): it stops when it gets there.
  - Holding the cap into the reopen on an uncertain read, and breaching the
    loss limit on the gap.
  - Reading the tape late: `getSignaturesForAddress` at its default
    `finalized` commitment lists an order 32 slots after it executed, as on
    mainnet.

## The central decision

What position to hold into the reopen, given the perp's weekend.

| Option | Why it looks defensible | Why it isn't |
| --- | --- | --- |
| Follow the perp's move from the last external price | the perp is the only price discovery over a weekend | a worker's order and thin-book drift move it too, and the open takes those back |
| Fade it | weekend moves on thin books revert | the informed part doesn't, and the open goes past it while the informed trader is still trading |
| Trade the perp against Phoenix's index | the index is Phoenix's oracle price, which quotes can't push; the perp trades at a premium to it | in a closed period the index is the perp's own price, lagged: it converges to the perp, and at the reopen both go to the open |
| Follow the account with the largest one-sided flow | informed flow is one-sided | so is a worker's; only whether its impact stays tells them apart |
| The full cap whenever the read is right | more PnL | a wrong read through a 3% gap on a quiet weekend, or a 10% one on a news weekend, breaches the loss limit |

**The calculation chain:** read the perp's price (the makers' mid, not the
mark: the mark sits on the index) and every taker order with its signer
(`getSignaturesForAddress` on the market at `confirmed`, then
`getTransaction`, decoding Phoenix's order instruction) → each order's
impact (the price change across the turn it landed in) → the one-sided
accounts → for each, whether its impact stays or decays (fit against the
decay the session's noise orders show) → the permanent level → whether the
informed account is still trading as the reopen nears → the expected open
(beyond the permanent level while it trades) and the spread of opens the
evidence allows → the position, capped by the loss limit through the worst
of them.

Where an early slip spreads: reading orders at `finalized` puts every order
32 slots late, so impact is measured on the wrong turns, permanence can't be
fitted, and the informed trader looks done when it isn't. Reading the mark
as the perp's price gives the index's level: every impact looks small and
slow.

**What must be combined:** the instruction (the family: who trades, the
odds, how impact decays, the index's rule, the band, the reopen), the chain
(the orders, signed, and the makers' price each turn), and the clock
(`MTX_REOPEN_SLOT`). No single record decides it: the orders don't say
who's informed, the price doesn't say who moved it, and the index says
nothing the perp didn't.

**Nothing warns the agent.** A bot that follows, fades or trades the basis
still trades and still wins sessions; the cost shows at the reopen.

## The four-part shape (fair-task-design)

| Part | Here |
| --- | --- |
| An exact rule | Phoenix's closed-period pricing: the index follows the perp's own book (an EMA, never more than 3% behind), the mark sits on the index, the perp stays in a 1/15 band, and at the reopen the index moves to the open. It changes what the index and the mark *mean* |
| An authoritative, literally true default that reads the wrong way | Phoenix's oracle (index) price. `Trader::oracle_usd` documents it as the price quotes can't push, and in the market-making task the grader values positions at the oracle. All true here too; but in a closed period it carries no external information |
| One settling record off the main path, one ordinary pointer | The takers' orders, per signer, in the market's transaction history. The pointer: "Every taker order is an ordinary Phoenix transaction, signed by its account", with the RPC methods that read them. The decisive fact (whose impact stays) is in no single record: it's the price path around one account's orders |
| The rule as one step inside a bigger calculation | The task asks for PnL; the inference decides one input, the distribution of the open, to the size held into the reopen |

Real-world sources of difficulty: imperfect information (who is informed
is never stated; a worker looks the same), judgement without a formula
(how much to hold into a gap whose size is uncertain, under a hard loss
limit), and the knowledge itself (a venue-specific closed-period pricing
rule, and the permanent/transitory decomposition of impact).

## Failure modes (the fair-task catalog)

| Mode | In a bot | What invites it |
| --- | --- | --- |
| F9 convenient signal accepted | the index (or mark) as fair value; the perp's price as the open | the index is Phoenix's oracle; the perp is the only price |
| F3 second condition missed | one-sided flow taken as informed without asking whether its impact stays | the informed trader and the worker trade in the same sizes and pace |
| F13 change not carried through | the informed level taken as the open while the informed trader is still trading | the level is the only number the flow gives |
| F15 clock anchored wrong | orders read at `finalized`, 32 slots late | the RPC's default commitment |

## Variants: each regime flips the best response

| Regime (of 72) | Best response |
| --- | --- |
| informed trader and a worker (26) | follow the account whose impact stays, beyond its level while it trades; fade what the worker's impact still holds up |
| informed trader alone (17) | follow it; hold beyond its level only while it trades |
| a worker alone (17) | fade the worker's remaining impact and the drift: a quiet weekend opens near the last external price |
| neither (12) | little to do: fade the drift, small |

| Habit | Fails in |
| --- | --- |
| follow the move | worker sessions, quiet weekends |
| fade the move | informed sessions, most of all while the informed trader is still trading |
| trade the basis against the index | everywhere: it sells the perp's informed premium and buys its discount |
| follow the most one-sided account | sessions with a worker, the worker alone most |
| the cap on every read | the sessions it reads wrong (a breach zeroes the grade) |

## Levers

| Lever | What the market does | What a good bot does | How an honest trader sees it |
| --- | --- | --- | --- |
| the closed-period index | an EMA of the perp, within 3% of it, inside the band | ignores it as a signal; knows the mark sits on it | the instruction's rule; the index trails the book |
| the informed trader (60%) | trades toward where SPCX opens, permanently, until the perp gets there | follows it, beyond its level while it's active | an account that trades one way and whose impact doesn't come back |
| the worker (60%, independent) | 12–50 shares one way, same sizes and pace, impact decays | fades what its impact still holds up | an account that trades one way and whose impact comes back at the session's half-life |
| noise | small orders either way from the other accounts | uses them to measure the session's impact and half-life | every order's jump, and its decay |
| per-session impact and half-life | 0.08–0.2% a share; 60–240 slots | measures them live | the jump at each order, the creep back after it |
| thin-book drift | wanders with no trade behind it, pulled back slowly | doesn't follow it | moves with no order in the turn |
| the band | the perp stays within 6.67% of the last external price | reads a pinned perp with an active informed trader as more to come | the perp at the band's edge while one account keeps trading into it |
| news vs quiet weekends | 0.5–12% vs 0.2–3% | sizes for the open's spread under each | the instruction's odds; whether anyone's impact stays |
| the reopen | index and makers move to the open in ≤4% steps | holds through it; sizes for it | `MTX_REOPEN_SLOT` |

## No tells

- The informed trader and the worker draw their start, pace and sizes from
  the same ranges, and trade only their own orders; which accounts they use
  is drawn per session, and every session has fresh keypairs.
- The session's two lengths are drawn first; the reopen slot is disclosed
  (as Phoenix's calendar discloses it) and dates nothing.
- Every session has noise and drift; workers occur with the same odds with
  or without an informed trader.
- The count of orders differs with who trades (each adds its orders), as it
  would in a market; it doesn't say which of the two one-sided accounts is
  which.

## Detectability

From an offline simulation (the session's
costs: half-spread, Phoenix's 3.5 bp taker fee and depth), USD per session
on 1,000 generated sessions: told 239, reference (g1) 212, the g0 reference
(the structural model alone, the gap alone) 56, a ridge regression on
generic features 41, follow 10, fade −23, basis −145. The told rung's
forecaster has R² 0.53 on held-out sessions against the reference's 0.45:
what knowing who is informed is worth. On chain the lab reproduces the
reference session by session (12 sessions: within a few dollars in most).

## Beating generator-fitting

The scenario generator ships to agents (`mtx scenario generate`), so a bot
can be fitted to the family. On an earlier task built the same way, every
Sol run did, at 2–3× a hand-tuned reference. Here g0 repeated the lesson: its reference was a
Bayesian model of the family, with parameters searched on generated
sessions, but it only traded the gap into the reopen, and all three Sol
runs traded the whole closed period (riding the flow while it builds,
fading uninformed pressure as it decays): 3.2–3.6× the reference. So g1:

1. **The reference is fitted to the generator the way the best bots were.**
   A gradient-boosted forecaster of the perp's next 64 slots (the reopen's
   included when it's nearer), trained on 8,000 generated sessions on 37
   features a bot can compute live: the structural model's read (who is
   informed, with what probability, where that puts the open, whether it's
   still trading, the half-life), per-account flow (net, count, age, impact,
   how much of it the price kept, the orders' limits and sizes), recent
   moves, quiet-turn drift, the premium over the index. Its policy (scale,
   dead band, stop, reopen sizing) was tuned on 1,000 other sessions. In
   the lab it makes 3.8× the g0 reference, about what the g0 Sol bots made
   on chain; a regression stacked on the model's own outputs didn't improve
   on it.
2. **The decisive values are per session, and only the live session shows
   them**: which account is informed and which is working an order (fresh
   keypairs every session), the impact per share, the decay half-life, the
   drift, whether the informed trader is still trading. The generator
   teaches the method, not the answer.
3. **The g0 Sol bots are frozen exploit rungs**: generator-fitted bots that
   must stay below the told rung on fresh sets.
4. **The generator's tells found in g0 are gone**: the reopen lands in one
   slot (the index snaps, as Phoenix's does), so there's no walk to follow;
   the worker's last order stays in the informed trader's size range.

What his conventions don't allow: a lever the generator can't teach at
all. The generator is the hidden family, so every regularity is learnable
in principle, and the defence is a reference that has learned it too. A
per-batch shift wouldn't help: each session runs as a fresh process with a
fresh `HOME`, so a bot can only learn a batch's values within a session,
and a batch drawn off the disclosed family would be unfair.

## Fairness

- Every correct path runs through standard RPC and `mtx-kit`: the
  reference does (`getSignaturesForAddress` at `confirmed`,
  `getTransaction`, `phoenix_rise`'s instruction decoding, Hawkeye's BBO,
  market orders).
- The sandbox's quirks that would hide the flow are stated: `getBlock`
  returns no transactions here, and `getSignaturesForAddress` at
  `finalized` lags 32 slots.
- The instruction's numbers are the generator's, clause by clause, and the
  engine's: the index's rule (EMA time constant, update cadence, the 3% cap
  that Phoenix's oracle band forces), the mark's offset from the index
  (measured on chain: +0.1%), the band, the reopen's steps.
- The mark sits on the index during the closed period (the oracle templates
  report the same price as the spot and perp components, so the mark's
  book component never takes over), which is also how Phoenix behaves with
  its 9,000-slot exchange-price EMA in a closed period: the instruction
  says so, because the drawdown limit is measured at the mark.
- Penalty twins as in flash-crash: a position at the cap passes, over it
  zeroes; a cheatcode zeroes; a send outside the turn is refused, not a
  violation.
- Void, not zero: a session where more than a tenth of the takers' orders
  fail voids the grade (the bot didn't see the flow its scenario
  describes).
- The reference is hidden from practice: practice files carry no reference
  PnL, the instruction says only that it holds a position into the reopen,
  and the image doesn't hold it.

## Failure modes, priced

| Failure mode | How it's charged |
| --- | --- |
| follows a worker | the worker's remaining impact plus the drift, the wrong way, at the open |
| fades the informed trader | the gap past the perp, the wrong way |
| takes the informed level for the open while it's active | the gap left past the level |
| trades the basis | the informed premium it sells, the round trips |
| the cap on an uncertain read | a breach: zero for the whole grade |

## Engine changes

- `crates/grader/src/weekend.rs`: the family, `OffHours` (reopen slot, band,
  half-spread, the index path, the takers), `WeekendShape`, stratified
  `draw_set`.
- `Scenario.offhours` and its overrides: through the closed period each
  move of the perp is `phoenix-market-move` (oracle and makers), followed in
  the same slot by `phoenix-oracle-price` back to the closed period's index,
  which only the oracle reports carry; between moves the index updates on
  its own; the reopen moves oracle and makers from the index to the open.
  Phoenix has no closed-period mode in the snapshot (it was taken on a
  Thursday), so the closed period is this scenario feature: the index
  follows the perp's scripted price, not the book the bot's own quotes
  shape, and the band is the makers'.
- Takers without the market-making task's competing maker (the flow
  generalized to an order list and a half-spread); `MTX_REOPEN_SLOT` for the
  bot; `--family weekend-gap`, `--draw weekend-gap` with its shape as the
  annotation and the told rungs' regime.
- Regression: the flash-crash and market-making references give
  identical PnL on the same draws before and after.
- Security: the takers' and the competing maker's keypairs now sit in
  root-only directories. Before, a bot (running as `mtx-bot` in the same
  container) could read them from `/tmp` and sign as a taker, which affects
  phoenix-market-making too.
- Weekend sessions get 240 setup slots: onboarding eight takers under a
  verifier's load overran 60, and those sessions didn't replay.

## Predictions

g1, before its screen: Sol between 0.7 and 1.3 of the reference, at least
one run below 1. (It came in at 0.85, 0.95 and 1.04.)

g0, before its screen: the reference at about 0.04 reward. GPT-6.1 Sol reads the
flow (it mapped accounts on chain in earlier tasks), probably fits a
model to generated sessions, and gets the index right; the
worker/informed split and the sizing are where it loses. Skill 0.3 to 1.2
per run, one run below 0.5.

## Generations

| Version | What happened | What changed |
| --- | --- | --- |
| g0 | Lab (2,000 sessions): reference $68.5 a session, told 126, gap-oracle 209, a generator-fitted regression 41. Engine ladder, two fresh 72-session sets (reference $2,496 and $3,342, reward 0.035 and 0.047): follow −1.24/−0.40, fade −0.13/0.09, basis −2.81/−2.03, fitted 0.38/0.30, told 2.08/1.94, gap-oracle 4.13/3.69, hindsight 28.4/21.4; every rule holds (bootstrap odds ≥ 0.94), the reference's deepest drawdown $331. Harbor: oracle skill 1.000 (reward 0.040), cheater 0 (tampered), sneaky 0. A Sol run found in its first minutes that the worker's last order can be under 0.2 shares, which the informed trader's never is (the worker's remainder): a tell for about a fifth of worker sessions  Screen, 3 GPT-6.1 Sol runs (one 72-session set, reference $4,558): rewards 0.225, 0.226, 0.201, skill 3.58, 3.59, 3.19, profitable in 74–78% of sessions. All three decoded every taker order per account and modelled permanent against decaying impact (a mixture of state-space models; a Kalman mixture; a gradient-boosted 64-slot forecast trained on 6,000 generated sessions), and traded the whole closed period ($124–131 a session there, against the reference's −$16); two also followed the reopen's index walk through its later steps | — (new task) |
| g1 | Ladder, two fresh sets (one container per rung): follow 0.12/0.06, fade and basis 0 (breaches), fitted 0.27/0.15, g0 reference 0.41/0.30, told 1.08/1.19, path oracle 0.92/1.10, frozen g0 Sol bots 1.06/1.09, 1.07/1.31, 0.83/0.92, hindsight 4.4/6.0: the reference is now about as strong as the bots fitted to the family; the told rung is too close to it and the path oracle too weak, so the ladder fails its told and exploit gates. Harbor oracle: skill 1.000, reward 0.219. Screen, 3 GPT-6.1 Sol runs (one 72-session set, reference $15,975): rewards 0.210, 0.230, 0.187, skill 0.95, 1.04, 0.85; no breaches, refused calls or overruns. The two below the reference beat it in the closed period ($135 and $116 a session against $90) and lost it at the reopen ($76 and $72 against $131): both cut their exposure into the reopen against a conservative open shock, where the family's odds let the reference hold | The reference fitted to the generator the way the g0 bots were (structural model plus a gradient-boosted 64-slot forecast on 8,000 generated sessions, trading the whole closed period); the reopen snaps in one slot; the worker's last order stays in the size range; told retrained with who-is-who flags; a path oracle; the g0 Sol bots as frozen rungs |
| g1 (ladder repair) | Two fresh sets, one container per rung: follow −0.15/0.05, fade −0.10/−0.24, basis 0, fitted 0.20/0.27, g0 reference 0.31/0.21, frozen g0 Sol bots 1.02/1.00, 1.20/1.17, 0.89/0.80, told 1.53/1.39, path oracle 2.25/1.99, hindsight 5.2/4.4. Every rule holds (odds 1.000), told beats every frozen bot on both sets (odds 1.000): the ladder holds. A straight lookahead oracle (cap toward the level 16–128 slots ahead) loses money or barely beats the reference in the lab: the wobble charges every flip | Told is told the regime (who is the informed trader and the worker, the half-life, where SPCX opens, as flash-crash's regime oracle is told where its session ends), gets the exact permanent/transitory split as features, and holds the known gap outright in its last 64 slots. The path oracle plans its positions over the whole known path by dynamic programming after costs. Reference, family and instruction unchanged |
