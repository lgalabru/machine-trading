# Market calibration

A market calibration is one file per asset with the numbers the market
model draws sessions from: how volatile the asset is, how fat its tails
are, how its volatility clusters and switches regime, when it trades, how
it gaps at the open, and what its venues charge and trade. Each number is
fitted to real market data or labelled as a default.

This is not `mtx calibrate`. That command writes a scenario's
`reference_pnl_usd`. Market calibration is `mtx market-cal`.

## The files

```
calibration/market/<asset>.<date>.<hash>.toml          the calibration
calibration/market/<asset>.<date>.<hash>.inputs.json   every input: url, sha256, bytes, fetch time
```

- **The id.** It is `<asset>.<date>.<hash>`, where the hash covers the
  inputs' SHA-256s and the fitter's code. A file is named by its id, and
  loading a renamed file fails.
- **Immutable.** A refit is a new file with a new id; nothing is edited in
  place. A scenario drawn from a file records its id and the file's SHA-256.
- **Raw data is never committed.** The inputs manifest cites each download
  by URL and SHA-256.
- **Strict schema.** The schema is version 1.x
  (`mtx_grader::market::calibration`). Unknown fields and unknown models
  fail to load.

Six files ship. All were written by the fitter in `calibration/fitter` from
keyless public data pulled on 2026-11-11, 2026-11-12 and 2026-11-13.

| File | Fair value from | Main limits |
|---|---|---|
| `sol-usd.20261112.0dbc6cdf` | Binance SOLUSDT 1 s bars (5 days) and 1 m bars (6 months); Phoenix perp candles and `/exchange`; Jupiter quotes | A CEX proxy, not Pyth. The regimes come from 6 calm-ish months (1.5× apart). Depth and impact come from one Jupiter snapshot. |
| `spcx.20261112.d1f48be2` | Phoenix SPCX perp mark, 1 m, 2026-06-15 to 2026-11-11 (since the IPO week) | The mark is the oracle plus the perp's premium. Only regular hours vs closed hours is fitted. The open gap rests on 107 opens. Venue numbers come from 30 days. Book and impact are defaults. |
| `nvda.20261113.903d1120` | Phoenix NVDA perp mark, 1 m, 2026-06-15 to 2026-11-12 (109 regular sessions) | As SPCX, plus two volatility states (the lower and upper halves of the days, not separate clusters). The open gap rests on 109 opens. |
| `tsla.20261113.8b7bca8e` | Phoenix TSLA perp mark, 1 m, 2026-06-22 to 2026-11-12 (104 regular sessions) | As NVDA, with 104 opens. GARCH persistence sits at the top of its search (weakly identified). |
| `spy.20261113.2b088307` | Phoenix SPY perp mark, 1 m, 2026-09-14 to 2026-11-12 (44 regular sessions) | One state (60 days). The open gap is SPCX's scaled by the volatility ratio, not fitted. The mark is effectively unsmoothed, and its closed-hours volatility is 1.08 of the regular-hours one. |
| `qqq.20261113.60ee1c12` | Phoenix QQQ perp mark, 1 m, 2026-09-14 to 2026-11-12 (44 regular sessions) | One state (60 days). The open gap is SPCX's scaled by the volatility ratio, not fitted. |

NVDA, TSLA, SPY and QQQ share SPCX's limits: the mark is the oracle plus the
perp's premium, Pyth's history is not keyless and so not used, and the book and
impact are defaults (a Phoenix book and signed fills have not been recorded;
the candles hold 122,000 to 427,000 trades per market, not signed fills).
Every US equity file has `block_seconds = 60`, so `draw_joint` accepts any set
of them together.

Each file lists its sources and limits under `[meta]`.

### Rerunning the fitter

`calibration/fitter` is stdlib-only Python (3.11 or later). The raw pull is
archived once outside git (`calibration/fitter/archive.json` names it by
SHA-256), and a rerun from it is offline and deterministic:

```sh
python3 -I calibration/fitter/run.py --from-archive calibration-raw-2026-11-11.tar.gz --out /tmp/cal
python3 -I calibration/fitter/run.py --from-archive calibration-raw-tickers-2026-11-13.tar.gz --out /tmp/cal
```

The first writes `sol-usd.20261112.0dbc6cdf` and `spcx.20261112.d1f48be2`; the
second (`archive-tickers.json`) the four equity files; each with its inputs
file, byte for byte as shipped. A run writes the assets whose inputs the
archive holds. The run checks every archived
file against the archive's manifest first and refuses to touch the network.
`--fetch` downloads fresh sources instead and stamps the files with today's
date and time. The id's hash covers the fitter's code, so any change to it
makes a new id: `calib/*.py` for SOL and SPCX, and `calib/*.py` with
`tickers/*.py` for the equity files. `calib/` is frozen for that reason.

### Sections

| Section | What | Used by |
|---|---|---|
| `[meta]` | id, asset, symbol, fit time, fitter code hash, inputs digest and count, sources, limits, date windows | provenance |
| `[fair_value]` | `per_second_sd`; `block_seconds`; an intraday shape (continuous calendars) | model |
| `[fair_value.tails]` | Student-t `df` of the block innovations; Hill α; kurtosis by horizon | model |
| `[fair_value.clustering]` | GARCH(1,1) `alpha`, `beta` at the block, half-life | model |
| `[fair_value.regimes]` | states: volatility multiplier, within-state log-sd (the session-to-session spread), share, hourly transition matrix | model |
| `[fair_value.jumps]` | background jumps: rate per hour, median and p90 size | model |
| `[calendar]` | `continuous`, or `sessions`: a timezone, session kinds (`pre`, `regular`, `post`, `overnight`) by weekday and local hours with volatility multipliers, closed-hours multiplier (weekends, holidays), holidays, gaps at a kind's open, the draw range | model |
| `[venues.<venue>]` | fees, tick, trades per minute and dispersion, trade-size distribution, premium (trade over mark), funding parameters | actors (PR 11), funding families |
| `[book]` | half-spread, depth by distance, impact `k q^γ` with permanent share and decay; each with a `*_source` saying fitted or default | realism round trips; actors |
| `[realism]` | stylized facts of real session-length windows, their sampling spread, and the narrowest bands generated sessions must fall in | realism checks |

## The market model (v2)

`mtx_grader::market::model` draws a fair-value path slot by slot from a
calibration:

- **Clock.** The session starts at an instant on the calendar. The
  start is drawn anywhere, inside a session kind, `lead_seconds` before
  one opens, or at a fixed instant.
- **Volatility level.** It is drawn per session from the regime's
  multiplier and its within-state log-sd. The draw is normalized so that
  the median session (over regimes, level draws and hours of the day)
  runs at `per_second_sd`.
  - The fitter fits the model to the same window statistics the checks
    read, by simulating the model's own sessions as `check_model` draws
    them (for SOL, ticked as the real closes are):
    - `per_second_sd` matches the real windows' median window sd;
    - `df` matches their within-window kurtosis;
    - the level dispersion matches their vol of vol. For SOL that is a
      common scale on the regimes' log-sds; for SPCX it is the single
      state's log-sd.
    - The mark's half-life matches the lag-1 return ACF.
    - The GARCH `alpha` and persistence match the |r| ACF at lags 1 and 5:
      for SPCX within its 30-minute windows, for SOL within 1 h windows of
      6 months of 1 m bars.
  - Regimes switch at their hourly rates. The calendar and the hour of the
    day scale each block.
- **Returns.** The model runs GARCH(1,1) on blocks with unit-variance
  Student-t innovations, then a Gaussian bridge from blocks to slots.
- **Mark.** `[fair_value.mark]` is a fitted reduced form, not a model of
  any venue's mechanics. Every `update_seconds` the output moves toward the
  path by an exponential average with the fitted half-life, and holds in
  between. It reproduces the small positive lag-1 autocorrelation of the
  real series:
  - **SPCX:** 5 s updates, 2.7 s half-life. The real series is Phoenix's
    mark, which is spot plus an EMA of the book-vs-spot gap, blended with
    other prices (`MarkConfig` in `crates/grader/src/phoenix.rs`).
  - **SOL:** 1 s updates, about 0.4 s half-life. This stands in for the
    short-lag momentum of Binance's 1 s closes.
  - **Don't smooth twice.** If generated paths later drive the engine's
    oracle and Phoenix computes its own mark from it, the engine's mark
    smoothing comes on top. In that case, draw the path without
    `[fair_value.mark]`, or refit its half-life against the engine's
    output.
- **Events.** A gap is drawn when the calendar enters a session kind that
  gaps. Background jumps (a Poisson process) are opt-in, because the
  fitted tails already hold those moves. A tail move lands in one slot,
  not spread evenly over its block.
- **Several assets.** `draw_joint` correlates block innovations with a
  correlation matrix on one shared clock. For the US equity perps the matrix
  is fitted: see [Correlation files](#correlation-files). For other sets it is
  a design parameter.

The path compiles to waypoints at most every `waypoint_slots` (default 4),
spaced so every Phoenix move finishes before the next one starts. These are
the same `path` and `fair_path` the session machinery already replays.
`Scenario::market_model` records the calibration id and SHA-256, any stated
departures, the start, the regime levels, the session kinds and every gap
and jump. It never records the seed.

The model is opt-in. A scenario uses it only if it was drawn from it.
Existing families, scenarios and tasks are unchanged, and a scenario
without `market_model` serializes exactly as before.

A spec states every departure from the calibration (TOML, all optional):

```toml
vol_mult = 1.0                 # stress beyond the calibration, stated
regime = "stressed"            # hold one regime; default: drawn, switching
regime_clock = 1.0             # switch this many times faster than the hourly rates
waypoint_slots = 4
jumps = false                  # background jumps on top of the fitted tails
gaps = true
[start]
mode = "before"                # draw | session | before | at
kind = "regular"
lead_seconds = 60
```


**Regimes at session timescale.** Calibrations switch regimes at hourly
rates, so a task minutes long almost never sees a switch. Two stated
departures change that, and the record keeps both (`regime_clock`, and the
whole chain as `regime_set`); a record without them serialises as before:

- `regime_clock` multiplies every state's leave rate. It changes how often
  the state changes, not what each state is: GARCH, the tails, the level
  draw and the calendar are untouched. Switches are drawn at block ends, as
  always. A clock at which some state would leave within one block more
  than half the time is refused, with the largest allowed value.
- `[regimes]` replaces the calibration's regime chain with a stated one,
  in the calibration's own form (`model = "markov"`, `names`, `vol_mult`,
  `log_sd_within`, `share`, `transition_per_hour`, and a `source` saying
  where the numbers come from), checked by the same rules. The median
  session is normalised over the stated chain, so it still runs at
  `per_second_sd`. This is how a family switches states on a calibration
  fitted with one (SPCX's): ground the states in the fitted level spread
  and say so in `source`.

```toml
regime_clock = 51.0
[regimes]
model = "markov"
names = ["calm", "stressed"]
vol_mult = [0.85, 1.7]
log_sd_within = [0.4, 0.4]
share = [0.7, 0.3]
transition_per_hour = [[0.9, 0.1], [0.25, 0.75]]
source = "SPCX's fitted level spread (log-sd 0.54) split into two states"
```

```sh
mtx market-cal show calibration/market/spcx.20261112.d1f48be2.toml
mtx market-cal generate --calibration calibration/market/spcx.20261112.d1f48be2.toml \
  --template bench/tasks/<task>/environment/practice/<one>.json \
  --spec open.toml --out /tmp/drawn --prefix open --count 6 --seed 7
```

`generate` keeps everything in the template (mandate, flow, funding) except
the path, the fair value and the record.

## Correlation files

A correlation file is the matrix `draw_joint` takes for a set of calibrated
assets, fitted to the real assets' co-movement. One ships, for the US equity
perps (`NVDA`, `QQQ`, `SPCX`, `SPY`, `TSLA`):

```
calibration/correlation/<set>.<date>.<hash>.toml          the correlation
calibration/correlation/<set>.<date>.<hash>.inputs.json   every input: url, sha256, bytes, fetch time, archive
```

- **Same rules as a calibration file.** Named by its id, immutable, strict
  schema (`mtx_grader::market::correlation`, version 1.x), a refit is a new
  file. The id's hash covers the inputs' SHA-256s, the pinned calibration
  files' SHA-256s, the fit's parameters and the code of `calibration/fitter/joint/`.
- **Pinned calibrations.** `[meta].calibrations` names the calibration id each
  asset was fitted through. `Correlation::joint_matrix` and `Correlation::draw`
  take any subset of the assets, in any order, and fail if a calibration is not
  the pinned one: the matrix means nothing for another fit of an asset. A
  scenario drawn from a file should record its `correlation_id` and SHA-256
  (`Correlation::sha256`) as it records a calibration's.
- **Not the correlation of the output.** The matrix is the correlation of the
  block innovations. The draw gives each asset its own Student-t factor,
  volatility level and GARCH state, so the generated returns correlate less.
  `realized_target` is what the draw reproduces; `matrix` is what `draw_joint`
  is given.

### What the fit does

`python3 -I calibration/fitter/run.py --correlation us-eq --from-archive <a> --from-archive <b>`
reads the Phoenix 1 m candles of the five assets from the two archives (SPCX's
is the 2026-11-11 pull, the others the 2026-11-13 one), offline, and writes the
file and its inputs. `sets/us-eq.toml` holds the parameters and the pins.

1. **Real correlation.** The regular-hours (09:31 to 16:00 New York) 1 m
   mark-return series of each asset over **one common window**, 2026-09-14 to
   2026-11-10 (42 sessions, 16,380 minutes): from SPY's and QQQ's first full
   week to the end of the SPCX pull. A return exists only where both bars do,
   and no gap is filled forward. For each pair, its statistic is the **median
   over clock-aligned 30-minute windows of the Pearson correlation of their
   1 m returns** (546 windows a pair), the one the joint check judges. The
   pooled 1 m and 5 m correlations are recorded beside it.
2. **Shrinkage.** The targets are the real medians shrunk 0.1 of the way toward
   their average off-diagonal (0.4393).
3. **Simulated moments.** For each pair, the fitter simulates `draw_joint`'s
   walkers (the two calibrations' GARCH, Student-t factors, regime levels and
   switching, and mark smoothing, as the default spec draws them) and solves for
   the matrix entry whose simulated median window correlation equals the
   target (secant steps, 6,000 sessions of 30 blocks, seeded, common random
   numbers). The simulated value is in `simulated_realized`; the Rust joint
   check, which draws with the real model, confirms it.
4. **Positive definite.** A matrix that is not positive definite (eigenvalue
   floor 0.02) would be projected to the nearest correlation matrix (Higham). The
   fitted one was already positive definite, so none was applied.

| Pair | Real median 30 m corr | Target (shrunk) | Matrix entry | Real pooled 1 m | Simulated pooled 1 m | Real pooled 5 m | Co-jumps / expected if independent |
|---|---|---|---|---|---|---|---|
| NVDA-QQQ | 0.6071 | 0.5903 | 0.6471 | 0.6360 | 0.4641 | 0.7284 | 13 / 0.18 |
| NVDA-SPCX | 0.2989 | 0.3129 | 0.3496 | 0.3456 | 0.2329 | 0.3994 | 2 / 0.13 |
| NVDA-SPY | 0.5463 | 0.5356 | 0.5871 | 0.5685 | 0.4275 | 0.6560 | 12 / 0.17 |
| NVDA-TSLA | 0.3231 | 0.3347 | 0.3755 | 0.3033 | 0.2627 | 0.3644 | 4 / 0.08 |
| QQQ-SPCX | 0.3918 | 0.3965 | 0.4372 | 0.4548 | 0.2886 | 0.5464 | 1 / 0.20 |
| QQQ-SPY | 0.7603 | 0.7282 | 0.7939 | 0.7944 | 0.5717 | 0.8957 | 34 / 0.26 |
| QQQ-TSLA | 0.4474 | 0.4466 | 0.4940 | 0.4605 | 0.3385 | 0.5273 | 7 / 0.13 |
| SPCX-SPY | 0.3361 | 0.3464 | 0.3843 | 0.3710 | 0.2602 | 0.4449 | 2 / 0.19 |
| SPCX-TSLA | 0.2657 | 0.2830 | 0.3162 | 0.2945 | 0.2083 | 0.3846 | 3 / 0.09 |
| SPY-TSLA | 0.4161 | 0.4184 | 0.4644 | 0.4267 | 0.3232 | 0.4883 | 8 / 0.12 |

### What it can't do

- **The 1 m scale.** Phoenix's mark is the oracle plus the perp's premium, and
  its 1 m returns carry quote noise the underlyings' don't, so real 1 m
  correlations read below the underlyings' (the Epps effect): the 5 m ones
  are higher for every pair (SPY-QQQ 0.90 against 0.79). The model targets 1 m,
  the scale sessions trade at.
- **Pooled returns.** The draw can't reproduce the real *pooled* 1 m
  correlation (SPY-QQQ 0.79 real, 0.57 drawn). Pooled returns weight the
  large-move windows, where real assets move together and the draw's volatility
  levels and GARCH states, being independent, do not. A bot that sizes on
  pooled correlation across a whole session sees less than the real pair
  shows. Fixing it needs shared volatility levels in `draw_joint`: a model
  change, not a refit.
- **Jumps.** The assets' large moves coincide in the data (SPY-QQQ: 34
  coincident jumps where 0.26 is expected by chance, all in the same
  direction; the pairs with SPCX have 1 to 3). The
  draw's background jumps are off by default and independent when on, so
  co-jumps are reported in the file, not modelled.
- **One window.** 42 sessions, one market regime; correlations of equities
  move with it. The bands below are sampling spread, not regime spread.

## Realism checks

`mtx_grader::market::realism` compares generated sessions with real ones.

- **Like with like.** Each statistic is computed the same way, on windows
  of the same length, for the real data (by the fitter, into `[realism]`)
  and for the generated sessions.
- **Groups, not single sessions.** Short-window estimators are biased, and
  comparing like with like cancels the bias. A single session is never
  judged alone.

Each file's `[realism]` section names the bars each statistic reads:
- **Volatility statistics** read returns over `vol_bars`, coarse enough
  that the tick doesn't dominate. SOL's 1 s closes sit below its $0.01
  tick, so SOL reads 10 s returns.
- **Tail and autocorrelation statistics** read returns over `agg_bars`
  and `cluster_bars`.
- **The real windows.** SOL uses 120 s windows from 2 held-out days (1,438
  windows). SPCX uses 30-minute windows of regular-hours 1 m returns from
  2026-06-15 to 2026-11-11 (1,284 windows); NVDA, TSLA, SPY and QQQ the same
  from their own first full weeks (1,308, 1,248, 528 and 528 windows).
- **SOL's long windows.** SOL also has `[realism.long]`: 1 h windows of 1 m
  closes over 6 months (4,415 windows). Its 120 s windows are too short
  for volatility clustering within a session to show. `check_model`
  draws as many 1 h sessions as short ones for it.

| Check | Statistic | Gated | The obviously wrong value its band must leave out |
|---|---|---|---|
| `vol_level` | median over windows of their return sd | yes, required | half and twice the real value |
| `vol_of_vol` | spread of the windows' log sd (interquartile range / 1.349): clustering across sessions | yes, required | the windows at one common sd |
| `kurtosis_within` | kurtosis of returns, each window scaled to unit RMS | yes, required (or `tail_ratio`) | Gaussian returns at each window's sd |
| `tail_ratio` | 99th over 90th percentile of \|r\|, each window scaled to unit RMS | yes, required (or `kurtosis_within`) | the same Gaussian windows |
| `clustering_lag_<n>` | ACF of \|r\| within windows at each lag | yes | each window shuffled |
| `predictability` | mean lag-1 ACF | yes | a lag-1 ACF 0.3 off |
| `long_clustering_lag_<n>` | ACF of \|r\| within the long windows (SOL: 1 h of 1 m bars), lags 1, 5 and 15 min | lags 1 and 5; lag 15 `info` | each long window shuffled |
| `kurtosis_pooled`, `tail_index` | kurtosis and Hill α of pooled returns | no (`info`) | |
| `gap_size_median`, `gap_size_p90` | \|gap\| at session opens vs the calendar's fitted gap | yes | |
| `round_trips` | probe buy-then-sell (and the reverse) one slot apart, through the book plus impact plus taker fees, at the p50/p90/p99 flow sizes | yes | |

**Bands are sampling spread, with no stated floors.** The fitter records
each statistic's spread over the real windows (`[realism.spread]`):
windows drawn with replacement, 300 at a time, with the sd of the
statistic's log, or of the statistic itself for those near zero. A check
combines two sampling errors and makes its band `spread.sigmas` standard
deviations of the difference:
- the real reference's, from that spread scaled to the real window count;
- the generated statistic's, bootstrapped from the generated windows (200
  draws, fixed seed), and at least the real spread at that many windows.

**Band width.** `spread.sigmas` is set by a Šidák correction so that a
fair generator fails any of a file's gated return checks at most 1% of
the time:
- SOL has 10 gated checks, so 3.29;
- SPCX, NVDA, TSLA, SPY and QQQ have 8 each, so 3.23.

At 3 standard deviations per check, ten checks alone would give a fair
model a 2–3% chance of failing somewhere.

**Measured rates.** The ignored test `false_fail_rates` measures them
(release, `FFR_SEEDS=300 FFR_OFFSET=500`). The seeds are fresh and were
never used in fitting.

| File | Sessions | Calibrated: false fails | No regimes: caught | No GARCH: caught |
|---|---|---|---|---|
| SOL | 300 | 2/300 (0.7%) | 300/300 | 300/300 |
| SOL | 1000 | 0/300 | 300/300 | 300/300 |
| SPCX | 300 | 0/300 | 300/300 | 289/300 (96.3%; not required) |
| SPCX | 1000 | 1/300 (0.3%) | 300/300 | 300/300 |
| NVDA | 300 | 1/300 (0.3%) | 300/300 | 175/300 (58%; not required) |
| NVDA | 1000 | 3/300 (1.0%) | 300/300 | 298/300 (99.3%) |
| TSLA | 300 | 2/300 (0.7%) | 243/300 (81%; not required) | 299/300 |
| TSLA | 1000 | 0/300 | 299/300 (99.7%) | 300/300 |
| SPY | 300 | 0/300 | 300/300 | 94/300 (31%; not required) |
| SPY | 1000 | 0/300 | 300/300 | 261/300 (87%; not required) |
| QQQ | 300 | 4/300 (1.3%) | 300/300 | 145/300 (48%; not required) |
| QQQ | 1000 | 2/300 (0.7%) | 300/300 | 288/300 (96%) |

The test asserts at most 2% false fails and at least 95% catches, with one
exception. At 300 sessions, SPCX can't reliably tell no GARCH from its
calibrated model. Its GARCH is short-lived (a half-life of about 4
minutes), and 300 sessions of 30 minutes catch its absence only 90-98% of
the time, depending on the seeds. That detection is therefore required
only at 1000 sessions, where it is 100%. To judge SPCX's volatility
clustering, check at least 1000 sessions.

The four equity files were measured the same way (`FFR_ASSETS=nvda,tsla,spy,qqq
FFR_SEEDS=300 FFR_OFFSET=500`). Like SPCX, they have 30-minute windows, so no
GARCH is required only at 1000 sessions. Two cells stay below 95% even so, and the
test exempts exactly these two:
- **TSLA, no regimes, 300 sessions (81%).** TSLA's two states move the session
  level so little beyond what GARCH and sampling already give that 300 sessions
  catch their removal four times in five. 1000 sessions catch it 299 times in 300.
- **SPY, no GARCH, 1000 sessions (87%).** SPY's clustering is weak and
  short-lived (persistence 0.52), so its absence is hard to see in 30-minute
  windows.

Only the gap check, resting on about a hundred opens, keeps a stated band.

**Power.** The fitter also records what each statistic reads on the
obviously wrong market (`[realism.null]`):
- Gaussian returns at each real window's sd (thin tails);
- Gaussian returns at the windows' median sd (no vol of vol);
- each real window shuffled (no clustering).

A check whose band also holds that value can't tell the two apart. When it
passes, it reports `no_power` instead of `pass`. The within-window
clustering checks usually have no power on these short windows: there,
clustering shows up between sessions, in `vol_of_vol`.

Pooled kurtosis and Hill's index vary by a factor of 3 or more between
samples of a few hundred real windows (SOL's pooled kurtosis: about 6 to
140), so they are reported (`info`), not gated.

Windows holding a gap are left out of the return checks, and gaps are
checked on their own. Round trips go through a market-making scenario's
competing makers, or else the calibration's default book. A check with too
little data is reported `skipped`: fewer than 30 windows, fewer than 20
gaps, or sessions shorter than the reference window. SPCX's reference
windows are 30 minutes of 1 m bars, so minutes-long task sessions can't be
judged against them on returns.

**Verdict.** The report's `verdict` is one of:
- `pass`: no check failed, and each required group (vol level, vol of
  vol, a tail check) has a check that passed with power;
- `fail`: a check failed;
- `not_judged`: nothing failed, but the return checks didn't run, or a
  required group had no power. It is not a pass.

`passed` is true only on `pass`.

```sh
mtx market-cal check model --calibration calibration/market/sol-usd.20261112.0dbc6cdf.toml
mtx market-cal check market-making --calibration calibration/market/sol-usd.20261112.0dbc6cdf.toml
mtx market-cal check bench/tasks/<task>/environment/practice --calibration <file>
```

- **Targets.** The target is `model` (the calibration's own model, with
  sessions one reference window long, drawn where the reference windows
  come from), a task family (`flash-crash`, `market-making`,
  `funding-carry`, `weekend-gap`, drawn with `--count` and `--seed`), or
  scenario files.
- **Output.** It prints a JSON report and exits 0 on `pass`, 2 on `fail`
  and 3 on `not_judged`. `mtx gates` calls it.

### The joint check

`mtx market-cal check-joint <correlation> --calibration <a> --calibration <b> ...`
draws `--count` (300) joint sessions of any subset of the file's assets, each
one 30 minutes long and starting inside a regular session, and judges each pair: the median over
sessions of the correlation of the 1 m returns, against the real median in
`[realism.pairs]`. `--calibration` repeats (`--calibration a --calibration b`) or takes several
files; at least two. A usage error exits 64, so exit 2 always means a failed check.

- **Band.** As the other checks: `sigmas` standard deviations of the difference
  of two sampling errors, the real median's (its spread over resamples of the
  546 real windows, `corr_spread_sd`) and the generated one's (bootstrapped from
  the generated windows, and at least the real spread at that many windows).
  `sigmas` is 3.29, a Sidak correction for the file's 10 pairs at 1%
  family-wise false fails; a subset is judged at the same width, which is
  conservative.
- **Power.** The obviously wrong value is the pair's windows with the sessions
  of one asset shifted by one against the other's (no correlation). A band that
  also holds it reports `no_power`. A check with fewer than 30 sessions is
  `skipped`.
- **Verdict.** As the others: `pass` needs every pair to pass with power;
  `fail` if any fails; `not_judged` otherwise. Exit code 0, 2 or 3.
- `mtx market-cal show-joint <correlation> [--calibration ...]` prints the file's
  matrices and pairs and, with calibrations, each pair's generated median beside
  the real one.

Measured by the ignored `joint_false_fail_rates` test (release,
`FFR_SEEDS=300 FFR_OFFSET=500`; the seeds were never used in the fit): 300
sessions a check, bars at most 2% false fails and at least 95% catches.

| Assets | Calibrated: false fails | Sessions shuffled between assets: caught | Independent draws (identity matrix): caught |
|---|---|---|---|
| SPY, QQQ (1 pair) | 0/300 | 300/300 | 300/300 |
| NVDA, TSLA, SPY, SPCX (6 pairs) | 0/300 | 300/300 | 300/300 |
| all five (10 pairs) | 0/300 | 300/300 | 300/300 |

**What the shipped files reject.** A sweep of wrong markets ran at 300
sessions, on seeds 1-3, against both files. The calibrated model passed
all 6 runs. The wrong markets failed 58 of 60 runs:
- constant-volatility Gaussian;
- volatility at 0.5×, 0.7×, 1.5× and 2×;
- no session-to-session vol dispersion;
- no GARCH;
- Gaussian tails;
- tails too heavy at df 2.5;
- tails too heavy at df 3. This is the exception: SOL passed it on 2 of
  3 seeds. Its fitted df is 4.19, and at 300 sessions SOL's tail checks
  can't reliably tell df 3 from it.

The test `adversarial_table` reprints the sweep.

**In-sample and out-of-fit checks.** Each file lists, in
`realism.fitted` and `realism.out_of_fit` and in its `limits`, which gated
checks its parameters are fitted to. Those checks are in-sample for the
model's own sessions: they test that the fit and the Rust model agree,
not that the model generalizes.

| File | Fitted (in-sample) | Out of fit |
|---|---|---|
| SOL | `vol_level`, `vol_of_vol`, `kurtosis_within`, `predictability` (on the 2 held-out days); `long_clustering_lag_1` and `_5` (on the 6 months) | `tail_ratio`, `clustering_lag_1` to `_3`, `round_trips` |
| SPCX, NVDA, TSLA, SPY, QQQ | `vol_level`, `vol_of_vol`, `kurtosis_within`, `predictability`, `clustering_lag_1` and `_5`, `gap_size_median` and `_p90` (SPY and QQQ: the gap sizes are SPCX's, scaled) | `tail_ratio`, `clustering_lag_10`, `round_trips` |

SOL's `long_clustering_lag_15` is reported, not gated. Real 1 m
clustering decays more slowly (long memory) than one GARCH(1,1) with
regimes can follow, and the model reads about 0.01 below it.

**Limits.** The bands are sampling spread within the fitted period. More
periods of real data would add period-to-period spread. SPCX's realism
reference is in-sample throughout: its windows are the history the
model is fitted on.
