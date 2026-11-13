"""Harbor's Hermes agent, for hermes-agent releases without `hermes version`.

Harbor 0.25.0's adapter checks the install with `hermes version`, a
subcommand hermes-agent has since dropped for `hermes --version` (v0.23.1
answers "'version' is not a `hermes` command"), and `--version` needs the
dependency environment `hermes pm repair` prepares for HERMES_HOME first.
Everything else is Harbor's adapter as is. `just bench` runs `hermes:<model>` with this class
(`-a mtx_hermes:Hermes`, bench/harness on PYTHONPATH); drop it once
Harbor's adapter calls `--version`.
"""

from typing import override

from harbor.agents.installed.hermes import Hermes as HarborHermes
from harbor.environments.base import BaseEnvironment


class Hermes(HarborHermes):
    @override
    def get_version_command(self) -> str | None:
        return 'export PATH="$HOME/.local/bin:$PATH"; hermes --version'

    @override
    async def install(self, environment: BaseEnvironment) -> None:
        await self.ensure_system_dependencies(
            environment, ("curl", "git", "ripgrep", "xz")
        )
        branch_flag = f" --branch {self._version}" if self._version else ""
        # main's installer, as Harbor's adapter runs it: it checks out the
        # pinned tag (--branch). v0.23.1's own installer leaves no dependency
        # environment for HERMES_HOME ("run `hermes pm repair`").
        await self.exec_as_agent(
            environment,
            command=(
                "set -euo pipefail; "
                "curl -fsSL https://raw.githubusercontent.com/NousResearch/hermes-agent/main/scripts/install.sh"
                f" | bash -s -- --skip-setup{branch_flag} && "
                'export PATH="$HOME/.local/bin:$PATH" && '
                'export HERMES_HOME="${HERMES_HOME:-/tmp/hermes}" && '
                'mkdir -p "$HERMES_HOME" "$HERMES_HOME/sessions" "$HERMES_HOME/skills" "$HERMES_HOME/memories" && '
                # A fresh HERMES_HOME has no dependency environment until
                # repaired; `--version` refuses to run without one.
                "hermes pm repair && hermes --version"
            ),
        )
