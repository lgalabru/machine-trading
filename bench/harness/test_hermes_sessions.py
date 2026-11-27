"""Every trial keeps its Hermes session (hermes_sessions.py).

    python3 -I bench/harness/test_hermes_sessions.py
"""

import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import hermes_sessions as hs  # noqa: E402


def session(sid, source, started, ended=None, n=2):
    return {"id": sid, "source": source, "started_at": started, "ended_at": ended,
            "messages": [{"role": "user", "content": f"{sid} {i}"} for i in range(n)]}


class Recover(unittest.TestCase):
    def write(self, folder, name, sessions):
        (folder / name).write_text("".join(json.dumps(s) + "\n" for s in sessions))

    def test_an_unclosed_run_and_its_continuation_are_kept_in_order_without_subagents(self):
        with tempfile.TemporaryDirectory() as tmp:
            logs = Path(tmp)
            (logs / hs.SESSION_FILE).write_text("")  # Harbor's export, empty
            self.write(logs, hs.ALL_FILE, [
                session("b", "oneshot", 200.0),           # continuation, never closed
                session("t", "tool", 150.0, 160.0),       # a sub-agent
                session("a", "oneshot", 100.0, 190.0),    # first segment
            ])
            self.assertEqual(hs.recover_main_sessions(logs), hs.ALL_FILE)
            kept = [json.loads(l)["id"] for l in (logs / hs.SESSION_FILE).read_text().splitlines()]
            self.assertEqual(kept, ["a", "b"])

    def test_a_run_killed_at_its_limit_falls_back_to_the_snapshot(self):
        with tempfile.TemporaryDirectory() as tmp:
            logs = Path(tmp)
            self.write(logs, hs.SNAPSHOT_FILE, [session("a", "oneshot", 1.0)])
            self.assertEqual(hs.recover_main_sessions(logs), hs.SNAPSHOT_FILE)
            self.assertIn('"a"', (logs / hs.SESSION_FILE).read_text())

    def test_harbors_own_export_wins_and_nothing_usable_does_nothing(self):
        with tempfile.TemporaryDirectory() as tmp:
            logs = Path(tmp)
            self.assertIsNone(hs.recover_main_sessions(logs))
            self.write(logs, hs.ALL_FILE, [session("t", "tool", 1.0)])
            self.assertIsNone(hs.recover_main_sessions(logs))
            (logs / hs.SESSION_FILE).write_text('{"id": "harbor"}\n')
            self.write(logs, hs.ALL_FILE, [session("a", "oneshot", 1.0)])
            self.assertIsNone(hs.recover_main_sessions(logs))
            self.assertEqual((logs / hs.SESSION_FILE).read_text(), '{"id": "harbor"}\n')

    def test_garbage_lines_are_skipped(self):
        self.assertEqual([s["id"] for s in hs.main_sessions('not json\n[1]\n' + json.dumps(session("a", "cli", 1.0)))],
                         ["a"])


class Wrap(unittest.TestCase):
    def test_only_the_run_command_is_wrapped(self):
        self.assertTrue(hs.is_run_command('export PATH=x && hermes --yolo chat -q "$I" 2>&1 | tee /logs/agent/hermes.txt'))
        self.assertFalse(hs.is_run_command("hermes sessions export /logs/agent/hermes-session.jsonl --source oneshot"))

    def test_the_wrapped_command_keeps_its_exit_status_and_exports_after(self):
        # A fake `hermes` that records its exports, and a run that exits 7.
        with tempfile.TemporaryDirectory() as tmp:
            bin_dir = Path(tmp) / "bin"; bin_dir.mkdir()
            logs = Path(tmp) / "logs"; logs.mkdir()
            fake = bin_dir / "hermes"
            fake.write_text(f'#!/bin/sh\necho "$@" >> {tmp}/calls\n')
            fake.chmod(0o755)
            command = hs.wrap_run_command("exit 7").replace("/logs/agent", str(logs))
            done = subprocess.run(["bash", "-c", command], env={"PATH": f"{bin_dir}:/usr/bin:/bin", "HOME": tmp},
                                  capture_output=True, text=True, timeout=30)
            self.assertEqual(done.returncode, 7, done.stderr)
            self.assertIn(f"sessions export {logs}/{hs.ALL_FILE}", Path(tmp, "calls").read_text())


class ExportInTheContainer(unittest.TestCase):
    def test_harbors_export_is_replaced_by_every_main_session_in_order(self):
        self.assertTrue(hs.is_export_command(
            "hermes sessions export /logs/agent/hermes-session.jsonl --source oneshot 2>/dev/null"))
        with tempfile.TemporaryDirectory() as tmp:
            bin_dir = Path(tmp) / "bin"; bin_dir.mkdir()
            logs = Path(tmp) / "logs"; logs.mkdir()
            sessions = [session("b", "oneshot", 2.0), session("t", "tool", 1.5), session("a", "oneshot", 1.0, 1.9)]
            # A fake `hermes` (launched by a Python, as the real one is) whose
            # export writes every session, open ones too.
            fake = bin_dir / "hermes"
            fake.write_text("#!/usr/bin/env python3\nimport json, sys\n"
                            f"open(sys.argv[3], 'w').write({json.dumps(''.join(json.dumps(x) + chr(10) for x in sessions))})\n")
            fake.chmod(0o755)
            command = hs.export_command().replace("/logs/agent", str(logs))
            done = subprocess.run(["bash", "-c", command], env={"PATH": f"{bin_dir}:/usr/bin:/bin", "HOME": tmp},
                                  capture_output=True, text=True, timeout=30)
            self.assertEqual(done.returncode, 0, done.stderr)
            kept = [json.loads(l)["id"] for l in (logs / hs.SESSION_FILE).read_text().splitlines()]
            self.assertEqual(kept, ["a", "b"])
            self.assertIn('"id": "a"', done.stdout)  # the first line, as Harbor reads it


if __name__ == "__main__":
    unittest.main()
