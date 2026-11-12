# STATE (checkpoint)
Branch infra/01-task-folders, 3 local commits on origin/main (c80ca119). Code done; fmt, clippy (workspace, kit all-features, wasm), cargo test --workspace all pass on runner-5. harbor lint passes on the 4 tasks.
Running: container `infra-01-ident` on northpier-runner-5 (/data/builders/infra-01; script ident.sh, log ident.log, outputs out/{base,head}/<gen>) compares session reports of origin/main's mtx (/data/builders/infra-01/base, target-base) vs this branch (src, target), 6 sessions per task, private seed in /data/builders/infra-01/seed (never print).
Next: read ident.log (expect IDENTICAL lines + IDENTDONE); then the "add a task" test (copy a task folder, `mtx task sync` via container, show git diff only touches generated files + new folder); then final report; clean /data/builders/infra-01 (keep until done).
Scratch scripts: /private/tmp/claude-501/-Users-michaelcrystal/b9534115-e058-4cac-a11d-790beba91629/scratchpad (run.sh, ident.sh, proof.sh).

# infra 01: tasks as folders

Title: Tasks as folders: task metadata, a generator registry, generated lists

A task that reuses an existing scenario generator is one folder under `bench/tasks/`; adding it edits no shared file.

- `task.toml` gains `[metadata.mtx]`: generator, reference bot, anchor, ladder, table text, leak needles.
- `mtx_grader::generators` replaces the CLI's `Family` enum and its match arms; `--draw` and `--family` take a generator name.
- `mtx task sync` writes `bench/tasks/tasks.just` (imported by the Justfile), `bench/image/anchors.txt` (read by the image build), the dataset's `[[tasks]]` (now all four) and the two README task tables; `mtx task check` and a unit test fail when they are stale.
- The Justfile's `bots`, `solution`, `leak-audit` and `ladder`, CI's oracle check and wasm clippy, and the Dockerfile's anchors read those lists instead of repeating them.

## Design choices
- Metadata is under `[metadata.mtx]`, next to the keys Harbor already accepts there (`snapshot`, `practice_reference`).
- Generated files are checked by a `cargo test` (so `just test` and CI's Tests step), not a new CI job, to avoid compiling mtx twice.
- Anchors are keyed by the generator unless a task sets `anchor`: two tasks sharing a generator with different references would otherwise overwrite each other's `/opt/mtx/anchors/<generator>`; `mtx task sync` refuses that.
- Tasks that are not hand-listed: CHANGELOG.md stays release prose.
- `dataset.toml` keeps existing digests; new tasks get a name only until `harbor sync` at release.

## Follow-up (not in this PR)
- References (`bots/<x>`), ladder configs/rungs and exploit bots still live under `bots/` and `bench/checks/`; they are found through the metadata (`reference`, `ladder`), but moving them into the task folder is a larger move.
- A new generator still needs its Rust family and one entry in `GENERATORS`.
