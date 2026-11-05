# Continuous AMM trading benchmark

Version 0.5 evaluates one submitted artifact against three private fixed flows.
The original frozen 0.4 release and Terra result remain unchanged. See the
[design record](DESIGN.md) for the private regimes and the
[task contract](../tasks/atomic-arb/instruction.md) for what an agent sees.

## Build and validate a release

The workspace uses LimeChain main's newer Surfpool dependency. Cargo.lock fixes
the resolved commit; `runtime_pin` checks that the benchmark's recorded revision
matches it. Build with `--locked`. Each searcher uses one CPU and 512 MiB, while
the grader has separate resources. Docker needs 12 CPUs and about 8 GiB.

Build the real AMM and optional executor programs once with
`bench/backrun/build-programs.sh OUTPUT`. Retain the original setup keys privately.
To extend the existing fixture, use its existing private directory as `SOURCE`.
The validator migrates execution metadata without changing its market, flow,
snapshot, wallet or setup-provenance bytes. It never alters the source release.

Choose one fixed calibration artifact before validation. It should use only
public observations and must profit after fees on all three scenarios. An
existing frozen agent artifact can serve this purpose; no new model call is
required. The basic reference bot can lose money, especially because it pays
priority fees under arrival-order execution and permits stale fills.

```sh
bench/backrun/validate.sh \
  /absolute/path/to/original-private-fixture \
  /absolute/path/to/programs \
  /absolute/path/to/cpmm-admin.json \
  /absolute/path/to/orca-admin.json \
  /absolute/path/to/fixed-calibration-artifact \
  work/scenario-set-validation
```

The pipeline builds tooling, generates the new private set once, runs workspace
and real-program checks, builds local images, verifies isolation and the public
image boundary, and runs the fixed artifact and a no-op once per scenario. It
then replays each recorded trading trace exactly and checks actual frenzy
buy/sell reserve deltas. Only a complete passing suite creates `release.json`.
Failed runs stay on disk. Do not select the best timing repeat or redraw flows
until an artifact happens to profit. Nothing is pushed or published.

The constituent commands are `fixture_set.py build/verify`, `package_set.py`,
`evaluate_set.py --calibration --artifact ...`, and `freeze_set.py`; each has
`--help`. Private generation seeds are recorded in the private set only, never
needed during grading. `set.json` hashes all inputs and provenance. Public images
contain only the initial pool playground, program binaries and filtered SDK.

## Run Terra once, evaluate all three

```sh
target/debug/mtx evaluate --task atomic-arb \
  --agent codex --model openai/gpt-6.1-terra --effort high \
  --jobs-dir work/amm-results --job-name terra-three-scenarios-001
python3 -m json.tool work/amm-results/terra-three-scenarios-001/report.json
```

The task manifest selects the default local release and pins its three-scenario
fixture set by hash. Use `--release PATH` to select another validated release.

`mtx evaluate` is the shared entry point. It uses one model development attempt,
then fresh isolated Harbor sessions for the three scenarios. The two later
Harbor oracle stages only copy the original compiled artifact; they invoke no
model and do not execute candidate code in the trusted grader. Artifact and
runtime hashes and hardware fingerprints must agree across all scenarios.

The combined report contains `reward_by_scenario`, per-scenario candidate and
reference results, and paths to detailed verifier evidence. Signed losses are
preserved. There is no aggregate score. Infrastructure failure leaves a partial
report marked incomplete; a contract violation is marked disqualified. There
are no automatic retries or best-of repeats. Each private session uses 2,000
200-ms slots, about 6m40s; development has its own two-hour maximum budget.

Codex CLI is pinned to 0.166.0 with an empty configuration. Authentication comes
from `CODEX_AUTH_JSON_PATH`, defaulting to `~/.codex/auth.json`. Old single-fixture
frozen releases remain runnable through the same launcher. `mth-backrun` remains
an internal AMM execution/playground tool, not a top-level `mtx backrun` command.
The raw single-session Harbor subtask is an implementation detail; use the
set-aware launcher for a complete run. UI/live-view integration is deferred.

## Execution and later Surfpool work

Native Surfpool handles standard RPC, WebSocket subscriptions and arrival-order
transaction processing. Priority fees are paid but do not improve inclusion.
The bot owns its capital and can retain inventory; non-native inventory receives
zero terminal credit. There is no atomic guard. All reserves change through real
AMM swaps. A missed injection slot or coalesced clock tick is an infrastructure
failure, not a trading loss. No-op capital remains exactly unchanged.

Keep optional native transaction-scenario actions and priority-fee scheduling as
separate Surfpool proposals, described in [DESIGN.md](DESIGN.md). Neither is part
of this release. A future fee scheduler should use one bounded pending queue for
user injection and all bots, a pinned reward-to-cost policy, capacity/validity
checks, FIFO ties, and normal fee/rollback behavior. It must not reorder already
executed transactions, expose pending flow, or remove the fast transaction-driven
mode used for development. Such a change requires its own version and validation.

For an infrastructure-interrupted set, explicitly rerun the same `mtx evaluate`
command with `--resume --retry-infrastructure`. Completed scored sessions are
reused, never repeated. The submitted artifact stays identical; only sessions
with a recorded infrastructure failure and no score may be retried. All attempts
remain in the job directory. Disqualified artifacts cannot be retried this way.

The public task is named `atomic-arb`. Previously frozen releases retain their
historical `atomic-backrun` metadata and checksums and remain runnable with
`--task atomic-arb`; new packages use the new name.
