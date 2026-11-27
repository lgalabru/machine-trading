# Design record: funding-carry-sol

Written to `skills/trading-task-design/references/design-record.md`. The
README says what the task is; this says why it's built this way, and what
each version learned. It is RFD 0007's funding-carry task: the carry trade
of `funding-carry-spcx`, moved to the market it can be measured on (SOL),
with every number that sets what a bot can earn taken from mainnet.
It replaces `funding-carry-spcx` in 0.15.0: that task is retired (its last
release is 0.14.0's; its files are in git history).

## Why SOL, not SPCX

RFD 0007 found that the bench's perp tasks guaranteed the edge they taught.
For the carry task that edge was the premium: `CarryFamily::spcx()` opened
premiums of 30 to 150 bp, crowded or informed, every 7.5 to 40 seconds.
Nothing on Phoenix looks like that, and SPCX can't be measured well
enough to say what it does instead (0.33 taker orders a minute in regular
hours, 89% of its prints a pinger's $3 lots). SOL can: it trades all day,
every day, and Phoenix's public API serves its funding history.

## What was measured

| Number | Value | Source |
| --- | --- | --- |
| Premium (hourly funding × 8), 90 days to 2026-11-25 01:12 UTC | mean +1.434 bp, sd 1.072, p1 -1.87, p99 +3.64, max \|8.29\| bp; longs paid 92.4% of hours; never at the cap (max 4.5% of it) | `calibration/funding/phoenix-funding.sol.20261125.248a28ea.json` (`all`) |
| Five-day windows of it (18 windows) | level mean 1.434 bp, sd 0.665 (p1 -0.044, p99 2.805); wander about the level 0.841 bp, lag-1 autocorrelation 0.827 (half-life 3.64 intervals) | same file, `windows[hours=120]` |
| Three-day windows (30) | level sd 0.77, p99 3.2; wander 0.745 bp, half-life 2.76 | `windows[hours=72]` |
| The tape's three days alone | mean +0.14 bp, 63.5% positive | `tape_window` |
| Funding settings on chain | interval 3,600 s, period 28,800 s, cap 1,931 quote lots a base lot (0.2273% at the calibration's fit; 0.2325% at the study's fetch) | `/exchange` (study and `snapshots/phoenix-perps-2026-11-13/exchange.json`) |
| Takers, regular / off / weekend | 0.9872 / 1.335 / 2.6323 orders a minute; median $511.98 / $534.75 / $509.71; log-sd 2.096 / 1.686 / 1.336; 780 / 2,233 / 1,395 minutes of tape | `calibration/tape/phoenix-tape.20261125.fc443f1e.json`, `markets.SOL.flow` |
| Takers' one-minute markout | 1.723 bp ($5-100, 1,449 orders), 1.29 ($100-1k, 3,322), 0.752 ($1k-10k, 2,504), 8.221 ($10k+, 147): order-weighted 1.330 bp | `markets.SOL.markouts` |
| Paid to cross | 3.02 / 1.78 / 1.52 / 4.95 bp by the same buckets | `markets.SOL.markouts.*.cost_bps` |
| Five minutes on, $100-1k | +0.275 bp (95% interval -0.63 to 1.13) | `markets.SOL.markouts.100-1000.markout_bps.300s` |
| Worst fill against the mid | $1k-10k median 1.40, p90 9.39 bp; $10k+ median 4.37, p90 20.32 bp | `markets.SOL.impact` |
| Pinger | 5,134 of 12,557 orders (40.9%), about 1.17 a minute | `markets.SOL.pinger_orders` |
| Top of book | one tick ($0.01): 1.45 bp wide at ~$69, half-spread 0.73 bp | luthor RFD 0001, 2026-11-22; luthor quotes of 2026-11-25 (106 minutes), as the brief cites them |
| Who leads | Binance leads Phoenix's SOL by about 0.5 s, Coinbase by about 0.25 s (1 s returns' lagged correlation) | luthor RFD 0001, 2026-11-22 |
| Makers on mainnet | luthor's paper book makers on SOL marked out at -1.0 to -1.6 bp a fill after fees | luthor RFD 0001 (scoreboard, 2026-11-22) |
| The index | per-second sd 8.22e-5 in the median session, Student-t (df 4.19), GARCH(1,1) (alpha 0.045, beta 0.912), calm/stressed regimes | `calibration/market/sol-usd.20261112.0dbc6cdf.toml` |

The brief that started 0.15 quoted SOL's impact as "$1k-10k median 0 bp,
p90 5.8; $10k+ median 2.9, p90 13.8". The committed tape study says 1.40 /
9.39 and 4.37 / 20.32 (worst fill against the mid, half the spread in
it). The task uses the committed file's numbers; where the brief's came
from is an open question.

## What a session is

A session is four to seven days of SOL's funding, played in six to eight
minutes, against six to eight minutes of SOL's price.

- **Funding.** Phoenix's program, on the snapshot's market, with the
  interval drawn from 3 or 4 seconds, the period eight intervals, the cap
  the snapshot's. Phoenix computes it on the book's mark (the mid at its
  last trade), as `funding-carry-spcx` did, for the reason its design
  record gives (on a surfnet the oracle-weighted mark can't follow a book
  premium).
- **The premium.** Each session draws its level from the five-day windows'
  levels (normal, mean 1.434, sd 0.665, clamped to -0.5 and 3.5 bp) and
  walks around it with the five-day windows' wander (0.841 bp, half-life
  3.64 intervals), on funding's clock. The level is the session's: a desk
  that had watched SOL's basis for a week would know roughly where it sits.
- **The index** is the market model v2's draw from SOL's calibration
  (`MarketModel::draw`, default spec: no stated departures), at its own,
  real pace. The waypoints are the model's every 4 slots once the last move
  has finished, each carrying the premium.
- **Takers** are the actor library's (`noise`, `informed`), the scale
  filled from the tape: orders a minute (times the library's clock factor,
  0.625), median size in SOL at the snapshot's $79.54, its log-sd. The
  session's hours are drawn with the tape's minutes as odds. The informed
  see the index 240 slots (60 s) ahead, right 85% of the time, trade when
  that move is over 7 bp (Phoenix's taker fee in and out), at 1.5 times
  noise's size; their share of orders, 0.12, is fitted (`fit_informed`, on
  200 model paths) so that the average order's one-minute markout is the
  tape's 1.33 bp. Both reach 20.32 bp through the book.
- **The arbitrageur** is the library's, against Phoenix's index: latency 1
  or 2 slots (the exchanges' lead), edge 3.5 to 5 bp (the taker fee and up
  to 1.5 bp more), one median order at a time, up to 40 held. It is what
  keeps the basis from sitting still: a book walked past its edge comes
  back within a slot or two, at the walker's expense.
- **Impact** (`impact`, the engine's script): every trade moves the book;
  none of it stays (permanent share 0), the rest halves every 60 s, at most
  20.32 bp, read at the tape's median order (6.44 SOL).
- **The pinger** keeps the mark fresh: 0.04 SOL every 4 to 10 slots from
  the shared taker accounts.

## Stated departures

| Departure | Measured | Here | Why |
| --- | --- | --- | --- |
| Funding's clock | interval 3,600 s, period 28,800 s | interval 3-4 s, period 8 intervals (900-1,200× faster) | a session of minutes must see funding; the period-to-interval ratio and the cap per interval are mainnet's |
| The premium's clock | half-life 3.64 hours | 3.64 intervals (11-15 s) | runs with funding's, so a session is days of the basis |
| The index's clock | real | real | not compressed: an unhedged carry here faces six minutes of SOL's risk, not days; a desk would hedge with spot, which this mandate doesn't hold |
| Same-side bursts | weekend 84% same side next, 27% within 1 s | coin-flip sides, Poisson arrivals | the library's noise has no bursts |
| Informed horizon and hit rate | not measured | 60 s, 85% | the library's defaults; the share is fitted to the measured markout |
| Informed size | the tape's $10k+ orders were the informed ones (+8.2 bp a minute on) | 1.5× noise's median | the direction is measured, the multiple isn't |
| Arbitrageur | the exchanges lead by 0.25-0.5 s | latency 1-2 slots, edge 3.5-5 bp, drawn | edge is the fee plus a margin, not measured |
| Impact persistence | five minutes on, a small order's move can't be told from 0 | permanent 0, half-life 60 s | the calibration's 0.5 and 30 s are unmeasured defaults; with a permanent share the bot's own carry would move the book's premium against it for good, which no measured flow shows |
| The pinger's pace | 1.17 prints a minute | about 34 a minute | the mark is the book's mid at its last trade: it must be fresh each interval |
| The book's depth | the tape's $10k+ orders' worst fill 4.37 bp (median), 20.32 (p90) | the snapshot's own makers: 125 SOL sent at once cost 18.5 bp on average plus the fee (measured on the engine, a flat session) | the snapshot's book at its moment; the instruction says it is thin |
| The premium's level, per session | one week's level on mainnet is what it is | drawn per session from the five-day windows | so a set covers rich and thin weeks; sets are stratified half under the mean, half over |
| The markout fit | measured over all hours | fitted on default-spec model paths, any hour | the model's paths at the median session's volatility |

## The skill

A good bot profits here because it prices the carry Phoenix will actually
pay, on the session's clock, and takes it only where it pays for what
trading costs.

- **The calculation chain.** Read the session's interval and period from
  the PerpAssetMap (the `venue` metadata's are mainnet's hourly ones: a bot
  that reads those sees no carry at all) → each step, the premium (the
  book's mid over the index; half-ticks, so average it) → the funding a
  short collects over the rest of the session: the premium over the period,
  each second, plus what its distance from its level still pays while it
  fades → against the cost of getting in: the 3.5 bp taker fee, and the walk
  through a thin book that the arbitrageur takes back past its edge, which
  grows with size and speed → size it against six minutes of SOL's risk,
  as v2 weighs losses → get in in slices, or rest orders and let the flow
  come (0.5 bp a fill, but the flow is one or two orders a minute) → hold:
  a position at the close pays no fee.
- **Where non-experts go wrong.** "Funding pays the side against the
  premium, so short it all": on a thin week the funding is worth less than
  the fee; sent at once, the walk costs more than the week's funding.
  Funding priced off the `/exchange` file (hourly over eight hours: worth
  nothing in a session). Funding priced off one read of the mid (half-ticks:
  the premium reads -0.63 or +0.63 bp more often than its level). Fading
  the premium back and forth like a mean-reversion trade (it pays 3.5 bp a
  side to earn a fraction of a basis point). Trading the index's wobbles
  (it's SOL: no edge without the informed takers' view).

## Anchors

- **Reference (0.5).** `reference/`: knows what the instruction states of
  SOL's basis across weeks (level 1.434 bp, sd 0.665; wander 0.841 bp,
  half-life 3.64 intervals), not the session's level. It estimates the
  level by shrinking the premium's running mean toward that prior, by how
  many half-lives it has seen; prices the funding over the rest of the
  session from it, less a 7 bp entry cost; weighs that against the index's
  risk as v2 weighs losses; and holds up to 60% of the cap (all of it at a
  6 bp loss-weighted edge), in 5 SOL market orders every 4 slots. It sheds
  only when the estimate turns against the position by more than getting
  out costs, and flattens past 60% of the drawdown limit. The first
  reference (a premium average with a fixed entry rule, 80% of the cap)
  measured a per-session Sharpe of 0.20 over 60 hidden sessions (astra,
  `~/mtx-015/fc-anchors`): too noisy for v2's band.
  Over 72 hidden sessions on mtx-bench-2 (six fresh secrets,
  `fc-ref2-1` to `-6`, v2 sessions), the new reference made $603.0
  (loss-weighted $566.3, per-session Sharpe 0.79) and the informed bot
  $944.2 (loss-weighted $906.6, Sharpe 0.78): the reference is 0.625 of
  the informed bot, inside the 0.2-0.7 band, and below it on every set of
  12 (40/86, 156/223, 72/111, 77/90, 131/225, 127/210).
  With the informed bot's risk rule, over 72 more hidden sessions
  (`fc-ref3-1` to `-6`): reference $493.4 (loss-weighted $353.4, Sharpe
  0.40), informed $849.9 (loss-weighted $637.1, Sharpe 0.43): 0.555, the
  informed bot ahead on every set (53/73, 30/40, 22/90, 71/143, 64/122,
  114/169, loss-weighted).
- **Informed (1).** `checks/bots/informed/`: the same execution, told the
  session's latent state: the premium's level, spread and half-life,
  funding's settings, the flow's pace, the informed share and the
  arbitrageur's latency and edge. It uses the level, the half-life and the
  period: from its first step it prices the funding over the rest of the
  session from the level (and the current premium's distance from it,
  fading), takes off the entry cost, weighs what's left against the index's
  risk as v2 weighs losses, and holds up to the full cap. It never sees the
  path, the premium as it will wander, or an order.
  Its risk rule: a loss budget of the funding it expects over the session
  plus one standard deviation of the index over it (both priced at its
  first step from what it is told and the calibration's per-second sd,
  never under 7 bp of the cap's notional). Past it, the index has taken
  more than the carry and an ordinary session's noise account for, and it
  gets out for the session. Without it the bot, at the full cap, lost more
  than the reference on bad sets: on the fast gates' public set its
  loss-weighted PnL was -$14.77 and v2 had no ceiling to scale to (astra);
  with it, +$14.45 (raw +$57.51 against the reference's +$30.7).

Frozen for 0.15.0 (SHA-256 of `src/lib.rs` then `src/main.rs`, first 16 hex): the reference `2e5758d9719fcbf5`, the informed bot `88fe6325227ea081`.

The latent fields are all process facts (`LATENT_FIELDS`,
`funding-carry-sol`): `premium_mean_bps`, `premium_sd_bps`,
`premium_half_life_slots`, `funding`, `orders_per_minute`,
`informed_share`, `arbitrageur_latency_slots`, `arbitrageur_edge_bps`. No
roster: the informed takers' wallets would make the task a flow-reading
one, which market-making already is.

## Ladder

`checks/ladder.json`: nop; blind long and short (the full cap at once);
the reference; the informed bot (`tell_latent`); the shared probe set
(coin flips, martingale, limit-hugger, stop-lotto, levers, breacher,
honest blends, crash) with the task's probe wrapper
(`checks/rungs/probe`). Rules: blind below the reference; the reference
below the informed bot. The honest probes are calibrated for this family
(`bench/checks/probes/calibrate.py` on the reference and the full-cap coin
flip, 4 fresh sets of 24 sessions on mtx-bench-2): the reference measured a
mean of $5.7 a session, a spread of $15.0 and a per-session Sharpe of 0.38;
the full-cap coin a spread of $28.6; so the coin is sized at 0.381 of the
cap, and the blends for a Sharpe of 0.1, 0.2 and 0.3 are 0.545, 0.703 and
0.866 (`crash-*` wrap `honest-0.2`'s). A Sharpe of 0.5 is past the
reference's: `honest-0.5` is held at a blend of 0.97, mostly the
reference, as flash-crash's is.

## Generations

| Version | What happened | What changed |
| --- | --- | --- |
| 0.15.0 | Built on `release/0.15.0-funding-carry`. Engine checks on this machine (macOS, the 96-market snapshot): a 1,610-slot flat session at a 3 bp premium paid a 125 SOL short +$14.57 against -$25.67 at no premium, so Phoenix's funding paid $40.24 (the formula's $50, less what the bot's own walk took off the book's premium while it lasted); 0 of 221 pinger orders failed; the library's takers and the arbitrageur traded (9 takes in 1,573 slots on an off-hours session). Anchors on one development set (`mtx grade --draw funding-carry-sol --count 12`, a throwaway secret, v2 sessions): reference +$57.64 (7 of 12 sessions up, per-session Sharpe 0.38), informed +$143.15 (Sharpe 0.49); weighted as v2 weighs losses, 43.75 against 119.79, so the reference is 0.37 of the informed bot, inside the 0.2-0.7 band; doing nothing $0. Twelve sessions: the ladder's four sets of 72 decide. Grading on this machine was flaky while other worktrees graded at once (surfnets unreachable mid-session, `RPC port … is already in use`): most runs past the first ~40 sessions failed before finishing, including a plain SPCX control | New task: SOL, measured. The scripted crowded and informed episodes of `funding-carry-spcx` are gone: SOL's realised premium never came near them |
