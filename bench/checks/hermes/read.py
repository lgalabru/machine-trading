"""Read a Hermes harness check (probe.sh): the stand-in provider's request
log and the two trials Harbor kept. Exits non-zero on any failure.

    MOCK_API_KEY=... MTX_PROXY_TOKEN=... python3 -I read.py <jobs dir> <route>

The key and the token come from the environment, never the command line.
"""

import json
import os
import sys
from pathlib import Path

ALLOWED = {
    "terminal",
    "process_manage",
    "read_file",
    "write_file",
    "patch",
    "search_files",
    "todo_list",
    "delegate_task",
    "execute_code",
}
CALLS = ["terminal", "terminal", "terminal", "write_file", "read_file"]


def effort_ok(route: str, fields: dict) -> bool:
    """The effort as each provider's API takes it: the proxy turns the chat
    form into OpenRouter's."""
    if route == "openai":
        return (fields.get("reasoning") or {}).get("effort") == "high"
    if route == "anthropic":
        return "high" in json.dumps(fields.get("output_config") or fields.get("thinking") or {})
    if route == "openrouter":
        return fields.get("reasoning") == {"effort": "high"} and "reasoning_effort" not in fields
    return fields.get("reasoning_effort") == "high"


def main():
    jobs, route = Path(sys.argv[1]), sys.argv[2]
    key = os.environ["MOCK_API_KEY"]
    token = os.environ["MTX_PROXY_TOKEN"]
    logged = [json.loads(line) for line in (jobs / "requests.jsonl").read_text().splitlines()]
    requests = [r for r in logged if "unsupported_path" not in r]
    # Hermes asks a custom host about the model (Ollama's /api/show) and
    # carries on without an answer; the agent's own turns must be answered.
    other = sorted({r["unsupported_path"] for r in logged if "unsupported_path" in r})
    fails = [f"an agent turn sent to {p}" for p in other if p.rstrip("/").endswith(("/responses", "/messages", "/completions"))]
    agent_turns = [r for r in requests if r["tools"]]
    if not agent_turns:
        sys.exit("hermes probe: FAIL, the provider got no request with tools")
    offered = set().union(*(r["tools"] for r in agent_turns))
    if offered != ALLOWED:
        fails.append(f"tools offered {sorted(offered)}, not the bench's {sorted(ALLOWED)}")
    if not all(r["key_ok"] for r in requests):
        fails.append("a request reached the provider without its key")
    efforts = sorted({json.dumps(r["effort"], sort_keys=True) for r in agent_turns})
    if not all(effort_ok(route, r["effort"]) for r in agent_turns):
        fails.append(f"effort not sent as {route} takes it: {efforts}")
    if any(r["system_mentions_leak_probe"] for r in requests):
        fails.append("a system prompt mentions what a trial planted")
    inventories = [r["last_tool_result"] for r in agent_turns if r["tool_results"] == 1]
    plants = [r["last_tool_result"] for r in agent_turns if r["tool_results"] == 3]
    if len(inventories) < 2 or not plants or "PLANTED" not in plants[0]:
        fails.append(f"the script didn't run through twice ({len(inventories)} inventories, {len(plants)} plants)")
    else:
        for i, seen in enumerate(inventories):
            if "INVENTORY-END" not in seen:
                fails.append(f"trial {i + 1}'s inventory didn't finish: {seen[:200]!r}")
            if "held:" in seen:
                fails.append(f"trial {i + 1} started with: {seen}")

    trials = sorted(p.parent for p in jobs.glob("*/*/result.json"))
    if len(trials) != 2:
        fails.append(f"{len(trials)} trials, not 2")
    for trial in trials:
        agent = trial / "agent"
        result = json.loads((trial / "result.json").read_text())
        if result.get("exception_info"):
            fails.append(f"{trial.name}: {result['exception_info'].get('exception_message', '')[:300]}")
        start = agent / "hermes-home-start.txt"
        end = agent / "hermes-home-end.txt"
        if not start.exists() or start.read_text().strip():
            fails.append(f"{trial.name}: Hermes's home at the start: {start.read_text() if start.exists() else 'not recorded'}")
        if not end.exists() or "leak-probe" not in end.read_text() or "MEMORY.md" not in end.read_text():
            fails.append(f"{trial.name}: the end inventory doesn't show what the trial planted")
        environ = agent / "proc-environ.txt"
        if not environ.exists():
            fails.append(f"{trial.name}: the agent's /proc read left no record")
        else:
            seen = environ.read_text(errors="replace")
            if key in seen:
                fails.append(f"{trial.name}: the agent read the provider's key from /proc")
            if token not in seen:
                fails.append(f"{trial.name}: the /proc read didn't reach Hermes's environment (no token either)")
        trajectory = agent / "trajectory.json"
        if not trajectory.exists():
            fails.append(f"{trial.name}: no trajectory.json")
        else:
            steps = json.loads(trajectory.read_text())["steps"]
            calls = [c["function_name"] for s in steps for c in s.get("tool_calls") or []]
            if calls != CALLS:
                fails.append(f"{trial.name}: trajectory tool calls {calls}")
        usage = result.get("agent_result") or {}
        if not usage.get("n_input_tokens"):
            fails.append(f"{trial.name}: no token counts")

    if fails:
        print(f"hermes probe ({route}): FAIL")
        for f in fails:
            print(f"  {f}")
        sys.exit(1)
    print(
        f"hermes probe ({route}): pass, 2 trials, {len(requests)} provider requests, tools {sorted(offered)}, "
        f"effort {efforts}, nothing carried from trial 1 to trial 2, the agent's /proc read holds the token "
        "but not the key"
        + (f", other requests {other}" if other else "")
    )


if __name__ == "__main__":
    main()
