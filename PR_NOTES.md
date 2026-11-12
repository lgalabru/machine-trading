# infra 05: spot as wasm components, wasm-buildable decoders, Token-2022 and v0

## STATE (checkpoint 2026-11-11 ~19:50 PST)
- Done and pushed (origin infra/05-spot-wasm, 4 commits up to 08c2769b): kit split, Token-2022, v0/ALT, spot component host, spot-smoke component. fmt, workspace clippy, kit --all-features clippy+tests, wasm clippy (all bots + kit venue features), cargo test --workspace: all pass.
- `just spot-smoke` on runner-5: all 8 grades ran (7 --components-only + 1 process); buyer/reduce-only disqualified as expected.
- RUNNING: anchor proof on northpier-runner-5, container `infra-05-build`, script /data/builders/infra-05/proof.sh (base = origin/main c80ca119 in /w/base, target /w/target-base; branch in /w/src, target /w/target); output /data/builders/infra-05/proof.out and /w/proof/*.json. Seed in /w/proof/.seed (never print; delete when done).
- NEXT: run /data/builders/infra-05/compare.sh in the container (expects identical reports without wall-clock fields); then report; then clean up: `docker rm -f infra-05-build` and remove /data/builders/infra-05 on runner-5.


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
