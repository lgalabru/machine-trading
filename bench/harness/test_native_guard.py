"""`just bench` refuses a native harness on a guarded task, unless MTX_UNGUARDED.

    python3 -I bench/harness/test_native_guard.py
"""

import json
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
    def test_native_harnesses_are_refused_on_a_guarded_task(self):
        tasks = tasks_with("0.12.0@sha256:ab", "0.12.0-agent@sha256:cd")
        for name in ("native:openai/gpt-x", "native:anthropic/claude-opus-5-5"):
            refusal = models.native_guard(tasks, [name, "claude-sonnet-5-5"])
            self.assertIn(name, refusal)
            self.assertIn("task1", refusal)
            self.assertNotIn("task0", refusal)

    def test_hermes_routes_run_guarded_tasks(self):
        self.assertIsNone(models.native_guard(tasks_with("0.12.0-agent"), ["claude-sonnet-5-5", "openai/gpt-x"]))

    def test_native_routes_run_unguarded_tasks(self):
        tasks = tasks_with("0.12.0@sha256:ab")
        self.assertIsNone(models.native_guard(tasks, ["native:openai/gpt-x", "native:anthropic/claude-opus-5-5"]))

    def test_unguarded_allows_a_native_codex_on_a_guarded_task_and_stamps_the_batch(self):
        tasks = tasks_with("0.12.0-agent")
        self.assertIsNone(models.native_guard(tasks, ["native:openai/gpt-x"], unguarded=True))
        batch = Path(tempfile.mkdtemp())
        (batch / "batch.json").write_text('{"models": ["native:openai/gpt-x"]}')
        models.stamp_unguarded(batch)
        self.assertTrue((batch / ".unguarded").is_file())
        plan = json.loads((batch / "batch.json").read_text())
        self.assertIs(plan["guarded"], False)
        self.assertEqual(plan["models"], ["native:openai/gpt-x"])


if __name__ == "__main__":
    unittest.main()
