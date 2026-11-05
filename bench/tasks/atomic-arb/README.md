# Continuous AMM trading: `atomic-arb`

An agent builds one native Solana bot, then the same artifact trades three
private fixed flows on 128 real AMM pools. This task is packaged locally; it is
not part of the published Phoenix dataset.

## The skill

A good bot profits because it recognizes executable opportunities, sizes trades
against actual liquidity, and manages execution and inventory as other traders
change the market. The agent designs its own development scenarios using real
swap transactions in the resettable public playground.

## The market

The same synthetic initial market contains 24 tokens, 64 Raydium CPMM pools and
64 Orca Whirlpools. Each of three independent sessions lasts 2,000 clock-driven
slots at 200 ms. The candidate and three fixed reference searchers each receive
95 WSOL, 5 SOL, one CPU and 512 MiB. Each session restores market state, capital,
program/scratch state and competitor processes in fresh isolated containers.

Surfpool processes signed transactions in arrival order. There is no atomic
benchmark guard, inventory restriction or fee auction. The proxy exposes native
processed RPC and WebSocket observations, with equal limits for all participants.
Only executed state is visible; future orders and pending rival transactions
are private. User swaps are fixed at generation time and never re-routed during
evaluation. Included user failures remain part of the recorded outcome.

## Scoring

Each scenario reports signed net native-capital change in SOL, with actual fees
already reflected in balances. Harness-funded rent deposits are neutralized at
both endpoints; non-native token inventory is credited at zero. The bot may hold
inventory, but must realize value in the tracked native accounts to earn credit.
There is no agreed aggregate score: the combined report contains three separate
profits, reference results and diagnostics. No averaging, best-of selection or
normalization is applied. Infrastructure failures leave the set incomplete;
artifact disqualification is reported separately.

## Baselines

The three fixed competitors search cycles of up to two, four and five swaps.
They retain their existing fee policies for continuity. Priority fees are charged
but do not improve inclusion under this execution policy. This makes their fee
choices deliberately visible in the gross/net comparison. A no-op must earn
exactly zero, and a maintainer smoke bot must profit on every scenario before a
release is frozen. Acceptance includes exact ledger replay, native RPC/clock and
fee tests, all-pool swaps, and container isolation checks.

## Realism limits

These are synthetic markets with fixed private user demand, not a claim about
future mainnet profitability. Local concentrated-liquidity approximations used
when generating demand are not full tick-crossing quotes; actual program
execution determines fills and failures. Clock-driven live races can vary with
scheduling, even though their input fixture set is fixed. Compare only the same
hardware profile. Three scenarios provide broader coverage, not statistical
confidence in a universal ranking.

## Run and inspect

After a maintainer freezes a local release:

```sh
mtx evaluate --task atomic-arb \
  --agent codex --model openai/gpt-6.1-terra --effort high \
  --jobs-dir work/amm-results --job-name terra-three-scenarios-001
python3 -m json.tool work/amm-results/terra-three-scenarios-001/report.json
```

The task manifest selects the default local release and pins its three-scenario
fixture set by hash. Use `--release PATH` to select another validated release.

Use `target/debug/mtx` if the new CLI has not been installed. Docker must have
12 CPUs and approximately 8 GiB available. The model develops once; subsequent
Harbor copy-only stages reuse its artifact without invoking a model or building
again. Candidate code executes only in isolated searcher containers.

The terminal shows Harbor setup and verifier progress, the active scenario,
and recent Codex tool activity during development. To watch an existing job
from a second terminal, run:

```sh
mtx evaluate --task atomic-arb --jobs-dir work/amm-results \
  --job-name terra-three-scenarios-001 --follow
```

Ctrl-C disconnects this observer. The command that launched the job prints
per-scenario results and the combined report path. The terminal summary and
finished job report key each score by the release's scenario name. Each
individual report still has its Harbor trial ID. Harbor's agent column describes
who populated the trial workspace: Codex builds the bot once, then the
`oracle` copy step installs that same artifact for the later trials. The
verifiers record its hash in every scenario. Its
`scenarios` entries link to full verifier reports; `harbor/` retains all trials.
A raw `harbor run` against one packaged subtask evaluates only that one session;
use `mtx evaluate` to run the complete set. Existing Phoenix review UI integration
is outside this change.

## More

[Bot contract](instruction.md) · [Maintainer setup](../../backrun/README.md) ·
[Private-flow design and boundaries](../../backrun/DESIGN.md)

For an infrastructure-interrupted set, explicitly rerun the same `mtx evaluate`
command with `--resume --retry-infrastructure`. Completed scored sessions are
reused, never repeated. The submitted artifact stays identical; only sessions
with a recorded infrastructure failure and no score may be retried. All attempts
remain in the job directory. Disqualified artifacts cannot be retried this way.
