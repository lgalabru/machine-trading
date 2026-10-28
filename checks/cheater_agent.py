"""Harbor agent for the anti-cheat check: installs checks/cheater as the bot.

    PYTHONPATH=. harbor run -p tasks/phoenix-flash-crash -o jobs \
        -a checks.cheater_agent:CheaterAgent

Run from the repository root. A sound task scores
this 0 with the guard's refusals in the verifier report.
"""

from pathlib import Path

from harbor.agents.base import BaseAgent
from harbor.environments.base import BaseEnvironment
from harbor.models.agent.context import AgentContext

CHEATER_DIR = Path(__file__).parent / "cheater"


class CheaterAgent(BaseAgent):
    @staticmethod
    def name() -> str:
        return "ott-cheater"

    def version(self) -> str:
        return "1.0.0"

    async def setup(self, environment: BaseEnvironment) -> None:
        pass

    async def run(
        self, instruction: str, environment: BaseEnvironment, context: AgentContext
    ) -> None:
        await environment.exec(command="rm -rf /app/bot && mkdir -p /app/bot", user="root")
        await environment.upload_dir(source_dir=CHEATER_DIR, target_dir="/app/bot")
        await environment.exec(command="chmod +x /app/bot/run.sh", user="root")
