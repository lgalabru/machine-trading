---
name: sf-trading-task-design
description: Designs, reviews, and hardens trading tasks for agent evals and RL environments — tasks where an agent writes a trading bot that is graded on replayed or simulated markets (Harbor tasks on Surfpool snapshots in open-trading, or any backtest/sandbox benchmark). Covers scenario families, hidden vs practice sets, PnL scoring and normalization, anti-cheat (RPC guards, sandboxing, cheatcodes), contamination, market realism (fees, slippage, liquidity, MEV, oracles, liquidations), statistical power, and the oracle/nop/cheater check suite. Use when user asks to "design a trading task", "add a new task to open-trading", "write a scenario family", "review this task", "is this task cheatable", "how should we score this bot", "make this a good RL environment", "calibrate difficulty", or compares trading benchmarks (StockBench, Alpha Arena, LiveTradeBench, FinRL, Solana Bench).
license: MIT
metadata:
  author: Solana Foundation
  version: 0.1.0
  repo: lgalabru/open-trading
---

# Designing great trading tasks

A trading task asks an agent to produce a **strategy as code** (a bot), then
scores the bot by running it through markets the agent never saw. A great
trading task has a score that moves only when trading skill moves. Luck,
memorized history, reading the answers, and cheat paths must not move it.

Everything below follows from that one sentence. When a design choice is
unclear, ask which option keeps the score tied to skill.

## When to use

- Creating a new task or scenario family in this repository (`tasks/<name>`)
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
| Score | `clamp(pnl / reference_pnl, 0, 1)` per scenario, mean across scenarios; also report raw `pnl_usd` | Comparable across scenarios; raw PnL keeps the signal above the reference |
| Disqualification | Score 0 for any refused RPC call or for ending liquidatable | Cheating and ruin must never pay |
| Valuation | From on-chain end state at the mark, never from bot logs | Grade outcomes, not claims |
| Checks | oracle ≈ 1, nop = 0, cheater = 0, all through Harbor | Proves solvable, non-trivial, and hack-resistant |

## Operating procedure

### 1. Write the skill thesis first

Write one sentence: *"A good bot profits here because it ___."* Examples:
"…buys forced liquidations below fair value and sells the rebound."
"…quotes both sides and skews inventory when the oracle moves."
If you can't fill in the blank, the task measures noise. The thesis decides
the family, the reference bot, and what the instruction may disclose.

### 2. Design the scenario family

A family is a generator: `(seed) → scenario`. Parameterize what varies and
pin what doesn't. In this repo, see `CrashFamily` in `crates/core/src/scenario.rs`.

- **Vary the dimensions that test the thesis:** timing, depth, number of legs, recovery fraction, duration.
- **Include adversarial draws:**
  - a crash that doesn't recover, so blind dip-buying gets punished
  - a fake-out two-leg move
  - a no-event scenario, if waiting patiently matters to the thesis
- **Respect protocol physics.** For example, Phoenix's oracle band is 5%, so moves must step at ≤4%. A path the protocol would reject tests your simulator, not the bot.
- **Make the regime knowable but the draw unknown.** Tell the agent the family ranges in `instruction.md` and keep the seeds secret. Ambiguity about the rules is a task bug; uncertainty about the outcome is the game.

### 3. Write the reference bot, then calibrate

- The reference sets the 1.0 line. Make it a reasonable, simple strategy, not an optimum: a ceiling the agent can only match leaves no headroom. Calibrate it on every scored scenario with `just calibrate`.
- Drop or regenerate scenarios where the reference doesn't profit cleanly. A non-positive reference can't normalize anything.
- Keep the oracle (`solution/`) identical to `bots/reference` (`just check-solution`).

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
`checks/` for each path you close.

### 6. Make the market honest

Replay realism decides whether profit means skill. Fees, slippage, depth,
latency, funding, liquidations, MEV and oracle behavior: see
[references/market-realism.md](references/market-realism.md). When realism is
missing, say so in the task README (for example, "static replay: makers don't
react to the bot").

### 7. Verify, then calibrate against real agents

1. `just harbor-checks`: oracle ≈ 1, nop = 0, cheater = 0.
2. `harbor lint --select ALL` and `harbor check tasks/<name>` (an LLM rubric review).
3. Run real agents k times each (`harbor run -p tasks/<name> -a claude-code -m <model> -k 5`, for example).
4. Run `harbor analyze jobs/<job>` to flag reward hacking and underspecified instructions.
5. **Target spread:** the best model mean should land roughly between 0.2 and 0.7, with visible variance across models. If everything sits at 0 or 1, the task teaches nothing. When every agent fails, suspect the task first.

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

## Reference files

- [references/benchmarks.md](references/benchmarks.md): existing trading and finance agent benchmarks, what each gets right and wrong
- [references/anti-cheat.md](references/anti-cheat.md): cheat surfaces and mitigations for bot-writing tasks on Surfpool
- [references/market-realism.md](references/market-realism.md): fees, impact, liquidity, MEV, oracles, funding, liquidations
- [references/quant-pitfalls.md](references/quant-pitfalls.md): overfitting, contamination, statistical power, metrics, RL reward design
- [references/review-checklist.md](references/review-checklist.md): the pre-publish checklist, including Harbor's own quality rubric
