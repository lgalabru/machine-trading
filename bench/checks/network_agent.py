"""A Harbor agent for the network check (bench/checks/network/probe.sh): it
tries, from the agent's container and as the agent user, to reach what a
guarded task's agent must not (ghcr.io and github.com, which hold the full
`mtx` and the reference bots, and the rest of the internet) and what it must
(the crates registry, its model's address, and the task's chain), checks the
image is the agent image (`mtx scenario generate` fails, no /opt/mtx/anchors),
and writes
what answered to network.json in its logs.

    MTX_PROBE_MODEL=<host>:<port> PYTHONPATH=bench harbor run -p bench/tasks/<task> \
        -o jobs -a checks.network_agent:NetworkProbe --allow-agent-host <host> \
        --disable-verification
"""

import json
import os
import re

from harbor.agents.base import BaseAgent
from harbor.environments.base import BaseEnvironment
from harbor.models.agent.context import AgentContext

SCRIPT = r"""
set -u
tls() { # host server-name: a TLS handshake that returns a certificate
  local args=(-connect "$1:443"); [ -n "$2" ] && args+=(-servername "$2")
  timeout 20 openssl s_client "${args[@]}" </dev/null 2>/dev/null | grep -q 'BEGIN CERTIFICATE'
}
fronted() { # host server-name: the certificate served names ghcr.io or github (the host, not the server-name's)
  timeout 20 openssl s_client -connect "$1:443" -servername "$2" </dev/null 2>/dev/null \
    | openssl x509 -noout -subject -ext subjectAltName 2>/dev/null | tee /dev/stderr | grep -qi 'ghcr\|github'
}
http() { # host port host-header: any HTTP status line back
  timeout 15 bash -c 'exec 3<>/dev/tcp/$0/$1; printf "GET / HTTP/1.1\r\nHost: $2\r\nConnection: close\r\n\r\n" >&3; read -r line <&3; case "$line" in HTTP/*) exit 0;; esac; exit 1' "$@" 2>/dev/null
}
say() { local name=$1; shift; if "$@"; then echo "reach $name yes"; else echo "reach $name no"; fi; }
for host in crates.io index.crates.io static.crates.io; do say "tls:$host" tls "$host" "$host"; done
for host in github.com codeload.github.com raw.githubusercontent.com objects.githubusercontent.com \
    pkg-containers.githubusercontent.com ghcr.io example.com; do
  say "tls:$host" tls "$host" "$host"
done
say "http:github.com" http github.com 80 github.com
ip="$(getent hosts ghcr.io | awk '{ print $1; exit }')"
say "ip-no-sni:ghcr.io" tls "$ip" ""
say "ip-sni-index.crates.io:ghcr.io" fronted "$ip" index.crates.io
# The image Harbor ran is the agent image: its mtx runs but can't draw a
# scenario, and the reference bots aren't in it.
say "mtx-runs" mtx --help
say "scenario-generate" mtx scenario generate --out /tmp/net-probe-gen --prefix probe --count 1 --seed 1
say "anchors-dir" test -e /opt/mtx/anchors
say "model" http "@MODEL_HOST@" "@MODEL_PORT@" "@MODEL_HOST@"
# Cargo: the registry answers, and a git dependency on the public repo doesn't.
tmp="$(mktemp -d)"
mkdir -p "$tmp/reg/src" "$tmp/git/src"; touch "$tmp/reg/src/lib.rs" "$tmp/git/src/lib.rs"
printf '[package]\nname = "reg"\nversion = "0.0.0"\nedition = "2021"\n[dependencies]\nitoa = "1"\n' > "$tmp/reg/Cargo.toml"
printf '[package]\nname = "gitdep"\nversion = "0.0.0"\nedition = "2021"\n[dependencies]\nmtx-grader = { git = "https://github.com/lgalabru/machine-trading" }\n' > "$tmp/git/Cargo.toml"
say "cargo-registry" env CARGO_HOME="$tmp/home-a" timeout 120 cargo fetch --manifest-path "$tmp/reg/Cargo.toml" -q
say "cargo-git:github.com" env CARGO_HOME="$tmp/home-b" CARGO_NET_RETRY=1 CARGO_HTTP_TIMEOUT=20 timeout 120 cargo fetch --manifest-path "$tmp/git/Cargo.toml" -q
# The task's chain: one practice scenario played on the offline surfnet in the container (not by `mtx-practice`, so the task needn't be calibrated yet).
practice="$(ls /opt/mtx/practice/*.json 2>/dev/null | head -1 || true)"
if [ -n "$practice" ]; then
  play() { timeout 900 mtx grade --jobs 1 --snapshot "$MTX_SNAPSHOT" --bot /app/bot --components-only --logs /tmp/net-probe-logs "$1" > /tmp/net-probe-grade.json; }
  say "chain" play "$practice"
else
  echo "reach chain skipped"
fi
rm -rf "$tmp"
"""


class NetworkProbe(BaseAgent):
    @staticmethod
    def name() -> str:
        return "mtx-network-probe"

    def version(self) -> str:
        return "1.0.0"

    async def setup(self, environment: BaseEnvironment) -> None:
        pass

    async def run(
        self, instruction: str, environment: BaseEnvironment, context: AgentContext
    ) -> None:
        model = os.environ.get("MTX_PROBE_MODEL", "")
        match = re.fullmatch(r"([0-9.]+):([0-9]+)", model)
        if not match:
            raise RuntimeError("MTX_PROBE_MODEL must be <ipv4>:<port>")
        script = SCRIPT.replace("@MODEL_HOST@", match[1]).replace("@MODEL_PORT@", match[2])
        result = await environment.exec(command=script, timeout_sec=1800)
        # A native Codex run on a guarded task is allowed only with web search
        # off in the config.toml Harbor writes: build that config as the bench
        # asks for it (bench/harness/models.py CODEX_AKS), put it in the
        # container, and read it there.
        web_off = "no"
        try:
            from harness.models import CODEX_AKS
            from harbor.agents.installed.codex import Codex

            codex = Codex(
                logs_dir=self.logs_dir,
                model_name="openai/probe",
                version="0.0.0",
                **{k: (v == "true" if v in ("true", "false") else v) for k, v in CODEX_AKS.items()},
            )
            import toml

            config = toml.dumps(codex._build_effective_config())
            put = await environment.exec(
                command=f"cat > /tmp/codex-config.toml <<'EOF'\n{config}EOF\ngrep -qx 'web_search = \"disabled\"' /tmp/codex-config.toml && echo ok"
            )
            if (put.stdout or "").strip().endswith("ok"):
                web_off = "yes"
        except Exception as error:  # the check fails closed
            (self.logs_dir / "codex-config-error.txt").write_text(str(error))
        result.stdout = (result.stdout or "") + f"\nreach codex-web-search-off {web_off}\n"
        reached = {
            m[0]: m[1]
            for m in re.findall(r"^reach (\S+) (yes|no|skipped)$", result.stdout or "", re.M)
        }
        record = {
            "return_code": result.return_code,
            "reached": reached,
            "stderr": (result.stderr or "")[-2000:],
        }
        (self.logs_dir / "network.json").write_text(json.dumps(record, indent=2))
