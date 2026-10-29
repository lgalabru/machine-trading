"""Harbor agents for the anti-cheat checks: each installs a bot from bench/checks/
as /app/bot, without building anything.

    PYTHONPATH=bench harbor run -p bench/tasks/phoenix-flash-crash -o jobs \
        -a checks.cheater_agent:CheaterAgent
    PYTHONPATH=bench harbor run -p bench/tasks/phoenix-flash-crash -o jobs \
        -a checks.cheater_agent:SneakyAgent

Run from the repository root. A sound task scores both 0: the cheater is
disqualified by the guard's refusals, and the sneaky bot's direct calls to the
surfnet are refused by the cheatcode lock, so it gains nothing and the grade
completes normally.
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


class CheaterAgent(_InstallBot):
    """Cheats through the RPC guard: cheatcodes, the file system, signals."""

    bot_dir = CHECKS / "cheater"

    @staticmethod
    def name() -> str:
        return "mt-cheater"


class SneakyAgent(_InstallBot):
    """Cheats around the RPC guard, on the surfnet's own port."""

    bot_dir = CHECKS / "sneaky"

    @staticmethod
    def name() -> str:
        return "mt-sneaky"
