# How bots are graded

This page explains scoring v2: what a bot's score means, how it is worked out, and why we think it is fair. It is for task authors, for people reading a leaderboard, and for anyone who wants to cite the method.

The exact behaviour is in `crates/grader/src/scoring_v2.rs`. The scenario fields it reads are in [scenario-format.md](scenario-format.md).

No task has a real informed bot yet. Until one does, every task uses an interim ceiling, described under Anchors.

## The idea in one minute

A bot trades 72 short sessions. We want one number that says how good it is. Three bots define the scale:

- **Doing nothing** (the `nop` bot) scores **0**.
- **The reference bot** scores **0.5**. It is a simple, honest bot that ships with the task.
- **The informed bot** scores **1**. It knows every fact the task's data could tell in principle, and trades under the same costs and limits. It does not know the future.

Your bot lands somewhere on that line. Below 0 means it did worse than sitting still. Above 1 means it beat the informed bot.

Two rules keep the number honest. Losses count more than gains, so a lucky gamble does not look like skill. And breaking the rules never pays.

## What the score does not replace

Today each task still reports its old `reward` and `skill`. Scoring v2 is reported beside them, as `score_v2` and friends in `reward.json`. A task's v2 score becomes its headline only when the task has a real informed bot and a reference that passes the strength band. See [Status](#status).

## One running example

We follow one bot, "Bot X", on a made-up task with 8 sessions. Real tasks have 72, and the arithmetic is the same. Dollar figures are session PnL. The mandate's drawdown limit `L` is $100.

| Session | 1 | 2 | 3 | 4 | 5 | 6 | 7 | 8 |
|---|---|---|---|---|---|---|---|---|
| Reference | +60 | +40 | −20 | +50 | +30 | −10 | +70 | +20 |
| Informed bot | +150 | +110 | +20 | +120 | +90 | +10 | +160 | +80 |
| Bot X | +90 | +60 | −40 | +80 | +50 | −30 | +100 | +30 |

Doing nothing makes $0 in every session.

## Anchors: what 0, 0.5 and 1 mean

The three anchors are measured on the same sessions as the bot, not guessed.

- **0** is the `nop` bot, which makes $0. v2 assumes a flat start with no carry, so doing nothing is $0. A session that starts holding something would need a measured nop run, which v2 does not do yet, and it refuses such a grade.
- **0.5** is the task's reference bot. Its PnL on every session is stored in the scenario (`reference_pnl_usd`), or measured by running it first on a fresh hidden set.
- **1** is the informed bot (`informed_pnl_usd` on the scenario). Reaching it is possible by definition: it is a strategy anyone could write who knew the task's model. A gap to it is a skill gap.

**While a task has no informed bot**, the 1 falls back to the cost-aware hindsight bound. This is what a trader with perfect foresight could make after paying the venue's taker fee and half-spread on every trade (8.5 bp together by default). It uses the reference's price marks, never the bot's own, so a bot cannot lower its own ceiling by jittering its marks. It has no free parameter and no model run in it. It cannot be reached, so it is an interim 1, and the report says `ceiling: "hindsight"`. A grade with no reference session cannot use it, and v2 refuses such a grade.

The map from dollars to score is two straight lines. With `E` the weighted sum of PnL (next section):

```text
raw = 0.5 · E_bot / E_ref                              if E_bot ≤ E_ref
raw = 0.5 + 0.5 · (E_bot − E_ref) / (E_inf − E_ref)    otherwise
```

`raw` is never below −1. It is not capped above, so a bot that beats the informed bot scores over 1 and is flagged `beats_informed`. The reported `score` is `raw` clamped to 0 to 1, for display. `edge` is `raw − 0.5`: the bot's edge over the reference.

**If the reference is too weak to anchor anything.** If the reference's weighted PnL is under a small share of the ceiling's, the 0.5 is set at that share instead (10% with an informed bot, 5% with the hindsight ceiling), and the grade says `anchor_floored`. This avoids a 0.5 that sits at or below doing nothing. It is a warning that the task needs a stronger reference.

## Losses count 1.5 times

A losing session counts `k = 1.5` times its size. A winning session counts once. There is no floor per session:

```text
w(x) = x            if x ≥ 0
w(x) = 1.5 · x      if x < 0
```

Why? Without it, a coin-flip bot can score well. It wins big in a few sessions, loses small in the rest, and the totals look fine. With losses weighted, a bot with no edge scores no better than doing nothing, which is what a desk would say.

In the example:

- Reference: gains 270, losses −30. Weighted: 270 − 45 = **225**.
- Informed bot: no losses, so **740**.
- Bot X: gains 410, losses −70. Weighted: 410 − 105 = **305**.

Bot X is above the reference, so it uses the second line:

`raw = 0.5 + 0.5 · (305 − 225) / (740 − 225) = 0.578`

Now compare Bot G, a gambler that makes +200, −150, +180, −160, +170, −140, +190, −120. Its plain total is +$170, which sounds fine. Weighted, its gains are 740 and its losses are −570 × 1.5 = −855, so `E = −115` and `raw = −0.26`. It scores below doing nothing, because it earned its total by risking far more than it made.

### The price: the break-even Sharpe

Weighting losses has a cost, and we state it. An honest strategy whose sessions are too noisy for its edge also scores below nop. If session PnL is roughly normal, the line is a per-session Sharpe of about **0.16** at `k = 1.5` (mean session PnL over its standard deviation, not annualised). Below it, the strategy is profitable but a desk would call it too noisy to be worth anything. Above it, it beats doing nothing.

At `k = 2` the line would be 0.28, and a strategy with Sharpe 0.2 would score below nop. That is too harsh, which is why k is 1.5. The grade reports `break_even_sharpe` beside the score so readers can see where the line sits.

k is a policy choice, not a law. It is the smallest value at which every probe gambler stays at or below nop on every task, while honest strategies with Sharpe 0.2 and up still beat nop.

## The near-breach charge

Loss weighting handles bad gambles. One case is left: the lucky gambler. A bot that bets the whole cap, gets lucky, and ends up ahead looks good on the totals. A desk would not trust it.

So a bot that drew down far more than the anchor did pays a charge `Π`, in dollars, taken off its winnings before the map. The anchor is the informed bot when its drawdowns are known, else the reference.

For each session, take the bot's drawdown as a share of the limit, at most 1. Take the root mean square over the set: `ρ`. Do the same for the anchor: `ρ_A`. Then:

```text
s = max(1, E_bot / E_anchor)       f_s = min(drawdown_s / L_s, 1), and f_s = 1 for a breached session
Π = min( λ · k · Σ L_s · max(0, ρ − s · ρ_A)² ,  max(E_bot, 0) )     λ = 1
```

Three properties matter:

- **It only takes back winnings.** The `min` stops it from ever making a loss deeper.
- **It is zero for doing nothing, for the anchor itself, and for a bot that takes risk in proportion to its edge.** The allowance `s` grows with what the bot earns over the anchor. A skilful bot that bets bigger, and earns for it, pays nothing.
- **It is squared**, so a small excess costs almost nothing.

With `λ = 1`, a bot that drew down to its limit in every session while the anchor never moved pays `k · L` per session, the same as a breach.

Example: suppose Bot X's drawdowns average ρ = 0.6 of the limit, while the informed bot's ρ_A is 0.3. With the informed bot as anchor, `s = 1`. The excess is 0.3, so `Π = 1.5 × 800 × 0.3² = 108` (8 sessions × $100 = $800 of limits). Bot X's `E` falls from 305 to 197, which is under the reference's 225, so `raw = 0.5 × 197 / 225 = 0.44`. Bot X took the right positions but at too much risk, and the score says so.

If neither the informed bot's nor the reference's drawdowns are known, nothing is charged and the grade says so.

On the numbers: in offline tests this charge cut a lucky full-cap gambler's average score from 0.112 to 0.017 for about a 1% loss of resolution, and charged 0.000 to a skilful bot that bet the reference's bets at 2 and 3 times the size.

## Breaches

A **breach** is a session that breaks its mandate or its order, ends liquidatable with no liquidation keeper, or is stopped out by the risk desk. The first three disqualify the session.

The stop-out is how a drawdown breach is priced instead. When a session's drawdown goes past the mandate's limit, the risk desk closes the position at market and the session ends. The close pays the book's impact and the taker fee. The PnL is the account's value after the close. See `scoring.stop_out` in [scenario-format.md](scenario-format.md).

A breached session counts at least as the worst loss the mandate allows, weighted. It counts its real weighted loss when that is worse:

```text
Xw_s = min(−k · L, w(X_s))
```

So a breach never beats complying, and never caps a loss. In the example, a session with PnL −$250 and `L = $100` counts `min(−150, −375) = −375`. A compliant session that lost the full limit would count −150.

On top of that, breaches cost the whole run in proportion to how often they happen. The run's `raw` loses `c` times its breach rate:

```text
raw  −=  c · (breached sessions / sessions)          c = 1.25
```

So each 1% of sessions breached costs 0.0125 of score. Three breaches in 100 sessions cost 0.0375, and so do one in 33 or two in 67. Every breach costs the same `c / n`, so there is no cliff and no point where one more breach is free. The rate, not the count, is what is charged, so the cost doesn't change with how many sessions the task has. `raw` is still floored at −1. The grade reports the points taken as `breach_penalty` and `c` as `breach_rate_weight`; the gap report adds them to `risk`.

Why a penalty on top of the floor: with the floor alone, a bot that levers past its limit and is stopped out now and then can beat its own compliant twin, because it earns more in every session it isn't stopped out. In the offline tests (TUNE2's N1) that happened on two flash-crash tasks.

Why this shape and this `c` (scoring_v2/TUNE3_breach_rate.md):

- **Linear in the rate.** Breaching is charged per session, like the rest of the score, and its marginal cost is constant. Two other shapes were tested. A square-root penalty charges the first breaches so hard that an honest Sharpe-0.2 bot, which is stopped out about once in 130 sessions, falls below doing nothing at every value that stops the breacher. A squared penalty barely charges occasional breaches, so it needs a larger constant and still leaves the breacher a thinner margin than linear does for the same cost to honest bots.
- **`c = 1.25`.** The probes set a window. Below about 0.82, the reference levered to 11 times its size on 0.11's flash-crash, stopped out in 4% of sessions, still beats its compliant twin. Above about 1.7, the honest Sharpe-0.2 bot on 0.12's funding-carry falls below nop. 1.25 sits inside it. It was checked on session sets of 24, 48, 72 and 100, with the constant fitted on 0.10 and 0.11 and validated on 0.12. Across breach rates of 1.4% to 20%, the occasional breacher scores at least 0.012 below its twin on the fit tasks and 0.017 below on the held-out ones.
- **What it costs a competent bot.** For a bot at the reference's level, stopped out in 1%, 2% or 3% of 100 sessions, the penalty is 0.0125, 0.025 or 0.0375. The floor is the larger cost: on these tasks it takes about 0.06 per 1% of sessions. A 3% stop-out rate takes a reference-level bot from 0.5 to about 0.27 on average. Under the old cap, it went to 0.

One limit. The penalty makes breaching lose in expectation, not in every run. A heavily levered bot that happens to be stopped out once in 100 sessions, where it would usually be stopped out four times, can beat its twin in that run by up to 0.04. Closing that would take `c` of about 6, which would push honest Sharpe-0.2 bots below nop and cost a 3% stop-out rate 0.18 more. Headlines average several runs, which is where this luck averages out.

The run cap this replaces held `raw` to 0.5 after one breach and to 0 after two. It was fitted on 72-session runs. At 100 sessions, a 2–3% stop-out rate, which a competent desk can have, zeroed the run: in card 8's pilot every graded bot (3–5 breaches in 100) scored 0, where this rule gives them 0.44 to 0.48.

**A keeper liquidation is not a breach.** If a scenario has a liquidation keeper, a liquidation is priced into the PnL like any other loss: the account value after it is what counts. It is not counted as a breach and does not trigger the cap. With no keeper, ending the session liquidatable is a breach.

Breaches are checked on intra-session equity, marked to market each step. The report keeps `breached_sessions` and `breaches_per_100_sessions`.

In the example, if Bot X had the −$250 session instead of its −$40 one, `E_bot` would be −10, `raw` before the penalty −0.02, and after it (one breach in 8 sessions, 1.25 / 8 = 0.16) −0.18: the breach alone took it from 0.58 to below nothing.

## Fatal runs score −1

Some things end a grade outright: calling a method the guard refuses, tampering with the grade, or not being graded at all. These score `raw = −1`, the floor. They are not scored 0, because then failing on purpose would beat losing.

What the sessions made is still reported as `measured_raw`, beside the score, but not scored.

Machine and harness failures are different. If the grader itself fails, the grade is an infrastructure error. It is retried, reported apart, and never scored.

## From runs to a headline

A model plays a task several times. Its headline for that task is:

```text
headline = clamp( mean of raw over runs , 0, 1 )
```

You average `raw` first and clamp once, at the end. You do not average clamped scores. Clamping each run would turn every losing run into 0 and hide how bad it was.

Example: five runs with raw 0.58, 0.40, −0.20, 0.60 and −1 (a fatal run). The mean is 0.076, so the headline is **0.08**. The average of clamped scores would have been 0.32.

## Statistics: what we report and why

**Raw beside clamped.** Today the Harbor `reward` is still the v1 reward. It becomes the clamped v2 score when a task's v2 score becomes its headline, which is fine for an eval. `reward.json` already carries `score_v2_raw`, `score_v2_raw_unfloored` and all the metrics. Anyone training on these scores should use raw, or `tanh(raw)`, never the clamped value. Clamping flattens every losing episode to 0 and gives no learning signal.

**One run's interval.** A single grade carries a 95% interval on `raw`, from a paired bootstrap over its sessions (2,000 resamples, the same session draws for the bot, the reference and the ceiling). It says how much one run's luck of the sessions could move the score. It is labelled `ci_scope: "sessions"` and `ci_use: "not for ranking"`. `within_noise` is true when the interval on `edge` includes 0: the bot cannot be told apart from the reference on this set.

**Comparing models.** `mtx bench rescore` runs a paired hierarchical bootstrap. Each resample draws the scenarios with replacement (the same draws for every model, so differences are paired), then draws each model's runs within the model. For each pair of models it reports the difference in mean `raw` with its interval, a two-sided p-value, and the Benjamini-Hochberg adjustment over the task's pairs. The pair is a **tie** when the adjusted p is 0.05 or more. Ranks are published as tie groups, not as a strict order.

Naive per-run intervals are about twice too narrow, and most 5-run gaps between close models turn out to be ties. Add runs only for pairs that matter. Runs that did not play the same scenarios are not compared.

**Reproducible.** The bootstrap is seeded from the grade's `eval_seed_sha256`, so a rescore reproduces every interval exactly.

**Across tasks.** The cross-task figure will be the mean of the per-task 0 to 1 scores with a 95% interval. We will also report the mean raw score, so negatives stay visible, and the interquartile mean as `rliable` defines it.

**Raw metrics.** Beside every score, the grade reports return on capital, per-session Sharpe, drawdown as a share of the limit (mean and worst), profitable share, worst session, usd lost, and the number of breaches and stop-outs. None of these is scored. They explain a score and keep the harm as prominent as the number. There is no Sortino.

## The probe gate

Reward hacking cannot be ruled out. It can only be tested for. Before a task's v2 score is a headline, a set of probe bots must land where they should. This is a blocking release check.

This table may still change.

| Probe | What it tests | Required result |
|---|---|---|
| nop | The bottom anchor | Exactly 0 |
| Zero-edge coin-flip, naive full-cap long and short, martingale, limit-hugger | A gamble with no edge must not look like skill | At or below nop (to be confirmed: band) |
| Honest but noisy bots, Sharpe 0.1 to 0.5 | A real edge is credited | Above nop when measured Sharpe is above the break-even (about 0.16); judged on measured Sharpe |
| Reference | The middle anchor | Exactly 0.5, unless `anchor_floored` |
| Informed bot | The top anchor | Exactly 1 |
| Levered reference | Leverage does not pay by itself | A leveraged bot that breaches never pays; levered by m never scores more than m times the reference's weighted PnL |
| Stop-lotto (coin-flip entry, tight trailing stop) | A trick that looks like risk control | To be confirmed |
| Occasional breacher | A bot that breaches now and then beats its compliant twin | The breach penalty binds (it takes `c · b / n` at b = 1, 2 and 3 breaches), and the breacher scores below its compliant twin |
| Crash-when-losing (fatal) | Failing on purpose when behind | Scores −1, so never above the bot it wraps |
| Crash-when-losing (graceful) | Flattening and stopping when behind | Reported, not gated: this is a stop-loss, a real risk control |
| Short-vol and long-vol pair | Equal mean and spread, different tails | Long-vol is not below short-vol |
| Adaptive bot | A bot that gambles when behind | Does not beat its stationary version |
| Exploit bots (cheater, sneaky) | Harness holes | Score 0 or fatal, per the task |

All probes run with fees. A bot that flattens and stops trading when it is behind is a stop-loss, which a desk rewards, so it is reported and not gated. It can score a little above a noisy honest bot, because the near-breach charge takes a noisy bot's thin winnings while a bot that cuts losses early pays none.

A known residual: a coin-flip entry with a trailing stop has a weak but real edge on funding-carry and weekend-gap, with a Sharpe near 0.2. It scores a little above nop there, about the same as an honest Sharpe-0.2 bot. That is a property of those task generators (paths that trend within a session), so the fix belongs in the task and the reference's strength band, not in the grade.

## Fairness

**What agents are told.** Agents are to be told the scoring rule in plain words in the task prompt (planned): the three anchors, that losses count more, and that breaches and reckless betting are charged. Real traders know their mandate and how they are judged. Hidden rules would make failures unfair. The grader's code stays out of the sandbox.

**What is not scored, and why.**

- **Profitable share and the worst session.** Reported, never scored. A rule that capped the best session would punish legitimate profit that arrives in a few bunches.
- **Certainty-equivalent or utility-style scores.** They double count the concavity that loss weighting already supplies, and they charge real edge.
- **Expected-shortfall and exposure-times-time charges.** Both were tested. Both pushed honest Sharpe-0.2 strategies below nop on held-out tasks, so they would charge a real, weak edge.
- **The plain hindsight bound.** Never in the score. It is reported as the luck headroom above the ceiling.
- **Sortino and annualised numbers.** Sessions are minutes long, and annualising would mean nothing.

**Why a ceiling that is not tied to the models.** A ceiling taken from the best model run would tie the scale to the models being graded. The informed bot, or failing that the hindsight bound, has no model run in it.

**How this compares with common practice.**

- Many RL benchmarks report scores normalised between a random policy and a reference. Our 0 and 0.5 are the same idea, with a fixed, never-graded bot at each end. The anchors come from fixed bots, not from the models being graded.
- `rliable` (Agarwal et al.) recommends interval estimates, the interquartile mean and performance profiles instead of a bare mean. We report intervals and keep the raw mean so negatives show, and will add the IQM.
- Prop-trading firms already work this way. They cap drawdown, stop traders out on a breach and judge consistency, not just totals. The stop-out and the breach-rate penalty copy that.

Two honest limits. First, k is a choice, and it makes some profitable but noisy strategies score under nop. We publish the break-even Sharpe so nobody has to guess. Second, a 72-session run cannot always tell a lucky bot at the anchor's own level of risk from a skilled one. The interval is what flags that.

## Status

**Merged:**

- The anchor map, k = 1.5, the anchor floor, and the fatal floor at −1.
- The near-breach charge (λ = 1), stop-out breaches and the breach-rate penalty (`c` = 1.25), which replaced the run cap.
- The cost-aware hindsight fallback and the informed-bot ceiling.
- The informed bot run on the hidden set it anchors (`mtx grade --informed`): told each session's latent state (the regime, who is who, the funding process), never its future, with nothing of it reaching the bot. One more bot run per session.
- The single-run interval, and the paired hierarchical comparison with tie groups.
- Raw metrics and the gap report (`wrong_read`, `missed`, `execution`, `risk`, `noise`).
- The break-even Sharpe, reported with every score.

**Planned:**

- **Informed bots for the four existing tasks** (flash-crash, funding-carry, weekend-gap, market-making), planned for 0.13. Until then they use the hindsight ceiling and v2 is reported beside the old reward. New tasks are planned to ship with one from the start.
- **The probe gate** as a blocking release check. The table above is a draft until it lands.
- **Market making.** Its 0.5 does not hold yet, because the reference is too weak, so its scores are flagged `anchor_floored` and it is kept out of the v2 headline until it is redesigned. The redesign has the grader close out the position at the end of each session, limits market orders to reducing the position, adds quoting obligations, and replaces the reference with a real market maker.
- **Reporting.** The interquartile mean across tasks is not yet in the code. The headline over a frozen common task set, with floored tasks left out, is also planned.
- **The task prompts** will carry the scoring rule in plain words. The wording is to be added with each task.
- **Not yet in v2:** a measured nop anchor for sessions that start holding something, a gap report that follows the informed bot step by step, and the optional tail-gap penalty.
