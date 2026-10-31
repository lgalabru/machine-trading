# The skill ladder, ceilings and screens

A reward on its own says how much a bot made. A ladder says whether the task
ranks strategies the way a trader would, and ceilings say how much room is
left above the reference. Gate every scenario change on both. The examples
come from a treasury-unwind execution task.

## Contents

- The ladder
- Control pairs
- The calibration model and the power test
- Ceilings
- Screens and generations
- Reporting results

## The ladder

Rungs are fixed bots, from harmful to ideal. For an unwind:

| Rung | Score (reference = 1) |
| --- | --- |
| dump everything at the open | −0.20 |
| TWAP | 0.09 |
| sell at the cap | 0.08 |
| sell at the cap, hold in rises | 0.20 |
| ports of earlier model bots | 0.30–0.49 |
| reference | 1.00 |
| knows the regime | 1.07 |
| hindsight | 1.18 |

- **Write the order rule down** before tuning, as strict inequalities:
  `naive < min(TWAP, capfull)`, `max(TWAP, capfull) < capfull_hold < reference < hindsight`.
  Rungs that encode opposite bets (TWAP against sell-at-the-cap) may swap;
  say so instead of forcing an order.
- **The rule must hold on every fresh hidden set**, not on average. Estimate
  the probability per set with a bootstrap over per-regime pools; a rule that
  holds on 87% of sets is a result, one that holds on 60% is a task that
  ranks by luck.
- **A family change that reorders the ladder fails**, whatever it does to
  the models' scores.
- **The reference must be positive in every regime** and above every rung
  but the ceilings, or say where a rung leads it and why that's fair (a
  passive bot leads in calm sessions; the reference wins it back in trends).
- For machine-trading: nop, a blind directional bet, the dip-buyer, the
  reference, the exploit bots and hindsight are the natural rungs.

## Control pairs

To prove a mechanic is rewarded, run a bot and its control, identical but
for that mechanic: a bot that rests its slices against the same schedule sold
immediately. If the gap is near zero, the mechanic doesn't pay and the
physics needs a lever (the unwind task needed resting fills to count for only 30–50%
of a taker sale's impact before passive execution paid at all).

Likewise check that the reference's lead isn't only direction-taking: score
it against a neutral benchmark (the session's TWAP of the mark) as well as
against the nop.

## The calibration model and the power test

- **A calibration model** is a fast simulator of the engine (Python, seconds
  per session) that runs the ladder on hundreds of draws. Tune the family on
  the model; it's the only way to try ten families in an afternoon.
  machine-trading's ad-hoc equivalents are the exploit bots' own simulators
  (`flash-crash-fable`'s `sim`, `flash-crash-opus`'s `bot replay`).
- **A power test** then runs the same ladder on the real engine, on several
  fresh hidden sets, and checks the order rule on each. Keep both numbers:
  ports of model bots read differently on the model than on the engine, and
  that gap is the model's error.
- Implement the reward formula twice (the model and the grader) and
  cross-check them before trusting either.

## Ceilings

Three bots bound the honest optimum from above and below:

- **Hindsight** plans each session knowing its path. Unreachable; it sizes
  the total headroom.
- **The regime oracle** knows the session's regime parameters (not its path)
  and plans on sibling sessions drawn with those parameters. The gap between
  it and the reference is what reading the market better could still earn.
- **An approximate Bayes bot** runs a particle filter over the generator,
  reading what a bot can see, and plans on its posterior. It's a lower bound
  on the best honest score, and it measures how many blocks a perfect reader
  needs to recognise each regime, against how long real bots take to act.

Report headroom by regime: the oracle's lead is largest where knowing the
regime's end pays (climbs), hindsight's where timing a shock pays (crashes).

**On normalization:** reference-normalized rewards (`(bot − nop) / (reference − nop)`)
saturate once agents beat the reference, which machine-trading hit twice. They
work when the ceilings show the reference already captures most of the
honest optimum (the unwind task's reference captures 94% of its Bayes bot). When the ceilings sit
far above the reference, normalize by a bound instead, as machine-trading
does, and report the reference ratio alongside.

## Screens and generations

- **Screen with frontier models before launch**: three runs of the strongest
  model on the candidate task. If they score near the reference, the task
  is done before it ships.
- **Read their bots as evidence** of what the task rewards, then **port them
  as ladder rungs**, both as written and adapted to the model. The next
  generation's levers are designed so those rungs drop (the unwind task's went from
  0.84–0.92 to 0.20–0.44) while the reference stays at 1.
- **Real bots adapt better than ports**: a port of a previous generation's
  bot understates what the next model run will do. Re-screen after every
  generation.

## Reporting results

- **Bootstrap intervals that resample both sessions and runs**: draw the
  hidden sessions (shared by all models, graded on one set) and each model's
  runs. Report 90% intervals and say which gaps aren't resolved.
- **Report every score with its time limit.** On the unwind task the ranking
  flips between 30 minutes and 2 hours. A model that stops early and one
  that iterates are different results at different limits.
- **Void, don't zero, infrastructure failures**: a trial with no reward, or
  whose anchor bots (reference or nop) overran on a loaded host, is re-run,
  not scored 0. A bot's own crash is the bot's failure and scores as it
  stands.
- **Commit every graded rollout** with its bot and per-session scores, so a
  reader can re-derive the table.
- **Pin the harness**: agent version, reasoning effort, disabled tools
  (web search, memory), so numbers stay comparable across releases.
