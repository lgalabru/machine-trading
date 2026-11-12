# PR 7: snapshot recipes + simulated markets (branch infra/07-snapshot-recipes)

## STATE (checkpoint 2026-11-12 ~03:40 UTC)
Done and committed (3 commits on top of c80ca119):
- crates/grader/src/recipe.rs, simulated.rs; snapshot.rs Manifest extended; `mtx snapshot build`; recipes for both snapshots; docs/snapshot-recipes.md; crates/grader/tests/recipe.rs (4 tests pass on the box).
- Live rebuilds of both snapshots from the public RPC done on runner-14: /data/builders/infra-07/rebuild/{sol-usdc-spot-2026-11-11,phoenix-spcx-2026-10-29}.

Box: runner-14, /data/builders/infra-07 (src synced by ~/scratchpad/infra07/i7run.sh, container script /data/builders/infra-07-dk.sh, target /data/builders/infra-07/target; files there are root-owned, clean with `docker run --rm -v /data/builders:/t alpine rm -rf /t/infra-07`). No container is running now.

Next:
1. Compare rebuilt vs committed snapshots (address sets, program bytes) and grade a reference on the rebuilt spot snapshot.
2. nightly fmt, clippy (workspace, mtx-kit --all-features, wasm32-wasip2), cargo test --workspace.
3. Identity gate: 4 tasks, 6 sessions, reference bots, base (origin/main) vs head, private seed generated on the box.
4. Fill the final report/PR description below; push infra/07-snapshot-recipes only after proof passes.
