"""Fit the market calibration files from an archived public-data pull.

    python3 -I calibration/fitter/run.py --from-archive <dir|.tar.gz> [--asset spcx] [--out DIR]
    python3 -I calibration/fitter/run.py --fetch [--asset spcx] [--out DIR] [--cache DIR]
    python3 -I calibration/fitter/run.py --correlation us-eq --from-archive <a> --from-archive <b> [--out DIR]

With --from-archive the run is offline: every archived file is checked against
the archive's manifest first, any network access is an error, and the pins in
assets/<asset>.toml (fit date, fit time) are used, so the output is the
shipped file byte for byte. With --fetch it downloads the sources into a cache
(calibration/fitter/.cache by default) and stamps the files with today's date and time.

An asset config with `kind = "equity"` is fitted by tickers/equity.py from its
own archive (named in `archive`); the others by the SOL/SPCX code in calib/.
--asset equity runs every equity asset. The archives are listed in
archive*.json beside this file.

--correlation <set> fits the cross-asset correlation file of sets/<set>.toml from
the archives that hold its assets' candles (--from-archive once per archive, offline
only), and writes it with the inputs file; the default --out is calibration/correlation.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import sys
import tarfile
import tempfile
import tomllib
from datetime import datetime, timezone
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

from calib import export_v1, fetch, fit, toml_out  # noqa: E402

SETS = sorted(p.stem for p in (HERE / "sets").glob("*.toml"))
ASSETS = sorted(p.stem for p in (HERE / "assets").glob("*.toml"))
CONFIGS = {a: tomllib.loads((HERE / "assets" / f"{a}.toml").read_text()) for a in ASSETS}
EQUITY = [a for a in ASSETS if CONFIGS[a].get("kind") == "equity"]


def verify_archive(raw: Path) -> None:
    """Every manifest entry exists with the recorded size and SHA-256, and
    nothing else is in the folder."""
    manifest = json.loads((raw / "manifest.json").read_text())
    for name, m in manifest.items():
        body = (raw / name).read_bytes()
        if len(body) != m["bytes"] or hashlib.sha256(body).hexdigest() != m["sha256"]:
            sys.exit(f"archive: {name} does not match the manifest")
    extra = {str(p.relative_to(raw)) for p in raw.rglob("*") if p.is_file()} - set(manifest) - {"manifest.json"}
    if extra:
        sys.exit(f"archive: files not in the manifest: {sorted(extra)[:5]}")


def open_archive(path: Path, tmp: Path) -> Path:
    if path.is_dir():
        return path
    known = {j["file"]: j["sha256"] for j in (json.loads(p.read_text()) for p in HERE.glob("archive*.json"))}
    if path.name not in known:
        sys.exit(f"archive: {path.name} is not listed in any archive*.json")
    if hashlib.sha256(path.read_bytes()).hexdigest() != known[path.name]:
        sys.exit(f"archive: tarball SHA-256 differs from the one recorded for {path.name}")
    with tarfile.open(path) as t:
        t.extractall(tmp / "raw", filter="data")
    return tmp / "raw"


def no_network(*_a, **_k):
    raise RuntimeError("--from-archive is offline, and a source file is missing from the archive")


def in_archive(asset: str, names: set[str]) -> bool:
    """Whether the archive holds what this asset's fit reads."""
    cfg = CONFIGS[asset]
    if cfg.get("kind") == "equity":
        return "phoenix/exchange.json" in names and any(n.startswith(f"phoenix/candles_{cfg['symbol']}_1m_") for n in names)
    need = "binance/" if asset == "sol-usd" else "phoenix/candles_SPCX_"
    return any(n.startswith(need) for n in names)


def fit_equity(cfg: dict, work: Path, pinned: bool) -> Path:
    from tickers import equity

    if pinned:
        fitted_at, fit_date = cfg["fitted_at"], cfg["fit_date"]
    else:
        now = datetime.now(timezone.utc)
        fitted_at, fit_date = now.isoformat(timespec="seconds"), f"{now:%Y%m%d}"
    cal, inputs = equity.fit(cfg, fitted_at, fit_date)
    return equity.write(cal, inputs, work)


def polite(get):
    """The download helper with more patience: Phoenix's candle API rate-limits a long pull."""
    fetch.PAUSE_S = 0.8
    return lambda url, data=None, headers=None, tries=4: get(url, data, headers, tries=10)


def fit_asset(cfg: dict, work: Path, pinned: bool) -> Path:
    """The two stages: fit.py writes the 0.1 intermediate; export_v1 turns it
    into the 1.x file. Returns the 1.x file."""
    asset = cfg["asset"]
    if cfg.get("kind") == "equity":
        return fit_equity(cfg, work, pinned)
    cal = fit.fit_sol()[0] if asset == "sol-usd" else fit.fit_spcx()
    lineage = cfg["lineage_id"]
    cal["meta"]["calibration_id"] = lineage
    (work / f"{lineage}.toml").write_text(toml_out.dumps(cal) + "\n")
    inputs = fetch.provenance(fit.inputs_for(cal["meta"]["inputs_prefixes"]))
    if pinned and "inputs_fetched_before" in cfg:
        # The inputs this fit read: the archive also holds files pulled for a
        # later fit of another asset.
        inputs = [i for i in inputs if i["fetched_at"] < cfg["inputs_fetched_before"]]
    (work / f"{lineage}.inputs.json").write_text(json.dumps(inputs, indent=1))

    export_v1.OUT = work
    export_v1.V01 = {asset: lineage}
    if pinned:
        at = datetime.fromisoformat(cfg["fitted_at"])

        class Pinned(datetime):
            @classmethod
            def now(cls, tz=None):
                return at

        export_v1.datetime = Pinned
        export_v1.FIT_DATE = cfg["fit_date"]
    else:
        export_v1.FIT_DATE = f"{datetime.now(timezone.utc):%Y%m%d}"
    old = tomllib.loads((work / f"{lineage}.toml").read_text())
    new, new_inputs = (export_v1.sol if asset == "sol-usd" else export_v1.spcx)(old)
    return export_v1.write(new, new_inputs, lineage)


def correlation(args) -> None:
    if args.fetch:
        sys.exit("--correlation reads the archived pulls: give --from-archive for each")
    from joint import corr

    cfg = tomllib.loads((HERE / "sets" / f"{args.correlation}.toml").read_text())
    with tempfile.TemporaryDirectory() as t:
        raws = []
        for i, a in enumerate(args.from_archive):
            tmp = Path(t) / str(i)
            tmp.mkdir()
            raw = open_archive(a.resolve(), tmp)
            verify_archive(raw)
            raws.append(raw)
        names = [a.name for a in args.from_archive]
        missing = [a for a in cfg["archives"] if a not in names]
        if missing:
            sys.exit(f"correlation: the fit reads {cfg['archives']}; missing {missing}")
        fetch._get = no_network
        cal, inputs = corr.fit(cfg, raws, cfg["fitted_at"], cfg["fit_date"])
        for r in inputs:
            r["archive"] = next(n for n, raw in zip(names, raws) if (raw / r["name"]).exists())
        made = corr.write(cal, inputs, Path(t) / "out")
        if cfg["expected_id"] and made.stem != cfg["expected_id"]:
            sys.exit(f"{args.correlation}: wrote {made.stem}, expected {cfg['expected_id']}")
        args.out.mkdir(parents=True, exist_ok=True)
        for p in (made, made.with_suffix(".inputs.json")):
            (args.out / p.name).write_bytes(p.read_bytes())
        print(args.out / made.name)


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    src = ap.add_mutually_exclusive_group(required=True)
    src.add_argument("--from-archive", type=Path, action="append",
                     help="archived pull: a folder, or the .tar.gz named in archive.json (--correlation takes one per archive)")
    src.add_argument("--fetch", action="store_true", help="download the sources (cached under calibration/fitter/.cache)")
    ap.add_argument("--asset", choices=[*ASSETS, "equity", "all"], default="all")
    ap.add_argument("--cache", type=Path, default=HERE / ".cache", help="with --fetch: the download cache (a raw folder with manifest.json)")
    ap.add_argument("--correlation", choices=SETS, help="fit this set's correlation file instead of the market files")
    ap.add_argument("--out", type=Path, default=None, help="where the files are written (default calibration/market, or calibration/correlation)")
    ap.add_argument("--out", type=Path, default=HERE.parent / "market", help="where the files are written")
    args = ap.parse_args()
    if args.out is None:
        args.out = HERE.parent / ("correlation" if args.correlation else "market")
    if args.correlation:
        return correlation(args)
    if args.from_archive and len(args.from_archive) != 1:
        sys.exit("--from-archive is given once, except with --correlation")
    if args.from_archive:
        args.from_archive = args.from_archive[0]

    with tempfile.TemporaryDirectory() as t:
        tmp = Path(t)
        if args.fetch:
            raw = args.cache.resolve() / "raw"
            fetch._get = polite(fetch._get)
        else:
            raw = open_archive(args.from_archive.resolve(), tmp)
            verify_archive(raw)
            fetch._get = no_network
        fetch.RAW, fetch.MANIFEST = raw, raw / "manifest.json"

        args.out.mkdir(parents=True, exist_ok=True)
        assets = {"all": ASSETS, "equity": EQUITY}.get(args.asset, [args.asset])
        if not args.fetch and args.asset in ("all", "equity"):
            names = set(json.loads((raw / "manifest.json").read_text()))
            assets = [a for a in assets if in_archive(a, names)]
            if not assets:
                sys.exit("archive: it holds the inputs of no asset")
        for asset in assets:
            cfg = CONFIGS[asset]
            work = tmp / asset
            work.mkdir()
            made = fit_asset(cfg, work, pinned=not args.fetch)
            if not args.fetch and made.stem != cfg["expected_id"]:
                sys.exit(f"{asset}: wrote {made.stem}, expected {cfg['expected_id']}")
            inputs_file = made.with_suffix(".inputs.json")
            for p in (made, inputs_file):
                (args.out / p.name).write_bytes(p.read_bytes())
            print(args.out / made.name)


if __name__ == "__main__":
    main()
