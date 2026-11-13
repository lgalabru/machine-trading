# Published bench results

One directory per release, written by `mtx bench publish <batch> <version>`
(see [docs/development.md](../../docs/development.md)). Browse one with
`mtx bench review <version>`, compare two with
`mtx bench compare <a>...<b>`.

The repo keeps one file per release, `release.json`: where the release came
from, its tasks, its scores (`"scores"`, each model's skill and reward by
task) and where its files are (`"store"`). The files are one archive,
`<version>/release.tar`, in the Hugging Face dataset
[solana-foundation/machine-trading-results](https://huggingface.co/datasets/solana-foundation/machine-trading-results),
pinned by revision and sha256. `mtx bench review`, `compare` and `export`
fetch it once, check its sha256 and unpack it into
`~/.cache/mtx/results/` (`MTX_CACHE` moves it).

Beside each archive, the dataset has the release as two tables for the
Hub's viewer and `datasets.load_dataset`: `<version>/trials.jsonl`, a row
per run (skill, reward, PnL, tokens, cost, time, a link to the run's review
page), and `<version>/sessions.jsonl`, a row per hidden session a bot
played (`crates/cli/src/tables.rs`). Its card, `README.md`, holds the
latest release's leaderboard and the list of releases. `mtx bench publish`
writes all three; `mtx bench upload <version>` writes a release's tables
and the card again. A release used to be
committed whole; `mtx bench upload <version>` moves one to the dataset.

An archive holds the batch's plan (`batch.json`), each task's `task.toml` as
it ran, and the Harbor jobs and trials, trimmed to what the review page reads
and compacted (`crates/cli/src/compact.rs`; `release.json` says `"format": 2`):

- each bot session log keeps its first 8 KiB and last 24 KiB;
- session files keep no transaction signatures (a throwaway chain's), and
  the calls' sends as a count; their steps, transactions and taker orders
  are columns, a list per field;
- each scenario is kept once, in `scenarios/<hash>.json.gz`, and a session
  points to it (`{"$ref": …}`);
- each of the reference bot's sessions is kept once per task: a trial whose
  copy matches another's, but for wall-clock fields, lists it in
  `verifier/sessions/reference-shared.json`;
- each source file of the bots is kept once: a trial whose file matches an
  earlier trial's lists it in `shared-files.json`.

The review page reads all of it as the grader wrote it. A release published
before compacting existed is compacted with `mtx bench compact <version>`,
before it is uploaded.

A **slim** release (`mtx bench publish --artifacts <dir>`, or `mtx bench
slim <version> --artifacts <dir>` for one published before) keeps only
`release.json`, `batch.json`, `tasks/`, and per trial its `result.json`,
`config.json`, `verifier/report.json`, `verifier/reward.json` and the bot's
source. Its full copy, everything above, is `<dir>/<version>`, outside the
repository, with `artifacts.sha256` listing every file the slim release
leaves out; the slim `release.json`'s `artifacts` section records that
list's SHA-256. Readers use the full copy when `MTX_ARTIFACTS=<dir>`.
It uploads nothing. Don't edit releases by hand: publish again with
`--force`.
