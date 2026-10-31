# Market realism

Profit only means skill if the market charges what a real one charges.
Every missing cost makes some strategy look better than it is, and agents
will find that strategy.

## Contents

- Costs and execution
- Liquidity and impact
- Static replay vs reactive markets
- Crypto and DeFi specifics
- Declaring the limits

## Costs and execution

- **Run trades through the real programs.** On a Surfpool snapshot, Phoenix, Jupiter or any AMM charges its real fees, rounding and lot sizes. This is the main advantage over CSV backtests: StockBench, for example, [omits costs entirely](https://arxiv.org/abs/2510.02209).
- **Include compute and priority fees.** Bots that spam transactions should pay for it.
- **Latency.** A bot that polls every 500 ms sees stale marks. Keep the slot time fixed (250 ms here). If the thesis is latency-sensitive, model a submission delay rather than letting transactions land in the slot they were built.

## Liquidity and impact

- **Use real depth.** Snapshot order books keep mainnet's real maker depth. Size the account so a reasonable bot walks the book a little. That makes sizing a real decision.
- **Make impact size-dependent.** [Almgren–Chriss](https://ideas.repec.org/a/rsk/journ4/2161150.html) separates temporary from permanent impact. At minimum, include one scenario where an oversized market order clearly loses.
- **Don't refill the book for free.** If scenario overrides re-cross or refill the book every step, a taker strategy gets unlimited liquidity at the moved price. Know what your market-move primitive does to depth.

## Static replay vs reactive markets

Static replay moves the market along a path. Nobody reacts to the bot.
That's fine for directional theses (crash buying), but it **overstates**:

- market making, since there's no adverse selection from informed flow
- large execution, since nobody front-runs it
- any strategy that profits from its own price impact

When the thesis depends on these, add reactive agents, such as a competing
taker or a sandwicher (see [Agent Trading Arena](https://arxiv.org/abs/2502.17967)
and [ABIDES](https://arxiv.org/abs/2110.14771)). Otherwise label the task
"static replay".

Reactive agents worth having:

- **A competing maker** that undercuts a better quote by a tick down to a floor and eases back out (machine-trading 0.8.0). Without it, a bot one tick inside static liquidity collects the spread unopposed.
- **A learning predator** that tracks the bot's pace per venue, trades ahead of it at a multiple of that pace (more when the pace is steady), buys back later, and stands down when the bot stops feeding it. It punishes readable flow, which is the skill an execution task should test, and it's visible only on the tape.
- **Makers that skew against absorbed inventory, books that refill at a finite rate, and permanent impact** from the bot's own flow.
- **Network faults**: expired blockhashes, dropped sends, late confirmations, 429s, a priority-fee floor under congestion. At fixed points they keep evaluation runs comparable, but a bot can learn them: draw them per session for training.

## Crypto and DeFi specifics

- **MEV.** On Solana, sandwiching was widespread enough that Jito shut down its public mempool ([Helius MEV report](https://www.helius.dev/blog/solana-mev-report)). A sandbox without searchers overstates naive large swaps. Either add a sandwicher, or cap the swap size the task rewards.
- **Oracles.**
  - Perps value positions at an oracle or mark price. Know its band and update rules; for example, Phoenix rejects moves beyond 5% per update, so scenario steps must be ≤4%.
  - Thin markets that feed oracles can be pushed. The [Mango Markets incident](https://www.certik.com/blog/mango-market) (~$116M, Oct 2022) is the canonical example. Decide whether moving the oracle is in bounds.
- **Funding.** Perp funding accrues over time (Hyperliquid pays hourly at 1/8 of the 8h rate, [docs](https://hyperliquid.gitbook.io/hyperliquid-docs/trading/funding)). Short scenarios can ignore it. Carry or basis tasks must simulate it across slots.
- **Liquidations.** If the bot can use leverage, replay the protocol's real liquidation path. In machine-trading, ending the session liquidatable scores 0 because otherwise ruin can't be valued, but mid-session liquidations should happen on-chain as they would on mainnet.
- **Clock.** Programs read `Clock` (slot and unix time). Snapshot restore must keep both plausible, or time-dependent logic (oracle staleness, funding) misbehaves. See the clock gap in the repo README.

## Declaring the limits

Every task README should list what the simulation doesn't model. For example:

> Static replay: makers don't react to the bot, there is no MEV, funding is
> not simulated (sessions are under two minutes), and the clock is near real
> time rather than the fork's timestamp.

This keeps scores honest, and it's the backlog for the next task.
