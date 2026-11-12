# infra 05: spot as wasm components, wasm-buildable decoders, Token-2022 and v0

## STATE (final, 2026-11-11 PST)
Done. Branch infra/05-spot-wasm pushed. Runner-5 scratch (container infra-05-build, /data/builders/infra-05) removed.

## Proof
- `cargo +nightly fmt --check`, `clippy -D warnings` (workspace; mtx-kit --all-features; wasm32-wasip2 for starter/reference/maker/carry/weekend/spot-smoke and for mtx-kit's venue features), `cargo test --workspace` and `cargo test -p mtx-kit --all-features`: pass (runner-5, rust 1.97.1).
- New tests: kit `token` (vs spl-token-2022-interface), `chain::token` Token-2022, `chain::lookup`, v0 packing/signing/sending; grader guard v0 forwarding, spot host grid/venues, ignored `the_spot_smoke_component_sells_and_buys_once` (CI runs it), and `tests/tokens_and_v0.rs`: an offline surfnet, a Token-2022 mint with a 1% transfer fee funded via `spot::fund_mint`, transfers through the guard (fee/withheld as the kit predicts), an ALT made in setup and a v0 transfer that lands as version 0 with 2 loaded writable keys, logged by the guard.
- `just spot-smoke`: 8 grades (7 `--components-only` + 1 process). Components fill on Phoenix and on OpenBook alone; buyer is disqualified (side) and reduce-only is disqualified ("bought 0.1 SOL"); the component decodes the books itself through rpc; fuel ~1.66e9 per 20-step session.
- Anchors: the 4 references (as components, `--components-only`), 6 hidden sessions each, one private seed, origin/main vs this branch. Component binaries are byte-identical. Reports are identical except `wall_ms`/`turn_ms` and, for funding-carry and weekend-gap, `fuel_used`/`fuel_max_call`. That fuel noise is already on main: a second main run of weekend-gap with the same seed differs from the first in the same way. Flag it for PR 12, deterministic grading: the fuel counts are not reproducible (probably the size of rpc answers).

Title: Spot sessions run strategy components; Phoenix v1/OpenBook v2 decoding for wasm; Token-2022 and v0 transactions

Spot bots can now be graded the way perp bots are: as wasm components, with `--components-only`.

- **Kit split.** New features `phoenix-v1-venue` and `openbook-v2-venue` (sharing `spot-venue`) build market, book, seat and open-orders decoding, the exact conversions and the instruction builders for `wasm32-wasip2`. Traders, meters and drivers stay behind `phoenix-v1`/`openbook-v2` (the chain). Decoders return a chain-free `protocols::DecodeError`, which converts into `ChainError`.
- **Spot component host** (`crates/grader/src/wasm/spot.rs`). A spot session whose bot has a `bot.wasm` runs it like a perp session does: paused world, per-call and per-session fuel, the grader trading the sheet on Phoenix v1 or OpenBook v2 with the bot's key through the bot's guard (claiming a seat or opening an open orders account on the first resting order). Process spot bots run as before.
- **Spot smoke as a component.** `bots/spot-smoke` builds `bot.wasm` (and `buyer/`, `accumulate/` via features) that decodes every session market's book in the component through `rpc`; `just spot-smoke` grades all of #27's smokes `--components-only`, adds an OpenBook-only smoke, and keeps one process-bot run (`process/`).
- **Token-2022.** `mtx_kit::token` reads either program's mints and accounts with their extensions (transfer fee, default state, pause, hook, non-transferable), computes transfer fees as the program does, and derives ATAs under either program; `chain::token` decodes extended accounts and builds Token-2022 ATAs/transfers; `spot::fund_mint` funds a wallet under the mint's program and refuses tokens the kit can't move.
- **v0 + lookup tables.** `chain::pack_with_lookup_tables` packs v0 transactions; `execute`/`preview`/`fire_and_forget` send them as built; `chain::lookup` reads, creates and extends tables. The guard reads, forwards and logs v0 sends, and the session report counts lookup-loaded keys.

## Design choices

- **One venue for components (TODO for WIT 0.2).** `infra/03-wit-v02` does not exist yet, so components use `mtx:strategy@0.1.0`, whose intents name no venue: a spot component's orders go to the scenario's **first** venue. `start`'s `venue` is JSON listing every market (`venue`, `program`, `market`, mints) and `orders` (the venue orders go to), so a component can read all books itself. PR 3 should add the venue to intents; the host's `Book` is already per venue. A `TODO(wit 0.2)` marks the spot.
- **Market for a spot component:** `mark`/`oracle`/`index` = the Pyth SOL/USD price the session values the account at; `best-bid`/`best-ask` = the traded venue's book; `position` = SOL held anywhere (as the session counts it), `entry-usd` = start price; `pnl` = account value change in USD (the session's own measure, not an estimate). Turn data (tape/depth/clock) is not passed: WIT 0.1.0 has no field for it (also TODO for 0.2).
- **Order rounding:** sizes floor to lots; prices round away from a worse fill (bids/buy limits down, asks/sell limits up); zero-lot orders are skipped. `plan` still cuts to the mandate's cap, as for perps; side restrictions remain the audit's job.
- **Market decode error type:** `PhoenixV1Market::decode`/`OpenBookV2Market::decode` now return `DecodeError` (they must build without the chain). Callers that `?` into the venue errors get `Chain(Decode)` instead of `Instrument` for a bad header; messages are unchanged in substance.
- **Token-2022 decoding is hand-written** (TLV over the base layout) so it builds for wasm; tests check it against `spl-token-2022-interface` (dev-dependency only). Spot sessions themselves stay SOL/USDC (any-asset spot is A0's follow-up); `fund_mint` is the funding path for it. The guard needed no change for Token-2022 (no program filter); its v0 path is now tested.
- **Mint checks:** the kit refuses to fund/trade mints with a transfer hook, non-transferable, paused, or frozen-by-default; transfer fees, permanent delegate, scaled UI amount etc. are reported, not refused (SPCX-style tokens carry some of these).
- `TransactionPlan` gained a `lookup_tables` field (default empty); legacy plans serialize exactly as before.

## Not done / follow-ups

- Multi-venue intents and turn data for components: PR 3 (WIT v0.2) then a small change in `wasm/spot.rs`.
- Venue adapters that trade Token-2022 mints (Phoenix v1 and OpenBook v2 programs are SPL Token only).
- A forked-SPCX-mint test needs a mainnet RPC; the test creates a Token-2022 mint with a transfer fee on an offline surfnet instead.
