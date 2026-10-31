# Existing trading and finance agent benchmarks

What exists, what each one teaches, and the gap machine-trading fills. Claims
marked *(secondary)* come from press or summaries, not the primary source.
Researched October 2026.

## Contents

- Backtest benchmarks (LLM era)
- Contamination studies
- Live-trading benchmarks
- Reactive and simulated markets
- Competitions with forward scoring
- Crypto and on-chain agent benchmarks
- Lessons for machine-trading

## Backtest benchmarks (LLM era)

| Benchmark | Agent does | Lesson |
| --- | --- | --- |
| [StockBench](https://arxiv.org/abs/2510.02209) (2025) | Daily buy/sell/hold on DJIA stocks over months, with point-in-time prices, fundamentals and news | Most models fail to beat buy-and-hold. No transaction costs or slippage, by its own admission. |
| [InvestorBench](https://arxiv.org/abs/2412.18174) (ACL 2025) | Stock, crypto and ETF decisions with the FinMem framework; 13 LLM backbones | Historical windows overlap training data. |
| [FinMem](https://arxiv.org/abs/2311.13743) | A single agent with layered memory, scored on cumulative return and Sharpe | Agent architecture is part of what's measured. |
| [TradingAgents](https://arxiv.org/abs/2412.20138) | A multi-agent "trading firm" (analysts, researchers, risk) | Backtested; vulnerable to contamination (below). |
| [CryptoTrade](https://arxiv.org/abs/2407.09546) (EMNLP 2024) | Reflective daily BTC/ETH/SOL trading from on-chain data and news | Daily granularity; no execution realism. |
| [FinBen / PIXIU](https://arxiv.org/abs/2402.12659) | A broad financial NLP suite with a FinTrade component (10 stocks, 1 year) | Trading is a small slice of a QA-centric suite. |

## Contamination studies

These are the most important papers for anyone designing trading evals.

- [Profit Mirage / FinLake-Bench](https://arxiv.org/abs/2510.07920) (2025): backtested LLM returns "evaporate" outside the model's knowledge window. TradingAgents shows a 55.68% Sharpe decay.
- [Look-Ahead-Bench](https://arxiv.org/abs/2601.13770) (2026): measures alpha decay from in-sample to post-cutoff periods.

**Takeaway:** a model's weights are a look-ahead channel. Point-in-time data
filtering is not enough when the model has read about the period. Synthetic
or perturbed paths on a real chain snapshot avoid this.

## Live-trading benchmarks

| Benchmark | Setup | Lesson |
| --- | --- | --- |
| [LiveTradeBench](https://arxiv.org/abs/2511.03628) (2025) | 50 live days, US stocks and Polymarket, portfolio allocation | LMArena rank doesn't predict trading results. |
| [Agent Market Arena](https://arxiv.org/abs/2510.11695) (2025) | A lifelong live paper-trading leaderboard, crypto and stocks | Agent design matters more than the backbone model. |
| [AI-Trader](https://arxiv.org/abs/2512.10971) (2025) | Live and uncontaminated: Nasdaq-100, A-shares, crypto | Most agents show poor returns and weak risk control. |
| Alpha Arena, nof1.ai (Oct–Nov 2025) | Six LLMs each with $10k on Hyperliquid perps, real money | Spread from about +22% to about −63% *(secondary)*. The organizers called the ranking non-definitive. |

**Takeaway:** live benchmarks are contamination-free, but they are a single
path with a few dozen correlated bets and can't be reproduced. They make good
press and bad measurement. Offline replay of many generated paths is the
complement.

## Reactive and simulated markets

- [Agent Trading Arena](https://arxiv.org/abs/2502.17967) (EMNLP 2025 Findings): a zero-sum simulated market with matching, where agents' trades move prices.
- [ABIDES / ABIDES-Gym](https://arxiv.org/abs/2110.14771): a discrete-event, multi-agent limit order book simulator with latency.
- [JAX-LOB](https://arxiv.org/abs/2308.13289): GPU-parallel limit order books for RL at scale.
- [mbt_gym](https://arxiv.org/abs/2209.07823): model-based market-making and execution gyms (Avellaneda–Stoikov style).
- [FinRL-Meta](https://arxiv.org/abs/2211.03107) (NeurIPS 2022 D&B): hundreds of gym environments. It names low signal-to-noise, survivorship bias and backtest overfitting as the core obstacles.

**Takeaway:** static replay can't capture the bot's own impact or reactive
counterparties. When the thesis depends on them (market making, large
execution), add reactive agents, or label results "static replay".

## Competitions with forward scoring

- [Optiver "Trading at the Close"](https://www.kaggle.com/competitions/optiver-trading-at-the-close) (Kaggle): scored on market data collected after submission *(secondary)*.
- [Jane Street](https://janestreet.com/machine-learning) competitions: heavy-tailed, non-stationary data; utility- and R²-style metrics *(secondary)*.
- [Numerai](https://docs.numer.ai): ~20-day live rounds scored on correlation and meta-model contribution, with stakes at risk.

**Takeaway:** quant competitions solved leakage by scoring on the future.
For agent tasks, the equivalent is hidden, freshly generated scenarios.

## Crypto and on-chain agent benchmarks

- [Solana Bench](https://solana.com/news/solana-bench) (Solana Foundation, 2025): scores the number of unique instructions an agent executes successfully, in Basic and Swap environments (Jupiter, Orca, Raydium, Phoenix, Meteora), tested on Surfpool. It measures competence, not PnL.
- EVM fork-based security benchmarks (exploit, detect and patch tasks on pinned mainnet forks), such as [CyberChainBench](https://arxiv.org/abs/2601.06565). Their lesson is to pin the fork block, or assertions drift.

**Gap:** we found no Solana-native benchmark that scores trading PnL on
real program logic with hidden, generated market paths. That is
machine-trading's niche.

## Lessons for machine-trading

1. Keep scoring offline on generated paths: reproducible, uncontaminated, unlimited.
2. Report baselines next to scores (nop, buy-and-hold, reference). Beating nothing is common.
3. Model costs through the real programs. Most backtest benchmarks skip them.
4. Run k trials and report consistency. Single runs mislead (see quant-pitfalls.md).
5. State the realism limits in each task README.
6. Gate every scenario change on a skill ladder and report headroom against ceilings (see ladder-and-ceilings.md).
