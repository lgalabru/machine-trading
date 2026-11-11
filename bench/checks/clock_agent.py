"""A Harbor agent for the clock check (bench/checks/clock/probe.sh): it reads
`time-left` the way an agent would, from a shell its own long-lived process
started, at the start and again after a minute, and writes the readings and
its own clock to clock.json in its logs.

    PYTHONPATH=bench harbor run -p bench/tasks/<task> -o jobs \
        -a checks.clock_agent:ClockProbe --agent-timeout-multiplier 0.5
"""

import json
import time

from harbor.agents.base import BaseAgent
from harbor.environments.base import BaseEnvironment
from harbor.models.agent.context import AgentContext

WAIT_SEC = 70


class ClockProbe(BaseAgent):
    @staticmethod
    def name() -> str:
        return "mtx-clock-probe"

    def version(self) -> str:
        return "1.0.0"

    async def setup(self, environment: BaseEnvironment) -> None:
        pass

    async def run(
        self, instruction: str, environment: BaseEnvironment, context: AgentContext
    ) -> None:
        started = time.time()
        result = await environment.exec(
            command=(
                'echo "env=${MTX_AGENT_TIMEOUT_SEC:-}x${MTX_AGENT_TIMEOUT_MULTIPLIER:-}"; '
                f"time-left; sleep {WAIT_SEC}; time-left"
            )
        )
        record = {
            "harbor_elapsed_sec": time.time() - started,
            "wait_sec": WAIT_SEC,
            "return_code": result.return_code,
            "stdout": result.stdout or "",
            "stderr": result.stderr or "",
        }
        (self.logs_dir / "clock.json").write_text(json.dumps(record, indent=2))
