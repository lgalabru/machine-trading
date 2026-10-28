# Quant pitfalls, translated into task design

Quant finance spent decades learning why backtests lie. Every lesson
applies to agent tasks, usually more strongly, because an agent can iterate
faster than any researcher.

## Contents

- Overfitting and multiple testing
- Contamination and look-ahead
- Statistical power with few scenarios
- Choosing metrics
- Reliability across trials
- RL reward design

## Overfitting and multiple testing

- An agent that runs the practice set 50 times and keeps the best variant has run 50 trials. That is exactly the setting of the [Probability of Backtest Overfitting](https://papers.ssrn.com/abstract=2326253) (Bailey, Borwein, López de Prado, Zhu) and the [Deflated Sharpe Ratio](https://papers.ssrn.com/abstract=2460551). Expect practice scores to be optimistic relative to hidden scores.
- [Harvey, Liu & Zhu](https://www.nber.org/papers/w20592) argue that new factors should clear t > 3, not 2. Apply the same skepticism to small score gaps between bots.

**Design rules:**

- Score only on hidden scenarios, and keep practice disjoint from them.
- Track the practice-vs-hidden gap per agent. A large gap means overfitting, and that is itself a useful signal.
- Generate hidden scenarios from the same family but different seeds. Rotate them each release.

## Contamination and look-ahead

- Point-in-time data stops leakage from the dataset, but not from the model's weights. Backtested LLM returns collapse after the knowledge cutoff ([Profit Mirage](https://arxiv.org/abs/2510.07920), [Look-Ahead-Bench](https://arxiv.org/abs/2601.13770)).
- Snapshots carry state, not future prices, so a single snapshot leaks little. Replaying a **historical** price path (the May 2021 crash, the FTX collapse) leaks a lot.

**Design rules:**

- Prefer synthetic or perturbed paths over historical ones.
- If you replay history, use periods after the evaluated models' training cutoff, and re-snapshot regularly.
- Keep the scenario family's *ranges* public and its *draws* private.

## Statistical power with few scenarios

- PnL per scenario is noisy: the same bot scores differently on different draws. With n scenarios, the standard error of the mean score is about σ/√n.
- Back-of-envelope: if per-scenario scores have σ ≈ 0.3, then 3 scenarios give an SE of ≈ 0.17, which can't separate 0.5 from 0.8. Ten give ≈ 0.09, and 30 give ≈ 0.05.
- Sharpe ratios from short samples are badly estimated and inflated by serial correlation ([Lo 2002](https://rpc.cfainstitute.org/research/financial-analysts-journal/2002/the-statistics-of-sharpe-ratios)).

**Design rules:**

- Use 10+ hidden scenarios for an eval and 100+ generated ones for RL. Watch the verifier timeout: sessions run serially.
- Report per-scenario scores (`score_<name>`) as well as the mean, plus a bootstrap CI when comparing agents.
- Don't headline Sharpe on sub-hour sessions. Prefer normalized PnL with explicit risk gates.

## Choosing metrics

| Metric | Use for | Caveat |
| --- | --- | --- |
| Σ PnL / Σ reference PnL over the set, clamped to [0, 1] | The headline reward | No credit above the reference; report the unclamped ratio alongside. Per-scenario ratios break when the reference loses, so normalize the set |
| Raw `pnl_usd` | Ranking strong bots | Scale differs by scenario |
| Disqualification count | Cheat or ruin monitoring | Must be 0 for honest bots |
| Max drawdown, CVaR ([Rockafellar–Uryasev](https://ideas.repec.org/a/rsk/journ4/2161159.html)) | Risk gates in longer sessions | Needs a per-slot equity curve |
| Win rate vs reference | Consistency | Hides the size of losses |

- Prefer **gates over blends.** "Score 0 if liquidatable" is clearer and harder to game than "PnL minus λ·drawdown" with a tuned λ.
- Always publish baselines: nop (0), buy-and-hold or a naive strategy, and the reference (1).

## Reliability across trials

- Agents are stochastic. Run k ≥ 3 trials per agent (`harbor run -k`).
- τ-bench's pass^k (success on all k trials) shows reliability collapsing with k: GPT-4o fell to ~25% at pass^8 on τ-retail ([Sierra](https://sierra.ai/blog/benchmarking-ai-agents)).
- For trading, a useful analogue is the fraction of trials whose bot beats a threshold (for example, a reward of 0.5).
- Separate the two sources of variance: environment variance (scenario draws; fixed by using the same hidden set) and agent variance (k trials).

## RL reward design

- **Reward only what you want.** Final PnL, or per-step mark-to-market PnL (which telescopes to final PnL), keeps the optimal policy intact. Bonuses like "placed an order" or "used the SDK" are non-potential shaping and change the optimum ([Ng, Harada & Russell 1999](https://mlanthology.org/icml/1999/ng1999icml-policy/)). Agents will spam orders.
- **Generate procedurally.** Procgen agents needed hundreds to thousands of levels to generalize ([Cobbe et al.](https://arxiv.org/abs/1912.01588)). A family with seeded draws is the trading equivalent.
- **Hold out, and rotate.** Train on one seed range and evaluate on another. Public static sets decay: OpenAI retired SWE-bench Verified over contamination and flawed tests ([OpenAI, 2026](https://openai.com/index/why-we-no-longer-evaluate-swe-bench-verified/)).
- **Use a curriculum.** Order difficulty with single-leg crashes before two-leg ones, then non-recovering crashes, then reactive counterparties. Go from liquid markets to thin ones, and from spot to perps.
- **Determinism.** Same bot plus same scenario must give the same PnL. Verify by re-running the oracle; open-trading's sessions are bit-for-bit reproducible.
