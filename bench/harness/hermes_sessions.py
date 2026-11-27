"""Keeping every trial's Hermes session, so every run has a trajectory.

Harbor's Hermes adapter exports a run's session with `hermes sessions
export --source oneshot` (then `--source cli`) once the agent is done. Hermes
v0.23.1 exports, under a source filter, only sessions marked ended
(`ended_at IS NOT NULL`, its prune filters), so a session that never closed
is dropped without a word and the trial has no trajectory: every run killed
at its time limit, and long runs whose last segment (after Hermes compacts
its context) isn't closed when the one-shot run exits. 0.12.0 lost 67 of
120 trajectories that way, 0.14.0 207 of 454.

What replaces Harbor's export (`export_command`, run in the trial's
container after the agent, as Harbor runs its own even after a timeout):
every session exported, then the main run's kept in the file Harbor builds
the trajectory from. Hermes's session database survives the agent being
killed, so a run cut off at its limit keeps its session too. Two more
copies, should that step itself not run:

- while the agent runs, a loop in the trial's container exports every
  session each SNAPSHOT_SECONDS to `/logs/agent/hermes-sessions-snapshot.jsonl`
  (the host's agent log folder), so a run killed at its limit keeps one at
  most that old;
- when Hermes exits, one export of every session to
  `/logs/agent/hermes-sessions-all.jsonl`.

An unfiltered export holds every session of the trial's fresh Hermes home,
the sub-agents' too (source "tool"). `recover_main_sessions` keeps the main
run's (sources "oneshot" and "cli"), in the order they started, and writes
them where Harbor reads the session from, when Harbor's own export came
back empty.
"""

from __future__ import annotations

import json
from pathlib import Path

SNAPSHOT_SECONDS = 60
SESSION_FILE = "hermes-session.jsonl"
ALL_FILE = "hermes-sessions-all.jsonl"
SNAPSHOT_FILE = "hermes-sessions-snapshot.jsonl"
# A `chat -q` run is stored under "oneshot" (Hermes since 2026-09; "cli"
# before); its sub-agents under "tool".
MAIN_SOURCES = {"oneshot", "cli"}
HERMES_HOME = "/tmp/hermes"


def wrap_run_command(command: str) -> str:
    """The agent's run command, with the snapshot loop beside it and an
    unfiltered export after it. The command's exit status is kept."""
    export = f'HERMES_HOME={HERMES_HOME} hermes sessions export'
    snapshot = (
        f"while sleep {SNAPSHOT_SECONDS}; do "
        f"{export} /logs/agent/.{SNAPSHOT_FILE}.tmp > /dev/null 2>&1 "
        f"&& mv -f /logs/agent/.{SNAPSHOT_FILE}.tmp /logs/agent/{SNAPSHOT_FILE}; done"
    )
    # The loop's output goes nowhere: its `sleep` outlives a `kill` of the
    # loop, and holding the command's stdout open would keep Harbor waiting
    # on the run until it ends.
    return (
        'export PATH="$HOME/.local/bin:$PATH"; '
        f"( {snapshot} ) > /dev/null 2>&1 < /dev/null & snapshots=$!; "
        f"( {command} ); status=$?; "
        "kill $snapshots 2> /dev/null; "
        f"{export} /logs/agent/{ALL_FILE} > /dev/null 2>&1; "
        "exit $status"
    )


# Run with the Python Hermes is installed with (the agent image may have no
# other): keep the main run's sessions, oldest
# first, from the unfiltered export (else the latest snapshot).
_FILTER = """
import json, sys
def main_sessions(path):
    out = []
    try:
        lines = open(path, errors="replace").read().splitlines()
    except OSError:
        return out
    for line in lines:
        try:
            s = json.loads(line)
        except ValueError:
            continue
        if isinstance(s, dict) and s.get("source") in ("oneshot", "cli"):
            out.append(s)
    out.sort(key=lambda s: (s.get("started_at") or 0, str(s.get("id"))))
    return out
for name in sys.argv[2:]:
    kept = main_sessions(name)
    if kept:
        with open(sys.argv[1], "w") as f:
            f.write("".join(json.dumps(s) + "\\n" for s in kept))
        break
"""


def export_command() -> str:
    """What replaces Harbor's export after the run (which it runs even when
    the agent timed out): every session exported, the main run's kept in
    SESSION_FILE. The first line goes to stdout, as Harbor's own export
    printed it, for the session's ID."""
    export = f"HERMES_HOME={HERMES_HOME} hermes sessions export"
    agent = "/logs/agent"
    return (
        'export PATH="$HOME/.local/bin:$PATH"; '
        f"{export} {agent}/{ALL_FILE} > /dev/null 2>&1; "
        # Hermes's installer brings its own Python, in its home's tools (its
        # `hermes` is a shell wrapper); the image may have none on the PATH.
        f'py="$(ls -d {HERMES_HOME}/tools/python-*/bin/python3 2> /dev/null | head -n 1)"; '
        '[ -n "$py" ] || py="$(command -v python3)"; '
        f'"$py" - {agent}/{SESSION_FILE} {agent}/{ALL_FILE} {agent}/{SNAPSHOT_FILE} << \'MTX_FILTER\'\n'
        f"{_FILTER}\nMTX_FILTER\n"
        f"head -n 1 {agent}/{SESSION_FILE} || true"
    )


def is_export_command(command: str) -> bool:
    """Harbor's source-filtered export after the run."""
    return "sessions export /logs/agent/hermes-session.jsonl" in command


def is_run_command(command: str) -> bool:
    """Harbor's command that runs the agent (not its install or export)."""
    return "hermes --yolo chat" in command


def main_sessions(text: str) -> list[dict]:
    """The main run's sessions in an unfiltered export, oldest first."""
    sessions = []
    for line in text.splitlines():
        try:
            session = json.loads(line)
        except ValueError:
            continue
        if isinstance(session, dict) and session.get("source") in MAIN_SOURCES:
            sessions.append(session)
    sessions.sort(key=lambda s: (s.get("started_at") or 0, str(s.get("id"))))
    return sessions


def recover_main_sessions(logs_dir: Path) -> str | None:
    """Write the main run's sessions to SESSION_FILE when Harbor's export is
    empty, from the final export or else the latest snapshot. Returns the
    file it used, or None when there was nothing to do or nothing to use."""
    target = logs_dir / SESSION_FILE
    if target.exists() and target.stat().st_size > 0:
        return None
    for name in (ALL_FILE, SNAPSHOT_FILE):
        source = logs_dir / name
        if not source.exists():
            continue
        sessions = main_sessions(source.read_text(errors="replace"))
        if sessions:
            target.write_text("".join(json.dumps(s) + "\n" for s in sessions))
            return name
    return None
