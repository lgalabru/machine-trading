# STATE (checkpoint 2026-11-12, resume from here)

Done: scenario_v2.rs (types, per-section validate, v1<->v2 conversion, `parse`), Scenario::load/save/validate wired to it, Spot::validate removed, docs/scenario-format.md, unit tests, golden tests (crates/grader/tests/scenario_v2.rs + tests/golden/scenario_v2/*.json). Branch infra/02-scenario-v2 pushed to origin.
Proof passed on runner-14 (rust:1.97.1): nightly fmt, clippy workspace -D warnings, kit --all-features clippy, wasm32-wasip2 clippy of 5 bots, cargo test --workspace (all ok).
Running: byte-identical check of the 4 tasks vs origin/main = acceptance criterion 8, run id 20261112-030305-761b6731 on northpier-runner-12 (containers infra-00-acc-*). Check: `cd /Users/michaelcrystal/kt-wt/infra-00 && ./acceptance/run.sh --status 20261112-030305-761b6731 --box northpier-runner-12`, then `--fetch`.
Next: read criterion 8 result from report; on PASS, send the final report. Clean up /data/builders/infra-02 on runner-14 (files are root-owned: remove through a docker run with the dir mounted).

# Infra PR 02: Scenario v2

## Draft PR description

**Scenario v2: independent sections, with a v1 loader**

A scenario file is now a set of sections that each check themselves.

- `ScenarioV2` (`crates/grader/src/scenario_v2.rs`): `assets`, `venues`, `market_model`, `actors`, `faults`, `tx` (placeholder), `mandate`, `order`, `scoring`, under a `schema: 2` field. Each section has its own `validate`; the few rules that join sections are one function each.
- A file without `schema` is v1 and loads unchanged. v1 and v2 convert into each other without loss; every shipped scenario is checked in a test, and six golden v2 files pin the output.
- `Scenario::save` (so `mtx scenario generate` and `mtx calibrate`) writes v2. `Scenario::validate` now runs the v2 sections, so there is one source of validation. The session still consumes the in-memory `Scenario`; sessions and reports are byte-identical.
- `docs/scenario-format.md` describes the format.

## Design choices

- The runtime `Scenario` struct is unchanged (v1 shape, same serde): many struct literals, tests and the session use it, and PR 6 restructures the session. v2 is the file format plus validation; `ScenarioV2::try_from(&Scenario)` and `into_scenario()` are the bridge.
- `duration_slots` stays top level (Python tools read it there).
- Perp vs spot is explicit: exactly one of `venues.perp` / `venues.spot`. Funding is `venues.perp.funding`; spot markets and turn data are `venues.spot`; the wallet is `assets.wallet`; spot `bound_usd`, participation and net scoring are in `scoring`.
- v1's making/offhours/tape take-flow is one `actors.takers` list; whether it is a maker's, a closed period's or a tape follows from `actors.competitor` / `market_model.closed_period`.
- `Spot::validate` was removed (its checks moved to sections). Error texts are kept so existing tests pass unchanged; taker order errors for making scenarios now use the tape-style message.
- Checked-in v1 practice files are not rewritten.

## For PR 6: call sites to switch to v2 accessors

session.rs: `opts.scenario.spot.is_some()` (~309); `scenario.funding` (354, 369, 462, 481, 500); `match (&scenario.making, &scenario.offhours)` (375, 411) with the `(None, None) if !scenario.tape.is_empty()` arm (404, 414); `scenario.making` (631, 651, 683) and `scenario.offhours` (651); `scenario.faults` (515).
spot.rs: `scenario.order` (790, 1037); `scenario.faults` (816); `scenario.turn_data` (911, 915, 981, 1073); `scenario.participation` (912); `scenario.net` (1052).
scenario.rs: `valuation()`, `setup_slots()`, `max_position()`, `bound_usd()`, `overrides()` branch on making/offhours/tape/spot/funding.
grade.rs: `reference_pnl_usd` (315, 396, 611) is `scoring.reference_pnl_usd`.
weekend.rs 577/598 read `scenario.offhours`.
