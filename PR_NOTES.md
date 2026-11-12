# PR 14: mtx gates + CI image

**Title:** Add `mtx gates`, a prebuilt CI image and fast gates on pull requests

One command folds the task checks into one report under the quality bar.

- `mtx gates <task> [--fast]` writes `gates.json` and `gates.md`: task files (`task status`), solvable and fair (reference, nop, ladder order, informed >= 2x best non-reading), graded correctly (exploit bots, clean runs; full: cheaters, leak audit, keypair probe, clock probe), realistic.
- Replay determinism and market calibration call `mtx replay check <task>` / `mtx market-cal check <task>` if the build has them, else report `not run`.
- `mtx task status` reads the component layout (lib.rs); `task new` copies every strategy source.
- `ladder.py` writes `summary.json`; `just ladder` takes count/out/min_odds; `just harbor-checks` takes a jobs dir.
- CI: `bench/ci/Dockerfile` + `ci-image.yml`; rust and site jobs run in it; a `gates` job runs `--fast` on changed tasks and uploads reports when dispatched.

## Design choices
- The task declares informed/non-reading rungs in its ladder file (`gates` block), next to the rungs it names; the gate sums PnL over all sets, baseline floored at 0.
- Fast mode: 1 set x 12 sessions; bootstrap floor 0.5 (order must still hold on the set).
- Replay/market-cal subcommand names (`replay check`, `market-cal check`) are guesses to be matched when PRs 12/10 land; not-run until then.
- CI image uses `:latest`. The CI gates job and image workflow are untested on GitHub (no pushes); the in-container bot-user switching is unverified.
- Full mode needs Docker + Harbor, not run here.
