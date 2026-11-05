#!/usr/bin/env bash
# Maintainer-only calibration bot; it consumes only public gateway observations.
set -euo pipefail
root=$1
executor=$2
address=$3
mkdir -p "$root"
container=$(docker create mth-backrun-runtime:local)
trap 'docker rm "$container" >/dev/null' EXIT
docker cp "$container:/usr/local/bin/mth-backrun" "$root/bot"
cp "$executor" "$root/executor.so"
cat > "$root/run.sh" <<'EOF'
#!/bin/bash
set -euo pipefail
exec ./bot reference --gateway "$MTH_BACKRUN_GATEWAY" --strategy 1 --keep-basis-points 9900
EOF
chmod 755 "$root/run.sh" "$root/bot"
python3 - "$root" "$address" <<'PY'
import pathlib,json,hashlib,sys
root=pathlib.Path(sys.argv[1]); address=sys.argv[2]
manifest={'version':2,'executable':'bot','executor':address,'programs':[{'address':address,'file':'executor.so','sha256':hashlib.sha256((root/'executor.so').read_bytes()).hexdigest()}],'scratch':[]}
(root/'manifest.json').write_text(json.dumps(manifest,indent=2)+'\n')
PY
