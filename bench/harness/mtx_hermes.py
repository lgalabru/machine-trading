"""Harbor's Hermes agent, set up the same for every trial and every model.

Harbor 0.25.0's adapter (harbor.agents.installed.hermes) runs Hermes with
its full CLI toolset, at Hermes's default reasoning effort, and with the
bundled skill catalog seeded into its home. This class keeps the adapter
and changes what a benchmark needs to compare models:

- Install: the installer at a fixed commit, checking out the pinned
  release (`version`) by its commit (PINNED), with no browser; the
  checkout is checked against that commit. hermes-agent v0.23.1's own
  installer leaves no dependency environment ("run `hermes pm repair`"),
  and Harbor checks the install with `hermes version`, a subcommand
  v0.23.1 dropped for `hermes --version`.
- Subagents: delegation.max_concurrent_children pinned to 4 (limits.py),
  so a trial has at most 4 background batches of 4.
- Tools: a shell, files, a todo list, subagents and code execution, the
  tools Claude Code and Codex give a model, all listed from the first turn
  (no tool search). No web, browser, vision, memory, skills or session
  search: no model gets help from outside its trial.
- Isolation: every trial starts with no memory and no skills. Hermes's
  home is /tmp/hermes in the trial's own container; the image must hold
  none, bundled skills are never seeded, and a trial whose home holds a
  memory or a skill when it starts fails instead of running. What the home
  holds before and after is kept with the trial (hermes-home-*.txt).
- Reasoning effort: `--effort` (or `--ak reasoning_effort=`) sets Hermes's
  `agent.reasoning_effort`, as it does Claude Code's and Codex's.
- Usage: token counts and cost from the session Hermes keeps, when its
  messages carry none.
- Keys: none in the trial. Hermes reaches its model through the bench's
  key proxy (key_proxy.py, on the host) with its route's token
  (`--api-key-name MTX_PROXY_TOKEN`); the proxy puts the key on upstream.
  Any process in the container can read another's environment under
  /proc, so a key in the container is a key the agent can print. This
  class refuses to run any other way.

`just bench` runs `hermes:<model>` with this class (`-a mtx_hermes:Hermes`,
bench/harness on PYTHONPATH).
"""

import json
import sys
from pathlib import Path
from typing import Any, override

import yaml
from harbor.agents.installed.hermes import Hermes as HarborHermes
from harbor.environments.base import BaseEnvironment
from harbor.models.agent.context import AgentContext

sys.path.insert(0, str(Path(__file__).parent))
from limits import DELEGATION_CHILDREN  # noqa: E402

# Harbor's adapter writes its config, runs and exports sessions here.
HERMES_HOME = "/tmp/hermes"

# install.sh from hermes-agent's main at a fixed commit: it checks out the
# release passed as `version` (--branch). Bump with `hermes` in the Justfile.
INSTALLER = (
    "https://raw.githubusercontent.com/NousResearch/hermes-agent/"
    "882cbc99e44d7837cd3cf0229f0352235effbf25/scripts/install.sh"
)

# The variable holding the route's token for the key proxy.
PROXY_TOKEN = "MTX_PROXY_TOKEN"

# Releases by commit: a tag can move, a commit can't.
PINNED = {"v0.23.1": "3b421578588243f920886e6fe79a6faa87ea026e"}

TOOLSETS = ["terminal", "file", "todo", "delegation", "code_execution"]

# Named off too, in case a later release adds one of them to the toolsets
# above.
DISABLED_TOOLSETS = [
    "web",
    "search",
    "browser",
    "vision",
    "image_gen",
    "video_gen",
    "tts",
    "memory",
    "skills",
    "session_search",
    "cronjob",
    "clarify",
    "computer_use",
    "connections",
    "x_search",
]

# A directory that doesn't exist: Hermes seeds its home from the bundled
# and optional skill catalogs, and seeds nothing from a missing one.
NO_SKILLS = "/tmp/hermes-no-skills"

# Where a memory or a skill would come from: Hermes's stores, its default
# home's, and the shared skill folders other agents keep.
INVENTORY = (
    f"for d in {HERMES_HOME}/memories {HERMES_HOME}/skills $HOME/.hermes/memories "
    "$HOME/.hermes/skills $HOME/.agents/skills $HOME/.claude/skills; do "
    '[ -e "$d" ] && find "$d" -mindepth 1 | sed "s|^|$d: |" | sort; '
    "done; true"
)

class Hermes(HarborHermes):
    capabilities = HarborHermes.capabilities.model_copy(
        update={"reasoning_effort": True}
    )

    def __init__(self, *args: Any, **kwargs: Any):
        kwargs.setdefault("toolsets", ",".join(TOOLSETS))
        super().__init__(*args, **kwargs)
        if self._model_api_key_name != PROXY_TOKEN or not self._base_url:
            raise ValueError(
                f"Hermes runs only through the key proxy: --base-url <proxy>/<route> --api-key-name {PROXY_TOKEN}"
            )

    @override
    def get_version_command(self) -> str | None:
        return 'export PATH="$HOME/.local/bin:$PATH"; hermes --version'

    @override
    async def exec_as_agent(
        self,
        environment: BaseEnvironment,
        command: str,
        env: dict[str, str] | None = None,
        cwd: str | None = None,
        timeout_sec: int | None = None,
    ) -> Any:
        isolated = {"HERMES_BUNDLED_SKILLS": NO_SKILLS, "HERMES_OPTIONAL_SKILLS": NO_SKILLS}
        return await super().exec_as_agent(
            environment,
            command,
            env={**isolated, **(env or {})},
            cwd=cwd,
            timeout_sec=timeout_sec,
        )

    @override
    async def install(self, environment: BaseEnvironment) -> None:
        await self.ensure_system_dependencies(
            environment, ("curl", "git", "ripgrep", "xz")
        )
        commit = PINNED.get(self._version or "")
        if commit is None:
            raise ValueError(f"Hermes {self._version!r} has no commit in mtx_hermes.PINNED")
        await self.exec_as_agent(
            environment,
            command=(
                "set -euo pipefail; "
                f'if [ -e {HERMES_HOME} ] || [ -e "$HOME/.hermes" ]; then '
                'echo "the image already holds a Hermes home"; exit 1; fi; '
                f"mkdir -p {HERMES_HOME} && touch {HERMES_HOME}/.no-bundled-skills && "
                f"curl -fsSL {INSTALLER} | bash -s -- --skip-setup --skip-browser "
                f"--skip-computer-use --hermes-home {HERMES_HOME} --commit {commit} && "
                f'[ "$(git -C {HERMES_HOME}/hermes-agent rev-parse HEAD)" = {commit} ] && '
                'export PATH="$HOME/.local/bin:$PATH" && '
                f"export HERMES_HOME={HERMES_HOME} && "
                # A fresh home has no dependency environment until repaired;
                # `--version` refuses to run without one.
                "hermes pm repair && hermes --version"
            ),
            env={"HERMES_HOME": HERMES_HOME},
        )

    @override
    def _build_config_yaml(  # type: ignore[override]
        self,
        model: str,
        max_turns: int | None = None,
        *,
        connection: Any = None,
    ) -> str:
        config = yaml.safe_load(
            HarborHermes._build_config_yaml(model, max_turns, connection=connection)
        )
        agent = config.setdefault("agent", {})
        agent["disabled_toolsets"] = DISABLED_TOOLSETS
        if self.options.reasoning_effort is not None:
            agent["reasoning_effort"] = self.options.reasoning_effort
        config["toolsets"] = TOOLSETS
        # Every tool in the model's tool list from the start, as the other
        # harnesses give them, not behind Hermes's tool search.
        config["tools"] = {"tool_search": {"enabled": "off"}}
        config["platform_toolsets"] = {"cli": TOOLSETS}
        config["memory"] = {
            "memory_enabled": False,
            "user_profile_enabled": False,
            "nudge_interval": 0,
        }
        # Subagents bounded and the same for every model: Hermes's default
        # (10 background batches of 10) would let one trial open ~100 model
        # calls at once. limits.py sizes the key proxy's caps from this.
        config["delegation"] = {
            **config.get("delegation", {}),
            "max_concurrent_children": DELEGATION_CHILDREN,
            "max_spawn_depth": 1,
        }
        config["skills"] = {
            "creation_nudge_interval": 0,
            "external_dirs": [],
            "project_discovery": False,
        }
        return yaml.dump(config, default_flow_style=False)

    async def _inventory(self, environment: BaseEnvironment, phase: str) -> str:
        result = await self.exec_as_agent(
            environment,
            command=f"({INVENTORY}) | tee /logs/agent/hermes-home-{phase}.txt",
            env={"HERMES_HOME": HERMES_HOME},
            timeout_sec=30,
        )
        return result.stdout or ""

    @override
    async def run(
        self,
        instruction: str,
        environment: BaseEnvironment,
        context: AgentContext,
    ) -> None:
        if not self._resume:
            # The installer makes these; nothing in them is the trial's.
            await self.exec_as_agent(
                environment,
                command=(
                    f"rm -rf {HERMES_HOME}/memories {HERMES_HOME}/skills "
                    f"{HERMES_HOME}/sessions {HERMES_HOME}/state.db "
                    f"{HERMES_HOME}/state.db-wal {HERMES_HOME}/state.db-shm && "
                    f"mkdir -p {HERMES_HOME}/memories {HERMES_HOME}/skills {HERMES_HOME}/sessions"
                ),
                timeout_sec=30,
            )
            held = await self._inventory(environment, "start")
            if held.strip():
                raise RuntimeError(
                    f"Hermes's home holds memories or skills before the trial:\n{held}"
                )
        try:
            await super().run(instruction, environment, context)
        finally:
            await self._inventory(environment, "end")

    @override
    def populate_context_post_run(self, context: AgentContext) -> None:
        super().populate_context_post_run(context)
        if context.n_input_tokens:
            return
        session_path = self.logs_dir / "hermes-session.jsonl"
        if not session_path.exists():
            return
        for line in session_path.read_text().splitlines():
            try:
                session = json.loads(line)
            except json.JSONDecodeError:
                continue
            if not isinstance(session, dict) or not session.get("input_tokens"):
                continue
            context.n_input_tokens = session["input_tokens"]
            context.n_output_tokens = session.get("output_tokens")
            context.n_cache_tokens = session.get("cache_read_tokens")
            cost = session.get("actual_cost_usd") or session.get("estimated_cost_usd")
            if cost:
                context.cost_usd = cost
            return
