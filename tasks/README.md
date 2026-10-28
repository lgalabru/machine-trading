# solana-foundation/open-trading

Trading tasks for agents on Solana. In each task the agent writes a bot,
and the bot is graded on market scenarios replayed on an offline mainnet
snapshot restored with [Surfpool](https://github.com/solana-foundation/surfpool).

The verifier runs with no network and receives only the bot. It forwards
standard Solana RPC methods only, so a bot can't call cheatcodes. Rewards
are PnL normalized against a reference bot.

| Task | Market | Scenario |
| --- | --- | --- |
| `phoenix-flash-crash` | Phoenix SOL perpetuals | Flash crash of 5–12%, then a partial or full rebound |

The hidden scenarios ship inside each task's `tests/`. On this public
version, treat scores as a development signal, not a held-out evaluation.
