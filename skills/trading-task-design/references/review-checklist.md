# Pre-publish review checklist

Run through this before `harbor publish`. Each item names the failure it
prevents.

## Thesis and family

- [ ] One-sentence skill thesis written in the task README. *Prevents tasks that measure noise.*
- [ ] Family ranges documented in `instruction.md`; seeds of hidden scenarios not recorded anywhere public. *Prevents ambiguity and leakage at once.*
- [ ] Family includes adversarial draws (non-recovery, fake-outs). *Prevents one-trick bots scoring 1.*
- [ ] Every waypoint respects protocol limits (e.g. Phoenix ≤4% per step). *Prevents testing the simulator instead of the bot.*
- [ ] Path shape leaks nothing: uneven steps and gaps, a random tail, noise between moves, independent legs, overlapping ranges for phases that should look alike. *Prevents bots that trade the generator instead of the market.*
- [ ] Design record written ([design-record.md](design-record.md)): each regime flips the best response, each lever has an honest way to detect it, and every mechanic is present in every session (no tells). *Prevents regimes that are noise, hidden facts, and setup-time labels.*
- [ ] Read the generator as an adversary: write down every rule an agent could deduce from the instruction, the practice files and `mth scenario generate`, and check that none beats the reference on its own. *Prevents structural leaks that no cheat check catches.*

## Instruction as contract

Harbor's own `harbor check` rubric enforces the first four; the rest are
trading-specific.

- [ ] Everything the grader checks is in `instruction.md`: env vars, allowlist, refusal penalty, valuation formula, stop signal and grace period, liquidation rule, "the grader does not build".
- [ ] Everything `instruction.md` claims is true of the image: paths, tools, timings, versions.
- [ ] Deliverable path is absolute and explicit (`/app/bot/run.sh`).
- [ ] No typos in paths, env vars, or commands.
- [ ] Scoring formula stated exactly, including clamping and aggregation.
- [ ] Realism limits stated (static replay, no MEV, no funding…).

## Scoring

- [ ] Reference bot calibrated on every scored scenario (`just calibrate`); none non-positive.
- [ ] Valuation reads on-chain state only.
- [ ] Position size is capped in the contract and enforced by the grader, and the reference trades up to the cap. *Prevents a reward that measures leverage instead of skill.*
- [ ] Raw PnL and per-scenario scores reported alongside the reward.
- [ ] Ladder order rule holds on every fresh hidden set, and the reference is positive in every regime ([ladder-and-ceilings.md](ladder-and-ceilings.md)). *Prevents a task that ranks by luck.*
- [ ] Each mechanic the task claims to reward has a control pair showing it pays. *Prevents levers that only add variance.*
- [ ] Penalty twins run for each hard rule: one bot just inside it, one just outside. *Prevents rules that zero the wrong side.*
- [ ] Hidden set large enough: 10+ for evals (see quant-pitfalls.md).

## Isolation and anti-cheat

- [ ] Hidden scenarios only in `tests/`, not in the agent image (`tests_or_solution_in_image`).
- [ ] Solution only in `solution/`, not in any image.
- [ ] Verifier has no network; receives only the bot artifact.
- [ ] Bot runs unprivileged; can't read `/tests` or write `/logs`.
- [ ] RPC access is an allowlist; refused calls recorded and disqualifying.
- [ ] One cheat bot per closed path in `bench/checks/`, each scoring 0.

## Reproducibility

- [ ] Snapshot pinned to a slot; manifest committed (`snapshots/<name>/manifest.json`).
- [ ] Image pinned by digest in both Dockerfiles (`just pin-image`); `harbor lint --select ALL` clean.
- [ ] Re-running the oracle gives the same PnL, or the README says why it can't (a grader-run counterparty that reacts in real time) and by how much it moves.
- [ ] SDK and bot dependencies pinned (lockfile shipped in the image).

## Checks

- [ ] `just harbor-checks`: oracle at its baseline (`reference_ratio` ≈ 1), nop = 0, cheater = 0.
- [ ] `harbor check bench/tasks/<name>` passes the rubric (or each FAIL is understood).
- [ ] Real agents run with `-k 3` or more; best-model mean roughly 0.2–0.7, reported with bootstrap intervals (sessions and runs) and the time limit.
- [ ] Infrastructure failures voided and re-run, not scored 0.
- [ ] `harbor analyze` run on those jobs; top trials' transcripts read by a human.
- [ ] Winning bots' source read; any edge the bot explains by the generator's mechanics is a leak to fix, not a result. Risk profiles that are too clean (worst session near 0, 90%+ sessions profitable) checked the same way.

## Publishing

- [ ] Task README states what is public (are hidden scenarios really hidden in this version?).
- [ ] Dataset digests refreshed (`harbor sync bench/tasks`) after any task change.
- [ ] Image public on GHCR so others can build.
