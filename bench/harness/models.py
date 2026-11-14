"""The models `just bench` runs in Hermes (models.toml), and a check of each
one's key and name before a batch spends anything.

    python3 -I models.py args <name> <proxy url>  Harbor's flags for a model, one per line
    python3 -I models.py key <name>...             the variables holding their keys
    python3 -I models.py route <name>              the model's route on the key proxy
    python3 -I models.py preflight <name>...       one 1-token request per model
    python3 -I models.py keys                      every variable that may hold a key
    python3 -I models.py token <name>              the model's token on the key proxy
    python3 -I models.py scan <dir>                files under <dir> holding a key, in any form

MTX_MODELS names another registry (the harness check's). A name that
isn't in it is `openai/<model>` as given, or
Anthropic's for a bare name, as `just bench` has always read it. Hermes
reaches every model through key_proxy.py, with its route's token
(MTX_PROXY_TOKEN when Harbor runs it) in place of a key: an HMAC of the
route's name under the batch's secret (MTX_PROXY_SECRET), so a token opens
one route only. Keys are read from the environment and never printed.
"""

import base64
import hashlib
import hmac
import json
import os
import urllib.parse
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


def route_token(route: str) -> str:
    """The token that opens one route of the batch's key proxy."""
    secret = os.environ.get("MTX_PROXY_SECRET", "")
    if len(secret) < 32:
        sys.exit("MTX_PROXY_SECRET must hold a random secret of 32 characters or more")
    return hmac.new(secret.encode(), route.encode(), hashlib.sha256).hexdigest()


def key_forms(key: str) -> set[bytes]:
    """A key as it may be written: as is, URL-encoded, and base64 (standard
    and URL-safe) at each of the three alignments it can take inside a longer
    encoded run, cut to the characters the key alone decides."""
    raw = key.encode()
    forms = {raw, urllib.parse.quote(key, safe="").encode(), urllib.parse.quote_plus(key).encode()}
    for offset, start in ((0, 0), (1, 2), (2, 3)):
        end = ((offset + len(raw)) * 8) // 6
        for encode in (base64.b64encode, base64.urlsafe_b64encode):
            forms.add(encode(b"\0" * offset + raw)[start:end])
    return {f for f in forms if len(f) >= 12}


def scan(root: Path) -> int:
    """Print every file under root that holds a key set in the environment,
    in any of its forms; the keys themselves never leave this process. A file
    that can't be read fails the scan too: nothing says it is clean."""
    forms = set()
    for value in (os.environ.get(n, "") for n in key_names()):
        if len(value) >= 8:
            forms |= key_forms(value)
    if not forms:
        # Nothing to look for is not a clean result.
        print(f"no key is set ({', '.join(key_names())}), so nothing was checked")
        return 1
    hits, unreadable = [], []
    for path in sorted(root.rglob("*")):
        if path.is_symlink() or not path.is_file():
            continue
        try:
            data = path.read_bytes()
        except OSError as e:
            unreadable.append(f"{path}: {e.strerror}")
            continue
        if any(f in data for f in forms):
            hits.append(path)
    for path in hits:
        print(f"key: {path}")
    for line in unreadable:
        print(f"unreadable, so not checked: {line}")
    return 1 if hits or unreadable else 0


def main():
    commands = {"args": 3, "key": 2, "route": 2, "preflight": 2, "keys": 1, "scan": 2, "token": 2}
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
    elif command == "token":
        print(route_token(entry(names[0])["route"]))
    elif command == "scan":
        sys.exit(scan(Path(names[0])))
    else:
        sys.exit(preflight(names))


if __name__ == "__main__":
    main()
