# Published bench results

One directory per release, written by `mtx bench publish <batch> <version>`
(see [docs/development.md](../../docs/development.md)). Browse one with
`mtx bench review <version>`, compare two with
`mtx bench compare <a>...<b>`.

Each holds `release.json` (where it came from, its tasks, and any file left
out for size), the batch's plan (`batch.json`), each task's `task.toml` as it
ran, and the Harbor jobs and trials, trimmed to what the review page reads.
Don't edit them by hand: publish again with `--force`.
