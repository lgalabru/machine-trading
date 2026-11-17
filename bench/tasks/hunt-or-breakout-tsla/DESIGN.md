# Design record: hunt-or-breakout-tsla

Written to `skills/trading-task-design/references/design-record.md`. The
README says what the task is; this says why it's built this way, and what
each version learned. It is card B3 of the batch-2 cards
(`TASK_CARDS_BATCH2.md`); the departures from the card are listed last.

## Thesis

A good bot profits here because it reads who leads a run through a round level (one hunter wallet pushing, or informed wallets building) and whether the oracle follows, then fades the hunt or follows the break before it resolves.

- **Wins:** fading a hunter's push while the book is through the level; following informed flow into a break, before the level jumps if it can.
- **Loses:** fading every run loses the breaks; following every run loses the hunts; waiting for the oracle enters breaks late and misses hunts that end first; trading every crossing of the grid pays costs on runs nobody led.
- **Regimes:** every session has 1 to 3 hunts and 1 to 3 breaks, mirrored up and down.
- **Mandate:** TSLA only, ±12 TSLA, a $500 loss limit; a position left at the close is closed at the oracle less 5 bp.

## Expert research

Stops cluster at round numbers, and price runs through them are the classic
place where liquidity is engineered (Osler 2003, "Currency orders and
exchange rate dynamics: an explanation for the predictive success of
technical analysis"; Osler 2005 on stop-loss cascades). A desk's question at
the moment of a break is whether it is information or a run on the stops:

- **Who is trading.** Informed flow comes from several accounts building a
  position over time (Kyle 1985; Easley, Kiefer and O'Hara on informed
  order arrival); a stop run is one large aggressive order followed by the
  same account unwinding.
- **Whether fair value moves.** A hunter can push a book but not the
  reference price: an index or oracle that doesn't follow is evidence of a
  run, one that does is evidence of information.
- **Speed.** The fade pays only while the book is through the level; waiting
  for full confirmation gives up most of it, which is why desks read the
  tape rather than the chart.

The non-expert trades the chart: it buys every breakout or sells every one.

## The central decision

At each run through a level: fade it (bet the book comes back to the
level), follow it (bet the level goes on), or stay out. The evidence is on
chain: every trader's Phoenix account, its position step by step, and the
oracle.

## Variants

| Run | Best response | Fade every run | Follow every run | Wait for the oracle |
| --- | --- | --- | --- | --- |
| Hunt | fade at once, cover when the book comes back | right | loses the excursion and the approach | right only if the hold outlasts the wait |
| Break | follow the informed flow, before the jump if it shows early | loses the follow-through | gains the follow-through less the excursion | enters late, or fades the creep |
| Drift through a level (no one leads) | stay out | pays costs | pays costs | pays costs |

## Levers

| Lever | What the market does | What a good bot does | How an honest operator detects it |
| --- | --- | --- | --- |
| The hunter | one wallet; pushes 2.4-4.8 TSLA a couple of slots before the run, trades the same size back its hold later | ride the push, then fade the run it leads; remember the wallet |
| The breaker | one informed wallet; sweeps 2.4-4.8 TSLA a couple of slots before each break, exactly as the hunter pushes, and trades it back its hold later | follow the run it leads; remember the wallet | trader accounts' positions (`getProgramAccounts`, then `getMultipleAccounts` each step), or the fills |
| The informed | 2-3 wallets that trade the oracle's move 20-60 slots ahead of it | follow their flow | the same accounts, building one way before or as the book runs |
| The excursion | the book's makers jump 0.15-0.45% through the level, hunts and breaks alike | nothing: it doesn't tell them apart | the book |
| The oracle | stays put in a hunt; creeps then jumps in a break | a confirmation, not a trigger | `market.oracle` |
| Chasers and noise | chasers take the way the book moved; noise either way | ignore them | they join after the run starts |

## No tells

- Every session seats every actor (noise, chasers, the informed, the
  hunter's wallet and the breaker's) and has both kinds of run; the roster the informed
  bot is told never depends on which runs come.
- Hunts and breaks draw the excursion, the approach and the time the book
  spends through the level from the same ranges.
- The run's way is the side the walk approaches the nearest level from, so
  runs go up and down with equal odds.
- The hunter's schedule (`at_slots`) and the path's premiums are in the
  scenario file, which the bot never sees.

## Detectability

On 16 of the Gate 0 sessions, the reference's calls against the informed
bot's: hunts 26 faded right, 1 followed, 4 missed; breaks 24 followed right,
3 faded, 5 missed; 6 trades on runs the informed bot left alone. It decides
about 10 slots after the informed bot on average; the informed bot often
follows a break before the book moves.

## Fairness

- The instruction states the family's every range: the grid, the counts of
  hunts and breaks, the excursion and follow-through, the hunter's size,
  hold and trigger, the informed horizon and pace, the noise and chasers.
- Everything a good bot uses is on chain: trader accounts and their
  positions, the book, the oracle.
- Practice is six sessions from the family.
- The reference stays inside the mandate in every session.

## The anchors

- **The reference (0.5)** lists Phoenix's trader accounts and reads their
  TSLA positions every step. It takes a 2.2-TSLA-or-more clip from an
  account that came into the session flat as a push, and once that push's
  run has played out (90 slots) names the account a hunter (the oracle
  never followed) or a breaker (it did). It rides and fades a known
  hunter's later pushes and follows a known breaker's; it leaves an unknown
  pusher alone, so it misses each session's first hunt and first break. It
  isn't told who anyone is.
- **The informed bot (1)** is told the informed wallets (the breakers'
  among them) and the hunter's, the hunter's hold and trigger, the grid and
  the informed horizon (identity and process facts; `LATENT_FIELDS`), never
  where, when or which way a run goes or whether it is a hunt. It rides a
  hunter's push into the run, then fades the run once the book is through;
  it follows a breaker's sweep or the informed wallets' flow at once.

## The exploit to defeat (first screen, 2026-11-17)

On the first build, one gpt-6.1-sol run (native Codex, high effort,
unguarded screen) scored raw 1.27: $6,244 on 72 hidden sessions against the
informed bot's $4,631, every session profitable, worst session +$10.5. Its
bot:

- lists every Phoenix trader account at the start and reads their TSLA
  positions each turn;
- takes a clip of about 2.4-4.8 TSLA (20-40% of the cap) from an account
  that was flat, a couple of slots before the book runs, as a stop hunter's
  push: it buys (or sells) the cap with the push, rides the book's jump
  through the level, then turns and fades the run until the book comes
  back;
- follows every run that no such push led;
- learns a hunter from its push and trade back, and stops adding risk at
  half the loss limit.

It worked because the class was labelled by structure: every hunt began
with one large push from a flat wallet and no break did, and the push
always came a couple of slots before a scripted jump. Reading sizes and
flatness was enough; who pushed never mattered. A fairness defect (no
cheap labels), not skill.

The fix: every break now begins the same way, with an informed breaker's
sweep from a wallet of its own, drawn from the same size and hold
ranges, flat before, traded back out a hold later. A push's size, timing
and the wallet's flatness no longer separate a hunt from a break; only who
pushed does, which a reader learns over the session from what each pushing
wallet's earlier runs did, and the informed bot is told. The `push-fade`
ladder rung re-implements the Sol bot's core without identity: it must stay
under the informed bot.

## Failure modes, priced

See the README's baselines: following every run and fading every run lose
about $20-35 a session; waiting for the oracle loses a little; staying out
of runs nobody led is worth more than any single rule.

## Predictions

Frontier agents will find the oracle-versus-book gap and land near the
wait-for-oracle rung or a little above. Reading trader accounts is what
lifts a bot past the reference; few will build the account scan.

## Generations

- **v1 (this build).** Gate 0 on the engine (72 sessions): the informed bot
  made $3,595, every non-reading rung lost. The first reference lost money;
  the second (crowd read before chasers, runs nobody led skipped) made $648,
  0.12 of the informed bot. The full build's reference reads only the
  session's new takers, takes a push as 2.2 TSLA or more, watches the pusher
  for 14 slots (a hunter goes quiet, an informed trader keeps trading),
  learns the hunter from a run it led and traded back out of, and lets the
  oracle decide runs nobody pushed: $1,114 on the same 72 sessions, 0.27 of
  the informed bot weighted as v2 weighs it, per-session Sharpe 0.63.

## Departures from the card

| Card | Built | Why |
| --- | --- | --- |
| Break continuation 0.4-1.5% beyond the level | 0.15-0.65% beyond an excursion of 0.15-0.45% | follow-every-run must net about zero: what a follower gains past the excursion matches what it loses in a hunt |
| The hunter hunts whenever it is near a level | the generator schedules its hunts (`at_slots`) and lays the book's excursion as a premium on the path | the snapshot's makers re-centre on every path move, so a push alone moves the book for a slot or two; the run is the makers repricing |
| Informed see the book ahead | they see the level (`sees: level`) | seeing the book, they would trade ahead of hunts too |
| Reference reads fill events | it reads trader accounts' positions | the kit's Phoenix event decoder isn't in the component build; positions are on chain for any bot |
