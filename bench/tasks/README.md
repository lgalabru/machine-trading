# solana-foundation/open-trading

Trading tasks for agents on Solana. In each task the agent writes a bot,
and the bot is graded on market scenarios replayed on an offline mainnet
snapshot restored with [Surfpool](https://github.com/solana-foundation/surfpool).

The verifier runs with no network and receives only the bot. Every cheatcode
is locked before the bot starts, and the bot reaches the chain through a
proxy that forwards standard Solana RPC methods only. Rewards are the bot's
total PnL over the hidden set, normalized against a reference bot's.

| Task | Market | Scenario |
| --- | --- | --- |
| `phoenix-flash-crash` | Phoenix SOL perpetuals | Flash crashes of 5–12%: most rebound, some stall, some keep sliding |

The hidden scenarios ship inside each task's `tests/`. On this public
version, treat scores as a development signal, not a held-out evaluation.
