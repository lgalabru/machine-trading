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

Two files ship. Both were written by the prototype fitter
(`infra-calib`, `calib/fit.py` and `calib/export_v1.py`) from keyless
public data pulled on 2026-11-11 and 2026-11-12.

| File | Fair value from | Main limits |
|---|---|---|
| `sol-usd.20261112.24db85c6` | Binance SOLUSDT 1 s bars (5 days) and 1 m bars (6 months); Phoenix perp candles and `/exchange`; Jupiter quotes | A CEX proxy, not Pyth. The regimes come from 6 calm-ish months (1.5× apart). Depth and impact come from one Jupiter snapshot. |
| `spcx.20261112.2b3c40c1` | Phoenix SPCX perp mark, 1 m, 2026-06-15 to 2026-11-11 (since the IPO week) | The mark is the oracle plus the perp's premium. Only regular hours vs closed hours is fitted. The open gap rests on 107 opens. Venue numbers come from 30 days. Book and impact are defaults. |

Each file lists its sources and limits under `[meta]`.

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
  - The fitter sets `per_second_sd` so that the model's median window sd
    matches the real windows' median. It divides out what the model's own
    windows read at unit level.
  - It fits `df` the same way, to the real windows' within-window
    kurtosis.
  - Regimes switch at their hourly rates. The calendar and the hour of the
    day scale each block.
- **Returns.** The model runs GARCH(1,1) on blocks with unit-variance
  Student-t innovations, then a Gaussian bridge from blocks to slots.
- **Events.** A gap is drawn when the calendar enters a session kind that
  gaps. Background jumps (a Poisson process) are opt-in, because the
  fitted tails already hold those moves. A tail move lands in one slot,
  not spread evenly over its block.
- **Several assets.** `draw_joint` correlates block innovations with a
  stated correlation matrix on one shared clock. Cross-asset correlation
  isn't fitted yet, so the matrix is a design parameter.

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
waypoint_slots = 4
jumps = false                  # background jumps on top of the fitted tails
gaps = true
[start]
mode = "before"                # draw | session | before | at
kind = "regular"
lead_seconds = 60
```

```sh
mtx market-cal show calibration/market/spcx.20261112.2b3c40c1.toml
mtx market-cal generate --calibration calibration/market/spcx.20261112.2b3c40c1.toml \
  --template bench/tasks/<task>/environment/practice/<one>.json \
  --spec open.toml --out /tmp/drawn --prefix open --count 6 --seed 7
```

`generate` keeps everything in the template (mandate, flow, funding) except
the path, the fair value and the record.

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
  2026-06-15 to 2026-11-11 (1,284 windows).

| Check | Statistic | Gated | The obviously wrong value its band must leave out |
|---|---|---|---|
| `vol_level` | median over windows of their return sd | yes, required | half and twice the real value |
| `vol_of_vol` | coefficient of variation of the window sds: clustering across sessions | yes, required | the windows at one common sd |
| `kurtosis_within` | kurtosis of returns, each window scaled to unit RMS | yes, required (or `tail_ratio`) | Gaussian returns at each window's sd |
| `tail_ratio` | 99th over 90th percentile of \|r\|, each window scaled to unit RMS | yes, required (or `kurtosis_within`) | the same Gaussian windows |
| `clustering_lag_<n>` | ACF of \|r\| within windows at each lag | yes | each window shuffled |
| `predictability` | mean lag-1 ACF | yes | a lag-1 ACF 0.3 off |
| `kurtosis_pooled`, `tail_index` | kurtosis and Hill α of pooled returns | no (`info`) | |
| `gap_size_median`, `gap_size_p90` | \|gap\| at session opens vs the calendar's fitted gap | yes | |
| `round_trips` | probe buy-then-sell (and the reverse) one slot apart, through the book plus impact plus taker fees, at the p50/p90/p99 flow sizes | yes | |

**Bands are sampling spread, with no stated floors.** The fitter records
each statistic's spread over the real windows (`[realism.spread]`):
windows drawn with replacement, 300 at a time, with the sd of the
statistic's log, or of the statistic itself for those near zero. A check
combines two sampling errors and makes its band 3 standard deviations of
the difference:
- the real reference's, from that spread scaled to the real window count;
- the generated statistic's, bootstrapped from the generated windows (200
  draws, fixed seed), and at least the real spread at that many windows.

So a fair generator passes whatever its seed. Only the gap check, resting
on about a hundred opens, keeps a stated band.

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
mtx market-cal check model --calibration calibration/market/sol-usd.20261112.24db85c6.toml
mtx market-cal check market-making --calibration calibration/market/sol-usd.20261112.24db85c6.toml
mtx market-cal check bench/tasks/<task>/environment/practice --calibration <file>
```

- **Targets.** The target is `model` (the calibration's own model, with
  sessions one reference window long, drawn where the reference windows
  come from), a task family (`flash-crash`, `market-making`,
  `funding-carry`, `weekend-gap`, drawn with `--count` and `--seed`), or
  scenario files.
- **Output.** It prints a JSON report and exits 0 on `pass`, 2 on `fail`
  and 3 on `not_judged`. `mtx gates` calls it.

**What the shipped files reject.** A sweep of wrong markets ran at 300
sessions, on seeds 1-3, against both files. The calibrated model passed
all 6 runs, and every wrong market failed every run:
- constant-volatility Gaussian;
- volatility at 0.5×, 0.7×, 1.5× and 2×;
- no session-to-session vol dispersion;
- Gaussian tails;
- tails too heavy (df 2.5 or 3).

The test `adversarial_table` reprints the sweep.

**Limits.** The bands are sampling spread within the fitted period. More
periods of real data would add period-to-period spread. The base
volatility level and the tails' df are fitted to the same window
statistics the checks read, so for the model's own sessions those checks
are in-sample. SPCX's realism reference is in-sample throughout: its
windows are the history the model is fitted on.
