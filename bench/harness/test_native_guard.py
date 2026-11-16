"""`just bench` refuses a native harness on a guarded task.

    python3 -I bench/harness/test_native_guard.py
"""

import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
import models  # noqa: E402


def tasks_with(*froms: str) -> Path:
    root = Path(tempfile.mkdtemp())
    for i, image in enumerate(froms):
        env = root / f"task{i}" / "environment"
        env.mkdir(parents=True)
        (env / "Dockerfile").write_text(f"# c\nFROM ghcr.io/x/machine-trading:{image}\nENV A=b\n")
    return root


class NativeGuard(unittest.TestCase):
    def test_a_native_route_on_a_guarded_task_is_refused(self):
        tasks = tasks_with("0.12.0@sha256:ab", "0.12.0-agent@sha256:cd")
        refusal = models.native_guard(tasks, ["native:anthropic/claude-opus-5-5", "claude-sonnet-5-5"])
        self.assertIn("native:anthropic/claude-opus-5-5", refusal)
        self.assertIn("task1", refusal)
        self.assertNotIn("task0", refusal)

    def test_hermes_routes_run_guarded_tasks(self):
        self.assertIsNone(models.native_guard(tasks_with("0.12.0-agent"), ["claude-sonnet-5-5", "openai/gpt-x"]))

    def test_native_routes_run_unguarded_tasks(self):
        self.assertIsNone(models.native_guard(tasks_with("0.12.0@sha256:ab"), ["native:openai/gpt-x"]))


if __name__ == "__main__":
    unittest.main()
