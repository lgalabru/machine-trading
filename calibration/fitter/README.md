# Calibration fitter

Fits the files in `calibration/market/` from public price history and venue
data. Python 3.11 or later, standard library only, no keys.

```sh
# Offline, from the archived pull (what the shipped files were fitted on):
python3 -I calibration/fitter/run.py --from-archive <raw dir | calibration-raw-2026-11-11.tar.gz> --out <dir>

# Fresh download into calibration/fitter/.cache (about 40 MB, a few minutes):
python3 -I calibration/fitter/run.py --fetch --out <dir>
```

`--asset sol-usd|spcx|nvda|tsla|spy|qqq` limits a run to one file, and
`--asset equity` to the equity perps; the default is every `assets/*.toml`
whose inputs the archive holds. `--cache <dir>` sets where `--fetch` downloads.

## From the archive

`--from-archive` first checks every file against the archive's `manifest.json`
(size and SHA-256, and no extra files), then runs offline: a source file the
archive lacks is an error, not a download. It uses the pins in
`assets/<asset>.toml` (fit date, fit time, the id the file must get) and exits
with an error if the id differs. The shipped files are reproduced byte for
byte.

The archive is a deterministic `.tar.gz` of the raw folder, named by SHA-256 in
`archive.json` (SOL and SPCX, pulled 2026-11-11) or `archive-tickers.json` (NVDA,
TSLA, SPY and QQQ, pulled 2026-11-13). `run.py` finds the entry by the tarball's file name. It is kept outside git, as the raw data rule requires. Build one
from a raw folder with `python3 -I calibration/fitter/pack.py <raw dir> <out.tar.gz>`.

## Layout

| Path | What |
|---|---|
| `run.py` | The single entry point |
| `pack.py` | Packs a raw folder into the archive tarball |
| `archive.json` | The archived pull: tarball SHA-256, size, file count |
| `assets/<asset>.toml` | Per-asset pins for a reproducible run |
| `calib/fetch.py` | Keyless sources, download cache, `manifest.json` |
| `calib/sources.py` | The fixed windows a fit pulls |
| `calib/stats.py` | Moments, Hill tail index, ACF, GARCH(1,1), regime mixture, jumps |
| `calib/fit.py` | Stage 1: the 0.1 fit for SOL/USD and SPCX |
| `calib/export_v1.py` | Stage 2: fits the model to the realism windows and writes the 1.x file |
| `calib/facts.py`, `calib/generate.py` | The stylized-fact comparison helpers |
| `calib/toml_out.py` | A small TOML writer |
| `tickers/equity.py` | The equity perps: the SPCX fit driven by `assets/<asset>.toml`, with fitted regimes and a gap fallback |
| `archive-tickers.json` | The archived ticker pull |

## Equity perps

An asset config with `kind = "equity"` is fitted by `tickers/equity.py` from
Phoenix's 1 m mark candles and `/exchange` alone (Pyth history is not keyless).
Its keys: the pull window (`pull_from`, `pull_to`), the fit window (`fit_from`),
the venue-stats start (`venue_from`), `regime_min_days` (two volatility states
at or above it, one below), `min_sessions` (the open gap is fitted at or above
it, and else scaled from `gap_fallback`'s) and, for a reproducible rerun, the
pins (`fit_date`, `fitted_at`, `expected_id`).

To refit after a new pull: `run.py --fetch --asset equity --cache <dir>`, pack
`<dir>/raw` with `pack.py`, record the tarball in an `archive*.json`, set the pins
from the fetch run and check that `--from-archive` reproduces it.

## Ids

A file's id is `<asset>.<date>.<hash>`; the hash covers the inputs' SHA-256s and
the fitter's code: `calibration/fitter/calib/*.py` for SOL and SPCX, and for the
equity perps `calib/*.py` and `tickers/*.py` together. Changing the fitter's code
makes a new id on the next fit, by design, so `calib/` stays as it is: it holds the
code the shipped SOL and SPCX ids were made with.
