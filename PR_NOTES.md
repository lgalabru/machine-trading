# PR 10: market calibration, market model v2, realism checks

## Current state (checkpoint 2026-11-12 03:45 UTC)
- Done and pushed (origin/infra/10-market-model): 5 commits, fmt/clippy (workspace, mtx-kit all features, wasm32-wasip2 bots)/`cargo test --workspace` all green on runner-5.
- Running: runner-5, container `infra-10-build` (rust:1.97.1, /data/builders/infra-10 mounted at /work): `/work/anchors.sh` grades the 4 reference anchors (6 drawn sessions, one private seed in /work/.seed, never printed) with `/work/mtx-main` (origin/main) and `/work/mtx-head`, writes /work/anchors/{main,head}/<family>/report.json and `<family> IDENTICAL|DIFFERS` lines to /work/anchors.log.
- Next: read anchors.log (if DIFFERS, rerun main twice to tell wobble from change); CLI smoke (`/work/smoke.sh`); then report. Afterwards remove container `infra-10-build` and /data/builders/infra-10.

Branch `infra/10-market-model`, from origin/main c80ca119.

## Draft PR description

**Market calibration files, a calibrated market model, and realism checks**

Market parameters come from a versioned per-asset calibration file fitted to real data. A new opt-in model draws fat-tailed, regime-switching, calendar-aware paths from it. Realism checks compare generated sessions with the real asset.

- `calibration/market/<asset>.<date>.<hash>.toml` (schema 1.0) plus an inputs manifest (URL and SHA-256 of every download). Loaded by `mtx_grader::market::calibration`, which uses strict serde, a `model` on every section and an id-to-filename check. Ships SOL/USD and SPCX, fitted from keyless public data, each with its sources and limits.
- `mtx_grader::market::model` draws the path. It uses a per-session volatility level and Markov regimes, GARCH(1,1) on blocks with Student-t innovations, and a Gaussian bridge to slots. It adds Poisson background jumps and a trading calendar (pre, regular, post, overnight, weekends, US holidays, New York DST) with gaps at the open, plus correlated multi-asset draws. The path compiles to the existing waypoints (`path`, `fair_path`), and `Scenario::market_model` records the draw.
- `mtx_grader::market::realism` runs stylized-fact checks: vol level, vol of vol across sessions, within and pooled kurtosis, the tail index, |r| ACF clustering, predictability, gap sizes and round trips that lose spread, impact and fees. Each is computed like for like against the real windows stored in the file, with bands.
- `mtx market-cal show | generate | check <scenarios|model|family> --calibration <file>`. `check` prints a JSON report and exits 2 on failure. `mtx gates` (PR 14) calls it.
- Opt-in: existing families, scenarios and the 4 tasks are unchanged. `docs/market-calibration.md` documents it.

## Done

- Schema 1.0 format, loader and validation, with typed accessors (`per_slot_sd`, `venue`, `gap`).
- SOL and SPCX files, produced by `infra-calib/calib/export_v1.py`, which the prototype gained in this PR.
  - The script carries the 0.1 fits over unchanged.
  - It refits for SPCX what 0.1 lacked: GARCH and t df on regular-hours 1 m mark returns, session-level vol dispersion (sd of log hourly RV) and background jumps.
  - It computes the realism references from real windows: SOL on the 2 held-out days of 1 s bars, as 120 s windows (the shortest engine session); SPCX on regular-hours 1 m mark returns, as 60-minute windows.
- Market model v2 with a calendar, regimes, tails, clustering, jumps, gaps and joint correlated draws. Waypoint compilation respects Phoenix move timing. The scenario integration is opt-in.
- The realism library and the CLI.
- Tests:
  - calendar (DST, holidays, sessions past midnight, overlaps);
  - loader (shipped files load, unknown fields and models fail, renamed file fails);
  - model (determinism, vol matches the calibration, `vol_mult`, the open gap at the right slot, closed hours quieter by the fitted ratio, regime switching rates, waypoint spacing, scenario round trip, joint correlation, t variance);
  - realism (a Rust port matching the Python fitter's statistics on a fixture, the calibrated model passing its own checks for SOL and SPCX, a Gaussian walk failing vol-of-vol and tails, today's MakingFamily failing SOL's vol level while its round trips lose, gap masking, round-trip and band logic).

## Not done (deliberately, or for later PRs)

- **Families still use their constants.** The calibration isn't wired into MakingFamily, ShockFamily and the rest: the plan says families switch one at a time, with checks before and after, and the 4 tasks must stay byte-identical.
- **Flow, book and impact are format and accessors only.** Actors (PR 11) consume them. The model draws the fair path only, with no premium path.
- **No aggregational-Gaussianity check.** It needs multi-horizon windows. The calibration records kurtosis by horizon.
- **Spread, depth, impact-curve, impact-decay, spread-vol and venue-basis checks** (CALIBRATION.md 12–17) need engine sessions with books (PR 6/11). The round-trip check takes probe trips from scenarios now, and takes any `RoundTrip`s from session fills later.
- **Bands are the prototype's first guesses.** Setting them from real-vs-real spread needs more weeks of data.

## Design choices

- **Naming.** It is "market calibration" everywhere (`calibration/market/`, `mtx market-cal`, `mtx_grader::market`) to avoid clashing with `mtx calibrate`.
- **Slot length.** The model uses the engine's `SLOT_SECONDS` (0.25 s), not the prototype's 0.4 s, and the file stores `per_second_sd`, so the slot length is the engine's choice.
  - SOL's base sd is the 10 s sd / √10, to stay above 1 s tick noise.
  - SPCX's base sd is the regular-hours 1 m sd / √60.
- **Volatility level is drawn per session.** CALIBRATION.md's finding: within 5 minutes, real clustering is weak, and the real difference is between sessions.
- **Calendar.** Only UTC and New York are supported (hand-written US DST rule, no tz database). Sessions run by weekday and local hours, and a session whose end is before its start runs past midnight. Holidays are listed in the file: NYSE 2026–2027. Drawn starts fall in the file's `draw_range` (2027 for SPCX), a synthetic year.
- **SPCX extended hours.** Pre, post and overnight share the closed-hours vol ratio (0.588), because the fit only splits regular hours from the rest. The file states this. The only gap is at the regular open (22 opens). The closed-hours total move is recorded but not used, because the model integrates closed-hours vol instead.
- **Defaults are labelled.** Book depth and impact for SOL derive from the Jupiter routed curve. SPCX's book and impact, and both assets' permanent share and decay, are defaults, and their `*_source` says so.
- **Correlation is a stated matrix, not fitted.** No cross-asset fit exists yet.
  - Assets share one clock and block length.
  - Each asset's t divisor is its own, so the realized correlation is a little below the stated one.
- **The record never holds the seed.** It holds the calibration id and SHA-256, `vol_mult`, the start instant, the regime levels, session-kind changes and every gap and jump.
- **Bands are `ratio` or `diff`, per check, in the file.**
  - Kurtosis-within and predictability use diff bands. Short windows put the real values near 0, or at the −1/n ACF bias, so ratios would be noise.
  - A check with too little data reports `skipped`, not fail. For example, task sessions of minutes against SPCX's hour-long reference windows.
- **Exit code.** `check` exits 2 when any check fails, after printing the report. It also prints JSON like every mtx command.
- **New error variant.** `Error::InvalidCalibration` was added to the grader's error enum.
- **New dependencies.** `toml = "1"` (grader and CLI) and `sha2` (grader). Both are already in Cargo.lock.

## Conflicts for the integrator

- **`Scenario` gains `market_model: Option<MarketModelRecord>`.** Every struct literal gets `market_model: None` (carry, grade, making, scenario, spot, weekend: one line each).
  - PR 2's `ScenarioV2` has its own `market_model` section (closed period and so on). When merging, carry this record into that section, or rename one of the two, so `save` keeps the record.
- **`scenario::move_slots`** is used from the market model. It is already `pub(crate)`, so there is no change there.
- **CLI.** There is one new `Command::MarketCal` variant and a `mod market_cal`. Otherwise `main.rs` is untouched.
