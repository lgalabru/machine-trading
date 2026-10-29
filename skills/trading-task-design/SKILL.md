---
name: trading-task-design
description: Designs, reviews, and hardens trading tasks for agent evals and RL environments — tasks where an agent writes a trading bot that is graded on replayed or simulated markets (Harbor tasks on Surfpool snapshots in machine-trading, or any backtest/sandbox benchmark). Covers scenario families, hidden vs practice sets, PnL scoring and normalization, anti-cheat (RPC guards, sandboxing, cheatcodes), contamination, market realism (fees, slippage, liquidity, MEV, oracles, liquidations), statistical power, and the oracle/nop/cheater check suite. Use when user asks to "design a trading task", "add a new task to machine-trading", "write a scenario family", "review this task", "is this task cheatable", "how should we score this bot", "make this a good RL environment", "calibrate difficulty", or compares trading benchmarks (StockBench, Alpha Arena, LiveTradeBench, FinRL, Solana Bench).
license: MIT
metadata:
  author: Solana Foundation
  version: 0.2.0
  repo: lgalabru/machine-trading
---

# Designing great trading tasks

A trading task asks an agent to produce a **strategy as code** (a bot), then
scores the bot by running it through markets the agent never saw. A great
trading task has a score that moves only when trading skill moves. Luck,
memorized history, reading the answers, and cheat paths must not move it.

Everything below follows from that one sentence. When a design choice is
unclear, ask which option keeps the score tied to skill.

## When to use

- Creating a new task or scenario family in this repository (`bench/tasks/<name>`)
- Reviewing an existing task before publishing to Harbor Hub
- Deciding how to score, normalize, or aggregate PnL
- Hardening a task against reward hacking
- Turning a task into an RL environment, or calibrating its difficulty

## Default decisions

Make these choices without asking unless the user says otherwise:

| Decision | Default | Why |
| --- | --- | --- |
| Deliverable | A bot directory (`/app/bot`, `run.sh`), not trades made by the agent itself | The agent's trades would be one sample; a bot can be replayed on many hidden markets |
| Environment | Offline Surfpool snapshot pinned to a slot, replayed deterministically | Reproducible; real program logic, real fees and liquidity |
| Market paths | Procedurally generated from a **family** with seeded parameters | Unlimited fresh scenarios; no memorized history |
| Scored set | Hidden scenarios in `tests/`, never in the agent image | Iterating on scored scenarios is overfitting |
| Practice set | Separate scenarios from the same family in `environment/practice` plus the generator | The agent can still do honest research |
| Score | `clamp(Σ pnl / Σ hindsight bound, 0, 1)` over the whole scored set, where a scenario's bound is the cap held the right way through every move at no cost (cap × start mark × the path's total variation). Report the reference's ratio as a baseline, and per-scenario PnL | Normalizing by a reference saturates as soon as agents beat it, which they did twice in machine-trading: once by sizing up, once by trading better. A bound no bot can reach keeps headroom forever, and losses still count |
| Position size | Cap exposure in the contract (`MTH_MAX_POSITION`), enforce it by sampling the position, and size the reference to the cap | PnL in dollars measures size as much as skill: without a cap, any bot that bets several times the reference's size and nets a profit saturates the reward |
| Risk | Gate it like a prop desk: a per-session drawdown limit (`MTH_MAX_DRAWDOWN`, ~10% of collateral) sampled twice a second, set so the reference stays inside it. Report worst session, tail loss, deepest drawdown, profitable share and Sharpe; don't blend them into the reward | PnL alone pays a bot that goes deep underwater and gets lucky. A gate is clear and hard to game; a weighted penalty rewards whatever weight you picked |
| Mandate | Put the market, cap and loss limit in one [mandate](../../specs/mandate.md) per scenario, hand the bot a copy (`MTH_MANDATE`), and audit every rule from the grader's own copy, including "no other market" | Limits scattered across env vars and grader branches drift apart. Phoenix offline lets a bot open BTC next to SPCX; until machine-trading audited the universe, nothing noticed |
| Disqualification | Reward 0 for any refused RPC call, any breach of the mandate (another market, a position over the cap, a drawdown past the limit), or any session ending liquidatable | Cheating, leverage and ruin must never pay |
| Direction | Make the first move unpredictable: mirror the family (crashes *and* squeezes, 50/50) and disclose the odds | If every scenario moves the same way first, a blind bet at the open is free money: in machine-trading, shorting the open made 2.4× the reference with no skill |
| Path shape | The path's shape gives away nothing a trader wouldn't see: moves ramp in uneven steps at uneven intervals, legs are drawn independently, phases that look alike share one duration range, the session ends a random while after the last move, and the mark wobbles in between | Frontier models reverse-engineer the generator. On machine-trading 0.4.0, Opus and Fable reached 3.5–3.9× the reference without reading the market, from equal steps (the first step gave the move's size) and a fixed tail (the end slot dated the aftermath) |
| Valuation | From on-chain end state, never from bot logs. At the mark for directional tasks; at the scenario's oracle when the bot quotes, since Phoenix's mark blends in the book | Grade outcomes, not claims. In machine-trading's market-making spike, one lopsided quote moved the mark 0.15%, and a maker quoting around the mark chased its own bid 8 bp above the oracle |
| Counterparties | When the thesis needs someone to trade with the bot (market making, execution), the grader runs them: takers sending ordinary transactions on a seeded schedule, from a shared pool of accounts, noise and informed overlapping in size | An offline surfnet has nobody else on it: a resting quote fills only when an override moves the price through it, which is pure adverse selection. Separate accounts for informed flow would let a bot read `getBlock` and front-run them |
| Checks | oracle at its calibrated baseline (`reference_ratio` ≈ 1), nop = 0, cheater = 0, all through Harbor | Proves solvable, non-trivial, and hack-resistant. Against a hindsight bound the oracle scores the baseline (~0.2), not 1 |

## Operating procedure

### 1. Write the skill thesis first

Write one sentence: *"A good bot profits here because it ___."* Examples:
"…buys forced liquidations below fair value and sells the rebound."
"…quotes both sides and skews inventory when the oracle moves."
If you can't fill in the blank, the task measures noise. The thesis decides
the family, the reference bot, and what the instruction may disclose.

### 2. Design the scenario family

A family is a generator: `(seed) → scenario`. Parameterize what varies and
pin what doesn't. In this repo, see `ShockFamily` in `crates/core/src/scenario.rs`.

- **Vary the dimensions that test the thesis:** timing, depth, number of legs, recovery fraction, duration.
- **Include adversarial draws:**
  - a crash that doesn't recover, so blind dip-buying gets punished
  - a fake-out two-leg move
  - a no-event scenario, if waiting patiently matters to the thesis
- **Respect protocol physics.** For example, Phoenix's oracle band is 5%, so moves must step at ≤4%. A path the protocol would reject tests your simulator, not the bot.
- **Make the regime knowable but the draw unknown.** Tell the agent the family ranges in `instruction.md` and keep the seeds secret. Ambiguity about the rules is a task bug; uncertainty about the outcome is the game.
- **Disclose distributions, not structure.** Agents read every range, in the instruction and in the practice files, as a rule to exploit. Disjoint ranges become decision rules: when a two-leg shock's first leg was 40–70% of its depth and a one-leg shock was at least 5%, Fable deduced "a leg under 5% means another is coming". Draw each part independently (memoryless legs, fixed continuation odds), and let ranges overlap wherever telling two phases apart should take skill.
- **Hide the generator's mechanics.** Anything the compiler of a path does deterministically is a signal: equal step sizes, fixed step spacing, a fixed tail after the last move, perfectly flat quiet periods. Randomize the step sizes and gaps, the tail, and add small noise. Keep the noise small: the hindsight bound counts it in the total variation.
- **Ask what a generator-aware bot would make.** Before publishing, read the generator as an adversary would (or have an agent do it, with the source): if knowing the generator alone, without reading the market, beats the reference by a wide margin, the family leaks.

### 3. Write the reference bot, then calibrate

- The reference sets the 1.0 line. Make it a reasonable, simple strategy, not an optimum: a ceiling the agent can only match leaves no headroom. Calibrate it on every scored scenario with `just calibrate`.
- Record the reference's PnL on every scenario, losses included: with set-level scoring only the set's total must be positive. Check that the reference profits over the family as a whole (calibrate a large sample), not just on average draws.
- Keep the oracle (`solution/`) identical to `bots/reference` (`just check-solution`).
- **Size the cap in notional, not units.** When a task moves to another market, re-derive the cap from collateral and the new price (100 SOL at $101 and 70 SPCX at $143 are both ~1× collateral), take a new snapshot, and let calibration confirm the book is deep enough to fill the reference at the cap.

### 4. Write the instruction as a contract

- **State every rule the grader enforces:**
  - the env vars
  - the RPC allowlist and the penalty for refused calls
  - the stop signal and grace period
  - the valuation formula
  - that the grader doesn't build the bot
- **State every claim truthfully:** paths, tools and timings must match the image.
- **Disclose the family, not the scenarios.** See [references/review-checklist.md](references/review-checklist.md).

### 5. Close the cheat paths

Assume the agent will try everything. Frontier models patch timers, stub
graders and read reference answers. Enumerate the cheat surface with
[references/anti-cheat.md](references/anti-cheat.md), and add an exploit to
`bench/checks/` for each path you close.

### 6. Make the market honest

Replay realism decides whether profit means skill. Fees, slippage, depth,
latency, funding, liquidations, MEV and oracle behavior: see
[references/market-realism.md](references/market-realism.md). When realism is
missing, say so in the task README (for example, "static replay: makers don't
react to the bot").

### 7. Verify, then calibrate against real agents

1. `just harbor-checks`: oracle at its baseline (`reference_ratio` ≈ 1), nop = 0, cheater = 0.
2. `harbor lint --select ALL` and `harbor check bench/tasks/<name>` (an LLM rubric review).
3. Run real agents k times each (`harbor run -p bench/tasks/<name> -a claude-code -m <model> -k 5`, for example).
4. Run `harbor analyze jobs/<job>` to flag reward hacking and underspecified instructions.
5. **Read the winning bots.** Agents explain their edge in the bot's doc comments: on machine-trading, every exploit so far was stated outright there ("Short from the start: the crash is certain", "every move is split into equal steps"). Treat a jump between model tiers as an exploit until the source says otherwise.
6. **Treat a too-clean risk profile as a leak.** Real edges have losing sessions. A worst session near zero, 90%+ of sessions profitable, or a Sharpe several times the reference's says the bot knows the script.
7. **Target spread:** the best model mean should land roughly between 0.2 and 0.7, with visible variance across models. If everything sits at 0 or 1, the task teaches nothing. When every agent fails, suspect the task first.

### 8. Size the scored set for statistical power

Three scenarios can't separate two bots. Use the bootstrap / standard-error
guidance in [references/quant-pitfalls.md](references/quant-pitfalls.md).
Rough rule: 10+ hidden scenarios for an eval, 100+ procedurally generated
ones for RL, with a private held-out pool that rotates.

## Anti-patterns

| Anti-pattern | What goes wrong |
| --- | --- |
| Scoring on historical price windows | Models memorized them; returns collapse after the cutoff |
| Scoring the agent's own live trades | One path, no reproducibility, luck dominates |
| Shipping hidden scenarios in the agent image | It's a practice set now |
| Rewarding activity (trade count, tool use) | Non-potential shaping; agents spam trades |
| Reading PnL from bot logs | The bot controls its logs |
| Blocklisting RPC methods | You'll miss one; allowlist instead |
| A reference that is the optimum | No headroom; every strong bot ties at 1.0 |
| No passive baseline | Can't tell skill from market drift |
| Free fills at mid | Every strategy looks profitable |
| A family whose first move is always the same way | A blind bet at the open wins every session |
| Normalizing by a reference agents can beat | The reward saturates at 1 and stops ranking anyone |
| Blending risk into the reward with a tuned weight | The weight becomes the thing agents optimize |
| Equal-sized steps, fixed spacing, or a fixed tail after the last move | The first step reveals the move and the end slot dates the aftermath: a bot can trade the script instead of the market |
| Disjoint ranges for phases that should look alike | Each boundary becomes a rule ("a pause over 60 slots means the shock is over") |
| A reference with a mechanical bias | Calibration measures the bias, not the strategy: machine-trading's first maker placed its bid before its ask on every requote, so the bid was live longer and it drifted long on balanced flow |
| No position cap, or a reference far below it | The reward measures leverage: the first real agent in machine-trading bet 6× the reference's size and hit 1.0 with an ordinary strategy |

## Reference files

- [references/benchmarks.md](references/benchmarks.md): existing trading and finance agent benchmarks, what each gets right and wrong
- [references/anti-cheat.md](references/anti-cheat.md): cheat surfaces and mitigations for bot-writing tasks on Surfpool
- [references/market-realism.md](references/market-realism.md): fees, impact, liquidity, MEV, oracles, funding, liquidations
- [references/quant-pitfalls.md](references/quant-pitfalls.md): overfitting, contamination, statistical power, metrics, RL reward design
- [references/review-checklist.md](references/review-checklist.md): the pre-publish checklist, including Harbor's own quality rubric
