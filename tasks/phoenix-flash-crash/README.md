# solana-foundation/phoenix-flash-crash

The agent writes a bot that trades Phoenix SOL perpetuals through a flash
crash and rebound. The market is an offline copy of Solana mainnet:
Phoenix's SOL market, snapshotted on 2026-10-27 and restored into a
[Surfpool](https://github.com/solana-foundation/surfpool) surfnet.

- **Agent environment:** a Rust toolchain, the `ott-botkit` SDK, a starter
  bot that builds and never trades, and `ott-practice`, which grades the bot
  on practice scenarios the same way the verifier does.
- **Verifier:** receives only `/app/bot`. With no network, it replays three
  hidden crash scenarios, each on a fresh surfnet. The bot runs as an
  unprivileged user behind an RPC proxy that forwards standard Solana
  methods only.
- **Reward:** the mean over scenarios of the bot's PnL divided by a
  reference bot's PnL, clamped to [0, 1]. A session scores 0 if the bot
  makes a refused RPC call or ends the session liquidatable.

Expected results: the oracle scores 1.0, `nop` scores 0.0, and a bot that
calls surfnet cheatcodes scores 0.0.

The hidden scenarios ship in `tests/scenarios` with the task, so treat
scores on this public version as a development benchmark, not a held-out
evaluation.
