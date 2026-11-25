# Design record: weekend-gap-spcx

Written to `skills/trading-task-design/references/design-record.md`. The
README says what the task is; this says why it's built this way, and what
each version learned.

## Thesis

A good bot profits here because it knows how much of a weekend's move
Phoenix's reopen keeps, and reads which of the few traders positioning into
the reopen know where it opens: it holds the part of the move the reopen
takes back, sized for the open's spread, and leans the way the informed
flow points.

0.15.0 rebuilt the task on mainnet's measurements (RFD 0007). The old
premise, a weekend whose own taker flow knows where SPCX opens and moves
the perp toward it, didn't happen on Phoenix: the tape's 23 recorded weekend
hours held one order over $5, and across 21 weekends the book's move came
from the makers' quotes, with a median of 31 minutes in 48 hours holding
$100 or more of prints. What the measurements do show is a reopen that
keeps about four fifths of where the makers took the book.

### Where every number comes from

| Number | Value | Source |
| --- | --- | --- |
| The weekend's move, unsigned | 21 measured values, median 0.86%, 0.05% to 2.75%; 15 of 21 up | `calibration/weekend/spcx-reopens.20261120.json` (`reopens.py` over Phoenix SPCX 1 m candles, Fridays 2026-06-12 to 11-20, holiday weekends 06-19, 07-03 and 09-04 left out): the book's last trade at Sunday 19:59 New York against the mark at Friday 19:59 |
| What the reopen keeps | 0.78 (through the origin; 95% bootstrap 0.57 to 1.04); OLS 0.71 with a +0.19% intercept (se 0.12), r 0.86 | same file: the mark at Sunday 20:05 against Friday's |
| The open's residual | sd 0.50% | same file |
| The reopen gap (open against the weekend's mark) | median 0.37%, p90 0.77%, max 1.40% (21); the calibration's 107 regular opens: median 0.44%, p90 1.12% | same file; `calibration/market/spcx.20261112.d1f48be2.toml` `[[calendar.gaps]]` |
| The makers' hourly re-price | sd 0.193% an hour | the same candles, the mark's hourly return over the 21 weekends (reported here, not in the file) |
| The weekend's prints | 31 orders a closed period, median $918, log-sd 1.53 | candles: median 31 minutes with $100 or more of prints a weekend (the file's `print_minutes_median`); sizes are the tape's weekday-night orders (`phoenix-tape.20261125.fc443f1e`, `markets.SPCX.flow.off`) |
| The tape's weekend | 0.0007 orders a minute, $11 an hour, one order | `phoenix-tape.20261125.fc443f1e`, `markets.SPCX.flow.weekend` (23.25 hours: the tape starts Saturday 23:45 UTC) |
| The reopen's burst | one order of about $3,000 in its first step | the tape: five buy fills in one slot at 2026-11-23 01:00:05 UTC, $2,998 |
| Cross-market | none carried: BTC r −0.17, SOL r −0.13 with SPCX's reopen gap (21 weekends); on the tape's one weekend every market rose (SPCX +0.57%, BTC +0.67%, SOL +0.67%, ANT +1.78%, NVDA +1.02%) and SPCX gave back 0.63% in the reopen's first 30 minutes, one point that fits nothing | the file's `cross_market`; the tape |
| The book | half-spread 0.4 to 5 bp | 0.4 bp (one tick): luthor's quotes on a weeknight, 2026-11-25; 5 bp: the calibration's default (`[book]`, no SPCX book recorded) |
| The index, band and mark | EMA of 120 slots every 8, 3% cap; 1/15 band; mark on the index | Phoenix's docs and the engine's `OffHours`, unchanged from 0.14 |

### Stated departures

- **Time.** 48 closed hours in 400 to 640 slots; the reopen's process is
  measured at five minutes and plays at once.
- **Direction.** A coin flip; 15 of the 21 weekends went up.
- **Resampling.** The weekend's move is one of 21 values, jittered
  (log-sd 0.2): the family can't say more than 21 weekends do.
- **The book.** Never recorded on a weekend; drawn from a weeknight's tick
  to the calibration's default.
- **The positioners.** Two informed wallets (right 95% of the time each;
  80% until the anchors' ladders, see Generations,
  seeing the open from 40 slots before it, about three orders each) and two
  decoys at the same pace and sizes, who trade as the informed do and lose
  the thread in the first half of the stretch. Not measured: the tape saw
  no such flow on SPCX, and the candles don't say who traded. They are the
  competition the task asks a bot to read, at about a third of SPCX's
  weekday-night pace (0.09 orders a minute). Without them the reference
  would be the whole task: the measured part (fade a fifth of the move) is
  one number a bot reads off the instruction.
- **The intercept.** The OLS fit's +0.19% at the reopen (1.6 standard
  errors) is left out: a drift up in a stock that fell from $166 to $130
  over the sample is no weekend fact, and the fit through the origin is the
  one the family draws.

## Expert research

- **Why it's a real expert task.** Equity perps trade over the weekend while
  their underlyings don't. Phoenix prices an equity perp's index from
  external markets 24/5 (Sunday 20:00 to Friday 20:00 ET); over weekends it
  follows its own book, and at the reopen the index snaps to the external
  price ([index price](https://docs.phoenix.trade/phoenix/real-world-assets/index-price.md),
  [mark price and bounds](https://docs.phoenix.trade/phoenix/real-world-assets/mark-price-and-bounds.md)).
  A desk holding the perp into Sunday 20:00 is betting on how much of the
  weekend's quoting the external market will confirm. Thin after-hours
  markets overreact and partly reverse at the open (Barclay and
  Hendershott, *Price Discovery and Trading After Hours*, RFS 16(4), 2003);
  21 weekends of SPCX on Phoenix show the same shape: the reopen kept 0.78
  of the move.
- **The expert's method.** Estimate how much of an off-hours move the open
  keeps from the venue's own history, size the position into the reopen by
  the expected take-back over the open's spread, and pay attention to who
  is still trading in the last hours: informed flow positions late, and its
  direction is worth more than any aggregate.
- **Where non-experts go wrong.**
  - Following the weekend's move into the reopen: the reopen takes about a
    fifth of it back.
  - Fading the whole move at the cap on every weekend: a small move's
    expected take-back doesn't pay the entry, and the open's spread
    (0.50%) dwarfs it.
  - Reading net flow without asking who sent it: decoys trade like the
    informed for a while.
  - Holding a position through the weekend's walk: it carries the makers'
    hourly re-prices with no edge until the last one.
  - Taking Phoenix's closed-period index for fair value: it is the perp's
    own price, smoothed.

## The central decision

What position to hold into the reopen.

| Option | Why it looks defensible | Why it isn't |
| --- | --- | --- |
| Follow the weekend's move | the perp is the only price over a weekend | the reopen keeps 0.78 of it: the follower pays the take-back |
| Fade the move at the cap | the reopen takes some back | most of the open is the 0.50% nobody knew: small moves don't pay |
| Fade by the expected take-back | what the reference does | right, and leaves the informed wallets' read on the table |
| Follow the net flow of the stretch | informed traders position late | decoys trade the same way at the same pace until they don't |

**The calculation chain:** the makers' mid against the starting mark (the
weekend's move) → the expected gap, (0.78 − 1) × the move → each wallet
that trades in the last stretch, its net flow (`getSignaturesForAddress` /
`getTransaction`, or its Phoenix position) → which ones look informed
(they keep one direction for the whole stretch; a decoy turns) → the odds
of an up or down open → the position, sized by the gap expected over the
open's spread, entered after the makers' last re-price.

## Variants

| Regime | Best response |
| --- | --- |
| a big weekend move (over 1%) | fade by the take-back; the informed flow mostly agrees |
| a small move (under 0.4%) | the take-back doesn't pay the entry: only the informed flow does |
| informed wallets disagree with the take-back | follow them when both agree |
| no informed flow (the gap under about 10 bp) | the take-back alone, or nothing |

| Habit | Fails in |
| --- | --- |
| follow the move | every weekend, on average |
| fade at the cap | small moves, and opens that keep going |
| net flow of the stretch | sessions where the decoys outtrade the informed |

## Levers

| Lever | What the market does | What a good bot does | How an honest trader sees it |
| --- | --- | --- | --- |
| the weekend's move | the makers re-price hourly, no trade behind it | measures it from the makers' mid | the book |
| the reopen's take-back | keeps 0.78 of the move, give or take 0.50% | fades the rest, sized | the instruction's measurements |
| the informed wallets | two, right 80% of the time each, in the last 40 slots | follows them | wallets that keep one direction for the stretch |
| the decoys | two, informed-looking then random | discounts them | wallets that turn |
| the weekend's prints and the burst | noise | ignores them | random sides, all session |
| the index and mark | an EMA of the perp, the mark on it | doesn't trade the basis | the instruction's rule |

## No tells

- The informed wallets and the decoys trade at the same pace, sizes and
  limits, from the same stretch; which wallets are which comes from the
  session's keyring, onboarded in an order drawn from its seed.
- The informed traders trade only when the open is more than about 10 bp
  away, so their silence says a little, and the instruction says so.
- The session's lengths are drawn first and independently; the reopen slot
  is disclosed (Phoenix's calendar discloses it).
- The makers' last re-price lands at least a compressed hour before the
  reopen, in every session: disclosed.

## Detectability

From an offline model of the family (`/tmp`-style lab, 100,000 sessions:
the measured weekend moves, gap = −0.22 × move + N(0, 0.50%), two wallets
right 80% of the time, entry costs of the half-spread, 3.5 bp fee and 2.5 bp
of depth, losses weighted 1.5): reference $13.6 a session ($9.4
loss-weighted, per-session Sharpe 0.33), informed $25.6 ($23.1, Sharpe
0.62), fade at the cap $14.7 ($8.6), follow −$30.2. Reference over informed,
loss-weighted: 0.41, inside the strength band (0.2 to 0.7).

## Fairness

- Every number the family draws is in the instruction, with its source and
  the departures stated.
- The reference reads only the makers' mid and the reopen slot; the
  informed bot reads its told wallets' positions with `getMultipleAccounts`
  and the trader arenas, as anyone who knew the wallets could.
- The positions are small against the mandate: the cap (70 SPCX, about
  $10,000) through a 3% gap is $300 of the $1,000 limit.
- A session where more than a tenth of the library's actions fail isn't
  scored (infrastructure).
- Practice anchor PnL is one draw of the actors: a calibration without a
  secret draws fresh session keys (`SessionKeys::random`), and with them
  the library actors' draws, so it won't reproduce between calibrations.

## Failure modes, priced

| Failure mode | How it's charged |
| --- | --- |
| follows the move | the take-back, about 0.22% of the move, at the open |
| fades every move at the cap | the entry's cost on small moves, and the open's spread on a full position |
| follows the decoys | the informed edge, the wrong way, when they turn |
| holds through the walk | the walk's variance, losses weighted 1.5 |

## Predictions

0.15.0, before any model run: the task is about one number (the take-back)
and one read (who is informed). A model that reads the instruction fades by
the take-back and lands near the reference (0.5); one that decodes the
stretch's flow per wallet and keeps the wallets that don't turn gets most of
the way to 1. Few will; rewards don't compare with 0.14's.

## Generations

| Version | What happened | What changed |
| --- | --- | --- |
| g0 | Lab (2,000 sessions): reference $68.5 a session, told 126, gap-oracle 209, a generator-fitted regression 41. Engine ladder, two fresh 72-session sets (reference $2,496 and $3,342, reward 0.035 and 0.047): follow −1.24/−0.40, fade −0.13/0.09, basis −2.81/−2.03, fitted 0.38/0.30, told 2.08/1.94, gap-oracle 4.13/3.69, hindsight 28.4/21.4; every rule holds (bootstrap odds ≥ 0.94), the reference's deepest drawdown $331. Harbor: oracle skill 1.000 (reward 0.040), cheater 0 (tampered), sneaky 0. A Sol run found in its first minutes that the worker's last order can be under 0.2 shares, which the informed trader's never is (the worker's remainder): a tell for about a fifth of worker sessions  Screen, 3 GPT-6.1 Sol runs (one 72-session set, reference $4,558): rewards 0.225, 0.226, 0.201, skill 3.58, 3.59, 3.19, profitable in 74–78% of sessions. All three decoded every taker order per account and modelled permanent against decaying impact (a mixture of state-space models; a Kalman mixture; a gradient-boosted 64-slot forecast trained on 6,000 generated sessions), and traded the whole closed period ($124–131 a session there, against the reference's −$16); two also followed the reopen's index walk through its later steps | — (new task) |
| g1 | Ladder, two fresh sets (one container per rung): follow 0.12/0.06, fade and basis 0 (breaches), fitted 0.27/0.15, g0 reference 0.41/0.30, told 1.08/1.19, path oracle 0.92/1.10, frozen g0 Sol bots 1.06/1.09, 1.07/1.31, 0.83/0.92, hindsight 4.4/6.0: the reference is now about as strong as the bots fitted to the family; the told rung is too close to it and the path oracle too weak, so the ladder fails its told and exploit gates. Harbor oracle: skill 1.000, reward 0.219. Screen, 3 GPT-6.1 Sol runs (one 72-session set, reference $15,975): rewards 0.210, 0.230, 0.187, skill 0.95, 1.04, 0.85; no breaches, refused calls or overruns. The two below the reference beat it in the closed period ($135 and $116 a session against $90) and lost it at the reopen ($76 and $72 against $131): both cut their exposure into the reopen against a conservative open shock, where the family's odds let the reference hold | The reference fitted to the generator the way the g0 bots were (structural model plus a gradient-boosted 64-slot forecast on 8,000 generated sessions, trading the whole closed period); the reopen snaps in one slot; the worker's last order stays in the size range; told retrained with who-is-who flags; a path oracle; the g0 Sol bots as frozen rungs |
| g1 (ladder repair) | Two fresh sets, one container per rung: follow −0.15/0.05, fade −0.10/−0.24, basis 0, fitted 0.20/0.27, g0 reference 0.31/0.21, frozen g0 Sol bots 1.02/1.00, 1.20/1.17, 0.89/0.80, told 1.53/1.39, path oracle 2.25/1.99, hindsight 5.2/4.4. Every rule holds (odds 1.000), told beats every frozen bot on both sets (odds 1.000): the ladder holds. A straight lookahead oracle (cap toward the level 16–128 slots ahead) loses money or barely beats the reference in the lab: the wobble charges every flip | Told is told the regime (who is the informed trader and the worker, the half-life, where SPCX opens, as flash-crash's regime oracle is told where its session ends), gets the exact permanent/transitory split as features, and holds the known gap outright in its last 64 slots. The path oracle plans its positions over the whole known path by dynamic programming after costs. Reference, family and instruction unchanged |
| 0.15.0 | Built on mainnet's measurements (RFD 0007; see the top of this file). Practice (6, seed 15): reference $6.4, informed $18.7. Fast ladder, one 12-session set (`gates-fast-v1`), macOS: nop $0, follow −$494 (v2 raw −1.0), fade at the cap +$135 (raw 0.61), reference +$118 (0.50), informed +$133 (1.00). Informed and reference held the same position into the reopen in 10 of 12 sessions (the informed wallets' votes agreed with the take-back, cancelled, or were absent); loss-weighted, the reference made 0.75 of the informed bot's, above the band's 0.7 on twelve sessions (fast mode: inconclusive). The offline model of the family gives 0.41 over 100,000 sessions. The summary and band check didn't complete: surfnets lost their RPC mid-session on this machine in later ladder runs (fade and reference rungs, five attempts) | The family `weekend-gap-mainnet`; the reference fades the take-back; an informed bot told the informed wallets; scoring v2; the rungs and frozen bots fitted to the old family leave the ladder |
| 0.15.0 (reference repair) | astra's 6-set ladder (72 sessions, mtx-bench-2) on the take-back-only reference: loss-weighted reference/informed 0.199, reference Sharpe 0.214: too weak. The reference now reads every transaction on SPCX's market and its signer, treats wallets first seen in the last 40 slots as the positioners, and leans the take-back by their summed position as one vote. 6-set ladder (fresh sets) with the vote trusted at 65%: reference $1,270, informed $1,702, fade $732; loss-weighted 0.669, reference Sharpe 0.454, informed 0.628 (band met), but the reference beat the informed bot on set 3 ($427 vs $402), so `reference < informed` held at odds 0.761 (needs 0.85). Trusted at 60% for one aborted ladder, then back to 65% (below) | The reference reads the late positioners' net flow (`tape.rs`); the vote posterior and the position reader shared in `reopen.rs`; practice recalibrated |
| 0.15.0 (derived trust, informed reads silence) | The reference's trust in the positioners' vote is derived, not fitted: two informed wallets right 80% of the time and two decoys trading a random view (50%) once they lose the thread make a late wallet right (2 × 0.8 + 2 × 0.5) / 4 = 65% of the time (a decoy is as right as the informed before it turns, so this leans low). A 60% trial, tuned to the gate, was reverted; its one ladder stopped on a set both anchors lost. The informed bot now uses every process fact: the latent adds the positioners' edge (10 bp), view noise (10 bp) and pace (3 orders a wallet), and it weighs a grid of gaps by the prior and each informed wallet's record, silence included (a wallet trades only while the gap it sees is past its edge) | Latent: `edge_bps`, `view_noise_bps`, `orders_per_wallet`; informed bot's grid posterior; reference back to 65% |
| 0.15.0 (ladder, derived 65% and informed reading silence) | 6-set ladder on mtx-bench-2 (`target/ladder6e`), fresh secret. It stopped on set 5: the informed bot lost $123 there (the reference made $32), and v2 can't scale a set whose ceiling sums negative. Over the 60 sessions graded: reference $661 (loss-weighted $499, per-session Sharpe 0.312), informed $859 ($656, Sharpe 0.366), fade $215; reference/informed 0.760, above the band's 0.7. Per set, reference/informed $: 333/371, 174/208, 96/240, 26/164, 32/−123; `follow < reference` on every set. Practice (6, seed 15): reference $11, informed $19. The informed bot's edge over the reference is real on four sets of five but small against the open's 0.50% residual: reading silence didn't widen it here | — |
| 0.15.0 (positioners know more) | Decided with astra: keep every measured number and strengthen only the stated departure, what the positioners know, not how often they're there. Their hit rate goes from 80% to 95% (decoys share it until they turn); the reference's derived trust follows, (2 × 0.95 + 2 × 0.5) / 4 = 72.5%. Offline (40,000 sessions, losses ×1.5), reference/informed went 0.67 → 0.62 and the informed bot's per-session Sharpe 0.61 → 0.91. Replay check first: the same binaries and secret give byte-identical PnL (3 drawn sessions, twice); a grade with no secret (practice calibration) draws fresh session keys, and with them the library actors' draws, so practice anchors' PnL differs between calibrations by design, not a determinism bug | Family `hit_rate` 0.95; reference `FLOW_HIT_RATE` 0.725 |
| 0.15.0 (ladder, positioners at 95%) | 6-set ladder on mtx015c (`target/ladder6f`), fresh secret; it stopped on set 4, where the informed bot made +$50 raw but −$6 loss-weighted, and v2 can't scale a set whose ceiling (the informed bot's loss-weighted sum) is negative. Over the 48 sessions graded: reference $475 (loss-weighted $318, per-session Sharpe 0.309), informed $952 ($854, Sharpe 0.621), fade $49; reference/informed 0.373, inside 0.2–0.7; informed above reference on all four sets ($208/$252, $52/$265, $192/$386, $23/$50); `follow < reference` on every set. Anchors' PnL is identical across the rungs' grades of a set (same secret): no determinism problem. With a per-session Sharpe near 0.6, the informed bot's loss-weighted sum over 12 sessions is negative on roughly one set in ten, which stops a ladder | — |
| 0.15.0 (gates and the 3 × 24 ladder) | `mtx gates weekend-gap-spcx --fast` on mtx015c: passed, 23 passed, 0 failed, 1 inconclusive (the market-calibration check: stylised paths), 7 not run (full mode). Ladder, 3 sets of 24 sessions, fresh secret (`target/ladder3x24`): reference $1,278 (loss-weighted $1,076, per-session Sharpe 0.415), informed $2,239 ($2,098, Sharpe 0.710), fade $640; reference/informed 0.513; per set, reference/informed $271/$855, $605/$809, $402/$574; `follow < reference` (odds 0.996) and `reference < informed` (odds 0.908) hold on every set; no rung refused or disqualified | — |
