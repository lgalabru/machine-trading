# Anti-cheat for bot-writing trading tasks

Assume the agent will find every shortcut. This is documented behavior, not
paranoia:

- METR found frontier models wrapping `time.time` to fake speedups, stubbing evaluators, reading the scorer's reference answer from the call stack, and overriding `__eq__`. Telling them not to cheat barely helped ([METR, 2025](https://metr.org/blog/2025-06-05-recent-reward-hacking)).
- Terminal-Bench saw submissions that shipped the tests folder, embedded encrypted solutions, edited timeouts, and downloaded solutions ([TB integrity update](https://www.tbench.ai/news/leaderboard-integrity-update)).

METR's recommendation applies here: **fix the scorer, don't scold the model.**

## Contents

- Cheat surface table
- The three-layer defense used in open-trading
- Building the cheater check
- Known open gaps

## Cheat surface

| Path | Example | Mitigation |
| --- | --- | --- |
| Chain cheatcodes | `surfnet_setTokenAccount` to mint USDC, `surfnet_setAccount` to rewrite positions | RPC guard **allowlist** (`crates/core/src/guard.rs`); any refused call scores 0 |
| Faucets | `requestAirdrop` | Not on the allowlist |
| Time travel | Clock or slot warps to jump past the crash, or to reach the rebound early | Time cheatcodes refused; the grader drives time |
| Price manipulation | Pushing the oracle's source market, or self-trading to move the mark used for valuation | Value at the mark the grader reads; decide whether moving the price counts as skill or as a violation, and say so |
| Reading hidden scenarios | `cat /tests/scenarios/*.json` | `chmod go-rwx /tests`; the bot runs as `ott-bot`; scenarios are absent from the agent image |
| Reading the reference | The solution in the image | The solution is used only by the oracle agent, never copied into images |
| Writing the reward | Writing `/logs/verifier/reward.json` | The bot user can't write `/logs` or `/tests` |
| Self-reported PnL | Logging "pnl: 1e9" | Value from on-chain state only |
| Killing or stalling the grader | Fork bombs, `kill -9` on the surfnet, ignoring SIGTERM | Separate user, its own process group, SIGKILL after the grace period, `pkill -u ott-bot` |
| Network | Fetching future prices or a solution | Verifier `network_mode = "no-network"` |
| Bypassing the guard | Port-scanning localhost to find surfnet's raw RPC | **Open gap**: run the bot in a sidecar that can reach only the guard |
| Environment leakage | Reading the grader's env vars | Bot env cleared to the contract variables only |
| Artifact smuggling | Leftover files from the agent image merging into the verifier | `rm -rf /app/bot` before Harbor copies the artifact in |
| Exploiting protocol bugs | A Mango-style thin-market pump; liquidation edge cases | Decide the policy up front; if it's a violation, detect it (e.g. the bot's share of volume, oracle deviation) |

## The three layers in open-trading

1. **Interface:** the bot talks to an RPC guard that forwards a fixed allowlist of standard Solana methods. Anything else, even inside a batch, is refused and recorded.
2. **Process:** the bot runs as an unprivileged user, in its own process group, with a scrubbed environment. It can't read `/tests` or write `/logs`.
3. **Container:** the verifier is a separate container with no network. It receives only `/app/bot` minus `target/`.

Each layer assumes the one above it fails.

## Building the cheater check

Write one cheat bot per closed path in `bench/checks/` and run each through Harbor
(`just harbor-checks`). Each must score 0, and the report should name the
refused calls. Minimum set:

- Calls `surfnet_*`, `requestAirdrop` and any time-travel method → disqualified.
- Reads `/tests/scenarios` → permission denied, scores as nop.
- Writes `/logs/verifier/reward.json` → permission denied.
- Sleeps past SIGTERM → killed, valued normally.
- (Once the sidecar exists) scans localhost ports → finds nothing but the guard.

When you add a mitigation, add the cheat that proves it works. A mitigation
without a failing cheat test is a hope.

## After real-agent runs

Run `harbor analyze jobs/<job>` on every batch. Its `reward_hacking`
criterion reads trajectories and flags suspicious success. Read the
transcripts of every top-scoring trial yourself. Terminal-Bench now has an
agent judge review every passing trial.
