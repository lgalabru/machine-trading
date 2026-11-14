"""The models `just bench` runs in Hermes (models.toml), and a check of each
one's key and name before a batch spends anything.

    python3 -I models.py args <name> <proxy url>  Harbor's flags for a model, one per line
    python3 -I models.py key <name>...             the variables holding their keys
    python3 -I models.py route <name>              the model's route on the key proxy
    python3 -I models.py preflight <name>...       one 1-token request per model
    python3 -I models.py keys                      every variable that may hold a key
    python3 -I models.py scan <dir>                files under <dir> holding a key's value

MTX_MODELS names another registry (the harness check's). A name that
isn't in it is `openai/<model>` as given, or
Anthropic's for a bare name, as `just bench` has always read it. Hermes
reaches every model through key_proxy.py, with the batch's token
(MTX_PROXY_TOKEN) in place of a key. Keys are read from the environment
and never printed.
"""

import json
import os
import re
import sys
import tomllib
import urllib.error
import urllib.request
from pathlib import Path

REGISTRY = Path(os.environ.get("MTX_MODELS") or Path(__file__).with_name("models.toml"))
APIS = ("openai", "anthropic", "chat")
DEFAULTS = {
    "openai": {"api": "openai", "upstream": "https://api.openai.com/v1", "key": "OPENAI_API_KEY"},
    "anthropic": {"api": "anthropic", "upstream": "https://api.anthropic.com", "key": "ANTHROPIC_API_KEY"},
}


def entry(name: str) -> dict:
    name = name.removeprefix("hermes:")
    models = tomllib.loads(REGISTRY.read_text())
    if name in models:
        found = dict(models[name])
        missing = {"api", "upstream", "model", "key"} - found.keys()
        if missing or found["api"] not in APIS:
            sys.exit(f"{name}: models.toml needs api ({', '.join(APIS)}), upstream, model and key")
    else:
        provider, _, model = (name if "/" in name else f"anthropic/{name}").partition("/")
        if provider not in DEFAULTS:
            sys.exit(f"{name}: not in bench/harness/models.toml, and only openai/ and anthropic/ models run without an entry")
        found = {**DEFAULTS[provider], "model": model}
    found["route"] = re.sub(r"[^a-z0-9.-]", "-", name.lower())
    return found


def harbor_args(found: dict, proxy: str) -> list[str]:
    base = f"{proxy.rstrip('/')}/{found['route']}"
    if found["api"] == "chat":
        model = ["-m", f"custom/{found['model']}", "--api-format", "openai_chat_completions"]
    else:
        model = ["-m", f"{found['api']}/{found['model']}"]
    return [*model, "--base-url", base, "--api-key-name", "MTX_PROXY_TOKEN"]


def request_for(found: dict, key: str) -> urllib.request.Request:
    upstream = found["upstream"].rstrip("/")
    prompt = [{"role": "user", "content": "Say ok."}]
    if found["api"] == "anthropic":
        url = f"{upstream}/v1/messages"
        body = {"model": found["model"], "messages": prompt, "max_tokens": 1}
        headers = {"x-api-key": key, "anthropic-version": "2023-06-01"}
    else:
        url = f"{upstream}/chat/completions"
        tokens = "max_completion_tokens" if found["api"] == "openai" else "max_tokens"
        body = {"model": found["model"], "messages": prompt, tokens: 1}
        headers = {"Authorization": f"Bearer {key}"}
    return urllib.request.Request(
        url, data=json.dumps(body).encode(), headers={**headers, "Content-Type": "application/json"}, method="POST"
    )


def preflight(names: list[str]) -> int:
    failed = 0
    for name in names:
        found = entry(name)
        key = os.environ.get(found["key"], "")
        if not key:
            print(f"FAIL {name}: {found['key']} is not set")
            failed += 1
            continue
        try:
            with urllib.request.urlopen(request_for(found, key), timeout=60) as response:
                json.load(response)
            print(f"ok   {name}: {found['model']} at {found['upstream']}")
        except urllib.error.HTTPError as e:
            detail = e.read().decode(errors="replace").replace(key, "<key>")[:300]
            print(f"FAIL {name}: {found['model']}: HTTP {e.code}: {detail}")
            failed += 1
        except (urllib.error.URLError, TimeoutError, json.JSONDecodeError) as e:
            print(f"FAIL {name}: {found['model']}: {str(e).replace(key, '<key>')[:300]}")
            failed += 1
    return 1 if failed else 0


def key_names() -> list[str]:
    models = tomllib.loads(REGISTRY.read_text())
    names = [d["key"] for d in DEFAULTS.values()] + [m["key"] for m in models.values() if "key" in m]
    return list(dict.fromkeys(names))


def scan(root: Path) -> int:
    """Print every file under root that holds the value of a key set in the
    environment; the values themselves never leave this process."""
    values = {v.encode() for v in (os.environ.get(n, "") for n in key_names()) if len(v) >= 8}
    if not values:
        return 0
    hits = []
    for path in sorted(root.rglob("*")):
        if path.is_file() and not path.is_symlink():
            data = path.read_bytes()
            if any(v in data for v in values):
                hits.append(path)
    for path in hits:
        print(path)
    return 1 if hits else 0


def main():
    commands = {"args": 3, "key": 2, "route": 2, "preflight": 2, "keys": 1, "scan": 2}
    if len(sys.argv) < 2 or sys.argv[1] not in commands or len(sys.argv) < 1 + commands[sys.argv[1]]:
        sys.exit(__doc__)
    command, names = sys.argv[1], sys.argv[2:]
    if command == "args":
        print("\n".join(harbor_args(entry(names[0]), names[1])))
    elif command == "key":
        print("\n".join(dict.fromkeys(entry(n)["key"] for n in names)))
    elif command == "route":
        print(entry(names[0])["route"])
    elif command == "keys":
        print("\n".join(key_names()))
    elif command == "scan":
        sys.exit(scan(Path(names[0])))
    else:
        sys.exit(preflight(names))


if __name__ == "__main__":
    main()
