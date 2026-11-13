"""The models `just bench` runs in Hermes (models.toml), and a check of each
one's key and name before a batch spends anything.

    python3 -I models.py args <name>         Harbor's flags for a model, one per line
    python3 -I models.py key <name>          the environment variable holding its key
    python3 -I models.py preflight <name>... one 1-token request per model

A name that isn't in models.toml is `openai/<model>` as given, or
Anthropic's for a bare name, as `just bench` has always read it. Keys are
read from the environment and never printed.
"""

import json
import os
import sys
import tomllib
import urllib.error
import urllib.request
from pathlib import Path

REGISTRY = Path(__file__).with_name("models.toml")
NATIVE_KEYS = {"openai": "OPENAI_API_KEY", "anthropic": "ANTHROPIC_API_KEY", "openrouter": "OPENROUTER_API_KEY"}


def entry(name: str) -> dict:
    name = name.removeprefix("hermes:")
    models = tomllib.loads(REGISTRY.read_text())
    if name in models:
        found = dict(models[name])
        provider = found["model"].split("/", 1)[0]
        if provider not in NATIVE_KEYS and not (found.get("base_url") and found.get("api_format")):
            sys.exit(f"{name}: provider {provider} needs base_url and api_format in models.toml")
        return found
    model = name if "/" in name else f"anthropic/{name}"
    provider = model.split("/", 1)[0]
    if provider not in ("openai", "anthropic"):
        sys.exit(f"{name}: not in bench/harness/models.toml, and only openai/ and anthropic/ models run without an entry")
    return {"model": model, "key": NATIVE_KEYS[provider]}


def harbor_args(found: dict) -> list[str]:
    args = ["-m", found["model"], "--api-key-name", found["key"]]
    if found.get("base_url"):
        args += ["--base-url", found["base_url"], "--api-format", found["api_format"]]
    return args


def request_for(found: dict, key: str) -> urllib.request.Request:
    provider, model = found["model"].split("/", 1)
    if found.get("base_url"):
        url = found["base_url"].rstrip("/") + "/chat/completions"
        body = {"model": model, "messages": [{"role": "user", "content": "Say ok."}], "max_tokens": 1}
        headers = {"Authorization": f"Bearer {key}"}
    elif provider == "anthropic":
        url = os.environ.get("ANTHROPIC_BASE_URL", "https://api.anthropic.com").rstrip("/") + "/v1/messages"
        body = {"model": model, "messages": [{"role": "user", "content": "Say ok."}], "max_tokens": 1}
        headers = {"x-api-key": key, "anthropic-version": "2023-06-01"}
    elif provider == "openrouter":
        url = "https://openrouter.ai/api/v1/chat/completions"
        body = {"model": model, "messages": [{"role": "user", "content": "Say ok."}], "max_tokens": 1}
        headers = {"Authorization": f"Bearer {key}"}
    else:
        url = os.environ.get("OPENAI_BASE_URL", "https://api.openai.com/v1").rstrip("/") + "/chat/completions"
        body = {"model": model, "messages": [{"role": "user", "content": "Say ok."}], "max_completion_tokens": 1}
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
            print(f"ok   {name}: {found['model']}")
        except urllib.error.HTTPError as e:
            detail = e.read().decode(errors="replace").replace(key, "<key>")[:300]
            print(f"FAIL {name}: {found['model']}: HTTP {e.code}: {detail}")
            failed += 1
        except (urllib.error.URLError, TimeoutError, json.JSONDecodeError) as e:
            print(f"FAIL {name}: {found['model']}: {str(e).replace(key, '<key>')[:300]}")
            failed += 1
    return 1 if failed else 0


def main():
    if len(sys.argv) < 3 or sys.argv[1] not in ("args", "key", "preflight"):
        sys.exit(__doc__)
    command, names = sys.argv[1], sys.argv[2:]
    if command == "args":
        print("\n".join(harbor_args(entry(names[0]))))
    elif command == "key":
        print(entry(names[0])["key"])
    else:
        sys.exit(preflight(names))


if __name__ == "__main__":
    main()
