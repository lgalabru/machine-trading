"""Fit the market calibration files from an archived public-data pull.

    python3 -I calibration/fitter/run.py --from-archive <dir|.tar.gz> [--asset spcx] [--out DIR]
    python3 -I calibration/fitter/run.py --fetch [--asset spcx] [--out DIR]

With --from-archive the run is offline: every archived file is checked against
the archive's manifest first, any network access is an error, and the pins in
assets/<asset>.toml (fit date, fit time) are used, so the output is the
shipped file byte for byte. With --fetch it downloads the sources into a cache
(calibration/fitter/.cache) and stamps the files with today's date and time.
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

ASSETS = sorted(p.stem for p in (HERE / "assets").glob("*.toml"))


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
    want = json.loads((HERE / "archive.json").read_text())["sha256"]
    if hashlib.sha256(path.read_bytes()).hexdigest() != want:
        sys.exit("archive: tarball SHA-256 differs from archive.json")
    with tarfile.open(path) as t:
        t.extractall(tmp / "raw", filter="data")
    return tmp / "raw"


def no_network(*_a, **_k):
    raise RuntimeError("--from-archive is offline, and a source file is missing from the archive")


def fit_asset(cfg: dict, work: Path, pinned: bool) -> Path:
    """The two stages: fit.py writes the 0.1 intermediate; export_v1 turns it
    into the 1.x file. Returns the 1.x file."""
    asset = cfg["asset"]
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


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    src = ap.add_mutually_exclusive_group(required=True)
    src.add_argument("--from-archive", type=Path, help="archived pull: a folder, or the .tar.gz named in archive.json")
    src.add_argument("--fetch", action="store_true", help="download the sources (cached under calibration/fitter/.cache)")
    ap.add_argument("--asset", choices=[*ASSETS, "all"], default="all")
    ap.add_argument("--out", type=Path, default=HERE.parent / "market", help="where the files are written")
    args = ap.parse_args()

    with tempfile.TemporaryDirectory() as t:
        tmp = Path(t)
        if args.fetch:
            raw = HERE / ".cache" / "raw"
        else:
            raw = open_archive(args.from_archive.resolve(), tmp)
            verify_archive(raw)
            fetch._get = no_network
        fetch.RAW, fetch.MANIFEST = raw, raw / "manifest.json"

        args.out.mkdir(parents=True, exist_ok=True)
        for asset in ASSETS if args.asset == "all" else [args.asset]:
            cfg = tomllib.loads((HERE / "assets" / f"{asset}.toml").read_text())
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
