# Bot contract v0

Status: draft

## Abstract

A bot is a program that trades one session of a task: a scripted market,
replayed on an offline copy of Solana mainnet. This specification states
what a grader gives a bot, what it expects back, and what it forbids. A bot
that keeps to it runs unchanged on a developer's machine (`mtx bot practice`),
in the benchmark's verifier, and in any grader that implements it. What a
bot may trade, and the limits it must stay within, is its
[mandate](mandate.md); this contract is how it runs.

The reference implementation is `mtx grade`, in `mtx-core`
(`crates/core/src/session.rs`, `guard.rs`), and `mtx-botkit`
(`crates/botkit`), the bots' Rust SDK.

## Conventions

The key words MUST, MUST NOT, REQUIRED, SHOULD, SHOULD NOT, MAY and OPTIONAL
are to be interpreted as described in BCP 14 (RFC 2119, RFC 8174) when, and
only when, they appear in all capitals.

## The bot

A bot is a directory. Its entry point is `run.sh`: the grader runs `bash
run.sh`, with the bot directory as the working directory.

- `run.sh` MUST start the bot without building or downloading anything:
  the grader has no network, and builds nothing.
- The bot MUST run on the grader's system with nothing but what's in its
  directory. The benchmark's is Debian bookworm (slim): libc, bash and
  coreutils, with no compiler, interpreter or SDK. A native executable,
  such as the `bin/bot` a starter's `build.sh` produces, meets this.
- A grader MAY leave out a `target/` directory, and SHOULD say whether it
  does.

## Environment

The grader clears the environment, then sets exactly these variables:

| Variable | Meaning |
| --- | --- |
| `MTX_RPC_URL` | The chain's HTTP JSON-RPC endpoint. See [The chain](#the-chain). |
| `MTX_KEYPAIR` | A Solana CLI keypair file (a JSON array of 64 bytes): the trading authority. |
| `MTX_EXCHANGE` | A file with the venue's market metadata, matching the chain, such as Phoenix's `/exchange`. |
| `MTX_SYMBOL` | The market to trade, such as `SPCX`. |
| `MTX_END_SLOT` | The slot at which the session ends and the account is valued. |
| `MTX_MAX_POSITION` | The mandate's position cap, in the instrument's base units. |
| `MTX_MAX_DRAWDOWN` | The mandate's drawdown limit, in USD. |
| `MTX_MANDATE` | A JSON file with the session's [mandate](mandate.md), its term set to the session's last slot. It holds everything the three variables above say, in one document. |
| `MTX_TURN_FD` | The file descriptor of the turn socket: `3`. See [Turns](#turns). |
| `HOME` | A private, writable directory for this session. |
| `PATH` | `/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin`. |
| `RUST_LOG` | The log level the grader asks for, `info` by default. |

The task says what the account holds when the session starts, such as the
authority's SOL for fees and its collateral on the venue. `mtx_botkit::BotEnv`
reads the variables, and `BotEnv::mandate` the mandate.

## Turns

The world moves in **steps**, of a few slots each, and waits for the bot
between them. The protocol is `mtx-turn-1`: JSON lines, one message a line,
over a Unix socket on the file descriptor `MTX_TURN_FD` names. Every message
is an object whose member `t` names it.

1. The grader sends `hello`, with the session's clock:

   | Member | Meaning |
   | --- | --- |
   | `protocol` | `"mtx-turn-1"`. A bot MUST refuse another. |
   | `steps` | The number of steps in the session. |
   | `slots_per_step` | The slots each step moves the world. |
   | `start_slot` | The slot the first step starts from. |
   | `end_slot` | The slot the session ends at. |
   | `turn_wall_ms` | The most one turn may take, in milliseconds. |

   The bot answers `{"t":"ready"}`.
2. For each step, the grader moves the world: it produces the step's blocks,
   where the market's moves and other traders' orders land. Then it sends
   `{"t":"step","k":…,"slot":…,"last":…}`: the chain now holds the state
   after step `k`, at `slot`, and `last` is true for the session's last step.
   The bot reads the chain, sends its transactions, and answers
   `{"t":"done","k":…}`. Nothing on the chain moves until it does.
3. After the last step, the grader sends `{"t":"end"}` and stops the bot.

Transactions the bot sends during its turn land before the next step. The
grader bounds the bot's time:

- **Hello.** A bot that doesn't answer `ready` within 10 seconds isn't
  waited for: the world moves at the slot time, 250 ms a slot, whatever the
  bot does. It's graded the same way.
- **One turn.** At most `turn_wall_ms`: 5,000 ms in the reference grader.
- **All turns.** At most 500 ms a step, summed over the session.

Past either limit, the world moves on without the bot. A transaction sent
while the world moves is refused, which is not a breach: it just doesn't
make it into the turn.

`mtx_botkit::Turns` implements the bot's side.

## The chain

`MTX_RPC_URL` serves standard Solana JSON-RPC over HTTP. There is no
websocket endpoint: a bot reads what it needs each turn.

- A transaction is final once it executes: there are no forks, and the next
  block comes only after the bot's turn. The endpoint serves reads at
  `processed`, and reports an executed transaction as `finalized`, so
  clients that wait for confirmation return within the turn.
- A bot MAY call only these methods: `getAccountInfo`, `getBalance`,
  `getBlock`, `getBlockCommitment`, `getBlockHeight`, `getBlockProduction`,
  `getBlockTime`, `getBlocks`, `getBlocksWithLimit`, `getClusterNodes`,
  `getEpochInfo`, `getEpochSchedule`, `getFeeForMessage`,
  `getFirstAvailableBlock`, `getGenesisHash`, `getHealth`,
  `getHighestSnapshotSlot`, `getIdentity`, `getInflationGovernor`,
  `getInflationRate`, `getInflationReward`, `getLargestAccounts`,
  `getLatestBlockhash`, `getLeaderSchedule`, `getMaxRetransmitSlot`,
  `getMaxShredInsertSlot`, `getMinimumBalanceForRentExemption`,
  `getMultipleAccounts`, `getProgramAccounts`,
  `getRecentPerformanceSamples`, `getRecentPrioritizationFees`,
  `getSignatureStatuses`, `getSignaturesForAddress`, `getSlot`,
  `getSlotLeader`, `getSlotLeaders`, `getStakeMinimumDelegation`,
  `getSupply`, `getTokenAccountBalance`, `getTokenAccountsByDelegate`,
  `getTokenAccountsByOwner`, `getTokenLargestAccounts`, `getTokenSupply`,
  `getTransaction`, `getTransactionCount`, `getVersion`, `getVoteAccounts`,
  `isBlockhashValid`, `minimumLedgerSlot`, `sendTransaction`,
  `simulateTransaction`.
- The grader refuses any other method, including the sandbox's `surfnet_*`
  cheatcodes and `requestAirdrop`, and records the call.

## The sandbox

- The bot has no network but the RPC endpoint.
- When the grader runs as root, the bot runs as an unprivileged user, one
  per session that runs at once.
- The bot runs in its own process group. When the session ends, the grader
  sends the group `SIGTERM`, and `SIGKILL` 2 seconds later. A bot MAY exit
  earlier.
- The bot's standard output and error go to the session's log.

## Grading

A grader MUST disqualify a session, whatever its PnL, when the bot:

1. called a method outside the list in [The chain](#the-chain);
2. broke its [mandate](mandate.md#rules), as the grader audits it after
   every step;
3. ended the session with its account liquidatable;
4. wrote a file the grader uses to report its grade.

What a session scores, and what a disqualified session does to the grade,
is the task's: in the benchmark, one disqualified session scores the whole
grade 0. Each task's README states its score, its account, its mandate and
its scenario family.

## The benchmark's grader

The benchmark's verifier implements this contract with these specifics:

- It receives the bot directory without its `target/`, read-only.
- It runs the bot on Debian bookworm (slim), with no network.
- It grades on a hidden set of scenarios drawn for each batch from a secret
  no file holds, and runs a reference bot on the same sessions.

`mtx bot practice` runs the same grader on a task's practice scenarios.
