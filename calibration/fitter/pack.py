"""Pack a raw-pull folder (with manifest.json) into a deterministic .tar.gz and
print its SHA-256 and size, the values archive.json records.

    python3 -I calibration/fitter/pack.py <raw dir> <out.tar.gz>
"""

from __future__ import annotations

import gzip
import hashlib
import sys
import tarfile
from pathlib import Path


def main() -> None:
    raw, out = Path(sys.argv[1]), Path(sys.argv[2])
    with out.open("wb") as f, gzip.GzipFile(fileobj=f, mode="wb", mtime=0, filename="") as gz:
        with tarfile.open(fileobj=gz, mode="w", format=tarfile.PAX_FORMAT) as t:
            for p in sorted(x for x in raw.rglob("*") if x.is_file()):
                info = t.gettarinfo(p, arcname=str(p.relative_to(raw)))
                info.uid = info.gid = 0
                info.uname = info.gname = ""
                info.mtime = 0
                info.mode = 0o644
                with p.open("rb") as fh:
                    t.addfile(info, fh)
    print(hashlib.sha256(out.read_bytes()).hexdigest(), out.stat().st_size)


if __name__ == "__main__":
    main()
