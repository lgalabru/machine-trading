"""Harbor agents for the anti-cheat checks: each installs a bot from bench/checks/
as /app/bot.

    PYTHONPATH=bench harbor run -p bench/tasks/flash-crash-spcx -o jobs \
        -a checks.cheater_agent:ComponentCheaterAgent
    PYTHONPATH=bench harbor run -p bench/tasks/flash-crash-spcx -o jobs \
        -a checks.cheater_agent:CheaterAgent

Run from the repository root. A sound task scores each 0, and its grade
completes. The component cheater (`cheater-wasm`) tries the chain's
cheatcodes, the file system, the environment, the network, the clock and
memory past the cap from inside the sandbox: its log must show every attempt
refused. The process cheaters (`cheater`, `sneaky`) are what a bot could try
before bots were components: the verifier grades components only, so they
aren't graded at all (`not_graded` is 1).
"""

from pathlib import Path

from harbor.agents.base import BaseAgent
from harbor.environments.base import BaseEnvironment
from harbor.models.agent.context import AgentContext

CHECKS = Path(__file__).parent


class _InstallBot(BaseAgent):
    bot_dir: Path

    def version(self) -> str:
        return "1.1.0"

    async def setup(self, environment: BaseEnvironment) -> None:
        pass

    async def run(
        self, instruction: str, environment: BaseEnvironment, context: AgentContext
    ) -> None:
        await environment.exec(command="rm -rf /app/bot && mkdir -p /app/bot", user="root")
        await environment.upload_dir(source_dir=self.bot_dir, target_dir="/app/bot")
        await environment.exec(command="chmod +x /app/bot/run.sh", user="root")


class ComponentCheaterAgent(BaseAgent):
    """Cheats from inside the component sandbox: built in the starter's place,
    as an oracle solution is."""

    @staticmethod
    def name() -> str:
        return "mtx-component-cheater"

    def version(self) -> str:
        return "1.0.0"

    async def setup(self, environment: BaseEnvironment) -> None:
        pass

    async def run(
        self, instruction: str, environment: BaseEnvironment, context: AgentContext
    ) -> None:
        await environment.upload_file(
            source_path=CHECKS / "cheater-wasm" / "lib.rs", target_path="/tmp/cheater.rs"
        )
        await environment.exec(
            command="cp /tmp/cheater.rs /app/bot/src/lib.rs && just --justfile /app/bot/Justfile build",
            user="agent",
        )


class CheaterAgent(_InstallBot):
    """Cheats through the RPC guard: cheatcodes, the file system, signals."""

    bot_dir = CHECKS / "cheater"

    @staticmethod
    def name() -> str:
        return "mtx-cheater"


class SneakyAgent(_InstallBot):
    """Cheats around the RPC guard, on the surfnet's own port."""

    bot_dir = CHECKS / "sneaky"

    @staticmethod
    def name() -> str:
        return "mtx-sneaky"
