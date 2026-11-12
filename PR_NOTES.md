# STATE: complete (box scratch removed)
- Pushed to origin/infra/08-oracles-crank. fmt (nightly), clippy (workspace, kit all-features, wasm32-wasip2 bots), `cargo test --workspace` all pass (runner-14, rust:1.97.1).
- Anchors: 4 tasks x 6 hidden sessions (one private seed per family generated on the box, not printed), reference as bot and reference, origin/main vs this head. Reports match field for field (scores, PnL, positions, steps, marks) except wall_ms and wasm fuel counters; per-step details also differ only in turn_ms, tx signatures and compute units (each session draws a random bot keypair, so the same holds between two runs of one binary).

# PR 8: Oracle writers, failure modes, venue crank

Draft title: Oracle writers with failure modes, and a per-step venue crank

One line: scenarios can drive the prices venues read (Phoenix's oracle, Pyth, Scope) and break them on purpose, and venues get a hook to stay valid on the engine's clock.

- `oracles` scenario section: feeds with a price path, cadence and confidence; typed `OracleSource` writers for Phoenix (existing `phoenix-oracle-price` template, split into steps Phoenix accepts), Pyth `PriceUpdateV2` (`pyth-price-feed-v2`) and Scope (`kamino-scope-price`); a Switchboard type that validation refuses until a consumer needs it.
- Seeded failure modes on every source: `stale`, `wide_confidence`, `off_price`, `delayed`; windows are fixed or drawn from the seed.
- `VenueCrank` trait and `Cranks`, called each step after blocks land and held sends are released, on both perp and spot loops; `SessionOptions.cranks` takes a factory. No venue uses it yet; a recording test double covers it.
- Scenarios without `oracles` and sessions without cranks take exactly the old path.

## Design choices
- Standalone `oracles` section (scenario v2 / PR 2 not present on origin/main). `deny_unknown_fields`, `skip_serializing_if` none, so old scenario files serialize unchanged.
- Writes are Surfpool overrides registered before the cheatcode lock (same as the existing path); no new cheatcode use after lock. Cranks use ordinary transactions.
- Failures act on the planned publications, not per writer, so they behave the same on every source. Phoenix has no confidence or publish time; Scope has no confidence; those fields are dropped by that writer. Phoenix restamps all reading ages whenever any Phoenix override plays, so `stale` on a Phoenix feed freezes the price, not the age.
- Pyth times: publish_time = chain clock at registration + as-of slot * 250 ms; `posted_slot` = base slot + as-of slot.
- Crank failure aborts the session as a grader error.
- Not verified against a live Surfnet in this PR (templates' value keys taken from surfpool's template YAML and examples): the first consumer PR should run one end to end.
