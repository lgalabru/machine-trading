"""Finish a batch's Hermes trials once Harbor is done with them: every trial
gets its agent trajectory and its model calls' cost.

Both used to be left to Harbor's run of the agent adapter, and both raced
with it. Harbor builds a Hermes trajectory while the run streams, or in the
adapter's post-run hook, and calls that hook only when the trial's context
is still empty, which its Hermes adapter's token counting during the run
often prevents; it then copies the agent's logs back over the host's copy.
0.12.0 lost 67 of 120 trajectories, 0.14.0 207 of 454, and no trial got the
cost the key proxy logged.

What every trial does have, after Harbor: `agent/hermes-session.jsonl`
(written in the container by the adapter's export, hermes_sessions.py, which
Harbor runs even after a timeout) and the batch's `usage.jsonl` (the key
proxy's log, usage_log.py). From them, for each trial:

- `agent/trajectory.json`, if missing, with Harbor's own Hermes converter;
- `result.json`'s `agent_result` tokens and `cost_usd`, from the proxy's
  log, the original kept as `result.json.orig`.

Run with Harbor's Python (its converter): `just bench` does, after its runs.

    <harbor's python> -I bench/harness/finish_trials.py <batch dir>
"""

from __future__ import annotations

import argparse
import json
import shutil
import sys
import uuid
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import hermes_sessions  # noqa: E402
import usage_log  # noqa: E402


def trajectory_of(session_text: str, model_name: str | None, version: str | None):
    """Harbor's ATIF trajectory for a Hermes session export, or None."""
    from harbor.agents.installed.hermes import Hermes

    # The converter reads its arguments, the model's name and the agent's
    # version: build it without the agent's constructor (which wants a whole
    # trial) and give it those two.
    converter = Hermes.__new__(Hermes)
    converter.model_name = model_name
    converter._version = version
    return Hermes._convert_hermes_session_to_atif(converter, session_text, str(uuid.uuid4()))


def without_orphan_results(session_text: str) -> str:
    """The session with every tool result that answers no call the model
    made dropped. A run cut off mid-call leaves one with an empty call ID,
    and Harbor's converter refuses the whole session for it (a pydantic
    ValidationError it swallows, so the trial had no trajectory)."""
    out = []
    for line in session_text.splitlines():
        try:
            session = json.loads(line)
        except ValueError:
            continue
        if isinstance(session, dict) and isinstance(session.get("messages"), list):
            calls: set[str] = set()
            kept = []
            for message in session["messages"]:
                if not isinstance(message, dict):
                    continue
                for call in message.get("tool_calls") or []:
                    if isinstance(call, dict) and call.get("id"):
                        calls.add(call["id"])
                if message.get("role") == "tool" and message.get("tool_call_id") not in calls:
                    continue
                kept.append(message)
            session = {**session, "messages": kept}
        out.append(json.dumps(session))
    return "\n".join(out) + "\n"


def finish_trial(trial: Path, usage: Path | None) -> dict:
    """Fill in one trial; what it did, for the summary."""
    agent = trial / "agent"
    done = {"trajectory": "kept", "cost": "none"}
    if not (agent / "trajectory.json").exists():
        hermes_sessions.recover_main_sessions(agent)
        session = agent / hermes_sessions.SESSION_FILE
        result = json.loads((trial / "result.json").read_text())
        model_name = ((result.get("config") or {}).get("agent") or {}).get("model_name")
        version = (result.get("agent_info") or {}).get("version")
        trajectory = None
        if session.exists() and session.stat().st_size:
            text = session.read_text(errors="replace")
            try:
                trajectory = trajectory_of(text, model_name, version)
            except ValueError:  # pydantic's ValidationError is one
                trajectory = trajectory_of(without_orphan_results(text), model_name, version)
        if trajectory is None:
            done["trajectory"] = "MISSING"
        else:
            (agent / "trajectory.json").write_text(json.dumps(trajectory.to_json_dict(), indent=2))
            done["trajectory"] = "built"
    total = usage_log.totals(usage, trial.name) if usage else None
    if total is not None:
        result_path = trial / "result.json"
        result = json.loads(result_path.read_text())
        context = result.get("agent_result") or {}
        context.update({
            "n_input_tokens": total["input_tokens"],
            "n_cache_tokens": total["cached_tokens"],
            "n_output_tokens": total["output_tokens"],
            "cost_usd": total["cost_usd"],
        })
        result["agent_result"] = context
        original = trial / "result.json.orig"
        if not original.exists():
            shutil.copy2(result_path, original)
        result_path.write_text(json.dumps(result, indent=2))
        (agent / "usage.json").write_text(json.dumps(total, indent=2) + "\n")
        done["cost"] = "unpriced" if total["cost_usd"] is None else f"${total['cost_usd']:.4f}"
    return done


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("batch", type=Path)
    args = parser.parse_args()
    usage = args.batch / "usage.jsonl"
    missing = 0
    for result in sorted(args.batch.glob("*/*__*/result.json")):
        trial = result.parent
        done = finish_trial(trial, usage if usage.exists() else None)
        missing += done["trajectory"] == "MISSING"
        print(f"{trial.name}: trajectory {done['trajectory']}, cost {done['cost']}")
    if missing:
        print(f"WARNING: {missing} trial(s) still have no agent trajectory", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
