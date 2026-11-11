# Published bench results

One directory per release, written by `mtx bench publish <batch> <version>`
(see [docs/development.md](../../docs/development.md)). Browse one with
`mtx bench review <version>`, compare two with
`mtx bench compare <a>...<b>`.

Each holds `release.json` (where it came from, its tasks, and any file left
out for size), the batch's plan (`batch.json`), each task's `task.toml` as it
ran, and the Harbor jobs and trials, trimmed to what the review page reads:
each bot session log keeps its first 16 KiB and last 48 KiB, and each of
the reference bot's sessions is kept once per task. A trial whose reference
session matches one another trial keeps, but for wall-clock fields, lists
it in `verifier/sessions/reference-shared.json` instead, and the review
page reads the shared copy. Don't edit them by hand: publish again with
`--force`.
