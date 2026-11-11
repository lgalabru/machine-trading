# Phoenix SPCX market maker

Run `bash run.sh`. The supplied `bin/bot` is a prebuilt native executable; it
requires no Rust installation, Python, package downloads, or Phoenix HTTP API.
It reads the `MTX_*` contract variables and uses `mtx_kit::run` for the turn socket.

The strategy anchors prices and account risk to the oracle. It cancels the prior
quotes, reads the competing BBO, and normally quotes one tick inside it, with
inventory-dependent prices and sizes. It separates persistent trend estimates
from short bursts of directional taker orders, reading the latter from standard
Solana `getBlock` responses. It excludes its own transactions from those signals.

Strong signals can trigger bounded IOC hedges. Entry decisions include an
estimate of the deeper makers' spread and taker fees. During an expected jump,
the exposed quote moves away from aggressive flow. Favorably priced stale quotes
can also receive limited IOC orders. Displayed crossed books are not treated as
executable round-trip arbitrage.

Risk controls:

- All orders use the single market and account provided by the contract.
- An internal 19.5-share ceiling leaves room below the 20-share mandate.
- The ceiling shrinks with oracle-valued drawdown, reserving 35 USDC for costs
  and estimation differences plus room for a 4.5% adverse move.
- At 65% of the drawdown limit, the strategy cancels quotes and moves toward flat.
- The kit checks the position cap again when converting the sheet to orders.
- IOC hedges use the oracle, rather than the book-sensitive mark, as their
  execution-price reference. Orders are sliced to at most eight shares.

`src/model.rs` and `src/trend.rs` contain compiled decision trees calibrated on
separately generated scenarios. Runtime features contain only current and past
RPC observations and the announced session clock. There are no runtime model
files, scenario files, or training dependencies.

Development in the supplied SDK environment:

```sh
./build.sh
PATH=/usr/local/rustup/toolchains/1.97.1-aarch64-unknown-linux-gnu/bin:$PATH \
  cargo test --release --bin bot
```

The tests check model-export parity, direction symmetry and cooldowns, and
position-capped IOC reversals priced from the oracle. Chain-based validation
reports are in `validation/`. Development simulations and training tools are
not needed to run the delivered bot.
