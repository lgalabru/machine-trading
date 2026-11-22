"""The models `just bench` runs in Hermes (models.toml), and a check of each
one's key and name before a batch spends anything.

    python3 -I models.py args <name> <proxy url>  Harbor's flags for a model, one per line
    python3 -I models.py key <name>...             the variables holding their keys
    python3 -I models.py route <name>              the model's route on the key proxy
    python3 -I models.py preflight <name>...       one 1-token request per model (paid, on an x402 route)
    python3 -I models.py wallet <name>             the address an x402 route pays from
    python3 -I models.py keys                      every variable that may hold a key
    python3 -I models.py token <name>              the model's token on the key proxy
    python3 -I models.py scan <dir>                files under <dir> holding a key, in any form
    python3 -I models.py native-guard <tasks> <name>...  refuse native models on a guarded task (MTX_UNGUARDED=1 allows)
    python3 -I models.py stamp-unguarded <batch>    mark a batch `guarded: false`, which publish refuses
    python3 -I models.py fetch-scan <batch>         flag fetch attempts in a batch's trajectories

MTX_MODELS names another registry (the harness check's). A name that
isn't in it is `openai/<model>` as given, or
Anthropic's for a bare name, as `just bench` has always read it. Hermes
reaches every model through key_proxy.py, with its route's token
(MTX_PROXY_TOKEN when Harbor runs it) in place of a key: an HMAC of the
route's name under the batch's secret (MTX_PROXY_SECRET), so a token opens
one route only. Keys are read from the environment and never printed.

A model with `pay = "x402-solana"` has no API key: its variable holds a
Solana wallet's secret, and each call is paid for in USDC over x402
(x402_solana.py), at most `max_usd` a call (DEFAULT_MAX_USD unless set).
"""

import base64
import hashlib
import http.client
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

sys.path.insert(0, str(Path(__file__).parent))
import x402_solana  # noqa: E402

REGISTRY = Path(os.environ.get("MTX_MODELS") or Path(__file__).with_name("models.toml"))
APIS = ("openai", "anthropic", "chat")
PAYS = ("x402-solana",)
# What an x402 route may pay for one call, in USD, unless its table says.
DEFAULT_MAX_USD = 5.0
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
        if "pay" in found:
            if found["pay"] not in PAYS:
                sys.exit(f"{name}: pay is one of {', '.join(PAYS)}")
            if found["api"] != "chat":
                sys.exit(f"{name}: an x402 route takes chat completions (api = \"chat\")")
            network = x402_solana.network_id(found.get("network", "solana"))
            if not any(n == network for n, _ in x402_solana.ASSETS):
                sys.exit(f"{name}: network is solana or solana-devnet")
            cap = found.get("max_usd", DEFAULT_MAX_USD)
            if isinstance(cap, bool) or not isinstance(cap, (int, float)) or not cap > 0:
                sys.exit(f"{name}: max_usd is a positive number of US dollars")
        if "max_tokens" in found:
            tokens = found["max_tokens"]
            if isinstance(tokens, bool) or not isinstance(tokens, int) or tokens < 1 or found["api"] != "chat":
                sys.exit(f"{name}: max_tokens is a positive integer, for a chat route")
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
        # A reasoning model can't answer in one token: OpenAI's then refuse
        # the request ("max_tokens or model output limit was reached"), and
        # behind a router ask for at least 16.
        body = {"model": found["model"], "messages": prompt, tokens: 64}
        headers = {"Authorization": f"Bearer {key}"}
    return urllib.request.Request(
        url, data=json.dumps(body).encode(), headers={**headers, "Content-Type": "application/json"}, method="POST"
    )


def paid_preflight(name: str, found: dict, key: str) -> bool:
    """One paid call of at most 64 tokens on an x402 route: the wallet's secret reads, the
    model is known, the quote is payable and the wallet can pay it."""
    try:
        wallet = x402_solana.Wallet(key)
    except ValueError as e:
        print(f"FAIL {name}: {found['key']}: {e}")
        return False
    request = request_for(found, "")
    url = urllib.parse.urlsplit(request.full_url)
    headers = {k: v for k, v in request.header_items() if k.lower() != "authorization"}
    headers["User-Agent"] = "mtx-bench-preflight/1"
    connection_class = http.client.HTTPSConnection if url.scheme == "https" else http.client.HTTPConnection

    def send(extra):
        connection = connection_class(url.netloc, timeout=60)
        connection.request("POST", url.path, body=request.data, headers={**headers, **extra})
        return connection, connection.getresponse()

    cap = float(found.get("max_usd", DEFAULT_MAX_USD))
    network = x402_solana.network_id(found.get("network", "solana"))
    try:
        connection, response, paid = x402_solana.pay(send, wallet, network, cap, x402_solana.Ledger())
        try:
            data = response.read()
        finally:
            connection.close()
    except x402_solana.PaymentRefused as e:
        print(f"FAIL {name}: {found['model']}: {e.reason}")
        return False
    except (OSError, http.client.HTTPException) as e:
        print(f"FAIL {name}: {found['model']}: {type(e).__name__}: {str(e)[:200]}")
        return False
    if response.status != 200:
        detail = data.decode(errors="replace")[:300]
        print(f"FAIL {name}: {found['model']}: HTTP {response.status}: {detail}")
        return False
    print(f"ok   {name}: {found['model']} at {found['upstream']}, paid ${paid:.6f} from {wallet.address}")
    return True


def preflight(names: list[str]) -> int:
    failed = 0
    for name in names:
        found = entry(name)
        key = os.environ.get(found["key"], "")
        if not key:
            print(f"FAIL {name}: {found['key']} is not set")
            failed += 1
            continue
        if found.get("pay"):
            failed += not paid_preflight(name, found, key)
            continue
        try:
            with urllib.request.urlopen(request_for(found, key), timeout=60) as response:
                json.load(response)
            print(f"ok   {name}: {found['model']} at {found['upstream']}")
        except urllib.error.HTTPError as e:
            detail = e.read().decode(errors="replace").replace(key, "<key>")[:300]
            # The model answered and ran out of room: it's reachable.
            if e.code == 400 and "output limit was reached" in detail:
                print(f"ok   {name}: {found['model']} at {found['upstream']} (answered past the preflight's budget)")
                continue
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


def guarded_tasks(tasks: Path) -> list[str]:
    """The tasks whose environment starts from the base's `-agent` image: their
    agents run with an allowlisted network, and no generator in their `mtx`."""
    found = []
    for dockerfile in sorted(tasks.glob("*/environment/Dockerfile")):
        if re.search(r"^FROM \S+-agent(@\S+)?\s*$", dockerfile.read_text(), re.M):
            found.append(dockerfile.parent.parent.name)
    return found


def native_guard(tasks: Path, names: list[str], unguarded: bool = False) -> str | None:
    """Why `names` can't run on `tasks`, or None. A native harness (Claude Code,
    Codex) can't be guarded: it holds its provider's real key in the agent's
    environment, and the provider's API its network must allow can fetch the
    web for the agent (and the key can be used to call it). So on a guarded
    task they are refused, and only Hermes runs: its key proxy holds the keys
    and body_policy.py refuses server tools. `unguarded` (MTX_UNGUARDED=1)
    allows them for a screen that is never published (see stamp_unguarded)."""
    native = [n for n in names if n.startswith("native:")]
    guarded = guarded_tasks(tasks)
    if unguarded or not native or not guarded:
        return None
    return (
        f"{', '.join(native)}: native harnesses can't be guarded, so they can't run guarded tasks "
        f"({', '.join(guarded)}): the provider API the agent's network would have to allow can fetch "
        "the web for it. Run these models in Hermes (without `native:`), or set MTX_UNGUARDED=1 for "
        "a screen that is never published."
    )


def stamp_unguarded(batch: Path) -> None:
    """Mark a batch whose native harnesses ran on guarded tasks: `guarded: false`
    in batch.json, and a `.unguarded` file `just publish` and `mtx bench
    publish` refuse."""
    (batch / ".unguarded").write_text("native harnesses ran on guarded tasks; not for publication\n")
    plan = batch / "batch.json"
    data = json.loads(plan.read_text()) if plan.is_file() else {}
    data["guarded"] = False
    plan.write_text(json.dumps(data, indent=2))


# What an agent's trajectory shows when it tried to fetch from outside: a
# fetch tool or client, a server-side web tool that ran, the provider's API or
# the key that opens it, the repository's host.
FETCH_PATTERNS = {
    "curl": r"\bcurl\b",
    "wget": r"\bwget\b",
    "python requests": r"\b(requests\.(get|post|request)|urllib\.request|httpx\.|aiohttp)\b",
    "http.client": r"\bhttp\.client\b",
    "/dev/tcp": r"/dev/tcp/",
    "node fetch": r"(?<![\w.])fetch\(\s*[\"'`]https?:",
    "web search/fetch ran": r"\"web_(search|fetch)_requests\":\s*[1-9]|\bweb_search_call\b|\"(name|function_name|tool)\":\s*\"(web_(search|fetch)|Web(Search|Fetch))\"|\bweb_fetch\b(?!_requests)",
    "codex exec": r"(?<!nvm\.sh; fi; )\bcodex\s+exec\b(?!\s+--dangerously)",
    "provider api": r"api\.openai\.com|api\.anthropic\.com|chatgpt\.com|/v1/(responses|chat/completions|messages)",
    "key": r"auth\.json|\b(OPENAI|ANTHROPIC|CODEX)_API_KEY\b",
    "provider sdk": r"\b(from|import)\s+(openai|anthropic)\b|\.responses\.create",
    "raw.githubusercontent": r"raw\.githubusercontent\.com",
    "github": r"github\.com|ghcr\.io",
}


# A client or fetch tool is only an attempt when something outside is named:
# a trajectory that reaches only the sandbox's own chain, or the crates
# registry, fetched nothing.
CLIENTS = ("curl", "wget", "python requests", "http.client", "/dev/tcp", "node fetch")
LOCAL_URL = re.compile(
    r"https?://(?!(localhost|127\.0\.0\.1|0\.0\.0\.0|\[::1\]|[\w.-]*crates\.io|\$|\{|<)\b)[\w-]+(\.[\w-]+)+"
)


def fetch_flags(text: str) -> list[str]:
    text = re.sub(r"\\[nrt]", " ", text).replace('\\"', '"')
    outside = bool(LOCAL_URL.search(text)) or "/dev/tcp/" in text
    return [
        name
        for name, pattern in FETCH_PATTERNS.items()
        if re.search(pattern, text) and (outside or name not in CLIENTS)
    ]


def trajectory_flags(raw: str) -> list[str]:
    """A trajectory's fetch flags, from what the agent did: each of its tool
    calls on its own (a client call counts only with an outside URL in the same
    call), so the task's text, the files it printed and what came back don't
    count. The web tools that ran are read from the step without what came back: a
    usage counter counts only when it is above zero."""
    try:
        steps = json.loads(raw)["steps"]
    except (ValueError, KeyError, TypeError):
        return fetch_flags(raw)
    flags: set[str] = set()
    for step in steps:
        if not isinstance(step, dict) or step.get("source") in ("user", "system"):
            continue
        made = {k: v for k, v in step.items() if k != "observation"}
        if re.search(FETCH_PATTERNS["web search/fetch ran"], json.dumps(made)):
            flags.add("web search/fetch ran")
        calls = step.get("tool_calls")
        units = [json.dumps(c) for c in calls] if calls else [json.dumps(made)]
        for unit in units:
            flags.update(fetch_flags(unit))
    return [name for name in FETCH_PATTERNS if name in flags]


def fetch_scan(batch: Path) -> dict[str, list[str]]:
    """Each trial's trajectory's fetch flags, written to `.fetch-scan.json` and
    into batch.json's `fetch_flags`, for a run that wasn't guarded."""
    found = {}
    for trajectory in sorted(batch.glob("**/agent/trajectory.json")):
        flags = trajectory_flags(trajectory.read_text(errors="replace"))
        if flags:
            found[str(trajectory.relative_to(batch))] = flags
    (batch / ".fetch-scan.json").write_text(json.dumps(found, indent=2))
    plan = batch / "batch.json"
    if plan.is_file():
        data = json.loads(plan.read_text())
        data["fetch_flags"] = found
        plan.write_text(json.dumps(data, indent=2))
    return found


def main():
    commands = {"wallet": 2, "args": 3, "key": 2, "route": 2, "preflight": 2, "keys": 1, "scan": 2, "token": 2, "native-guard": 3, "stamp-unguarded": 2, "fetch-scan": 2}
    if len(sys.argv) < 2 or sys.argv[1] not in commands or len(sys.argv) < 1 + commands[sys.argv[1]]:
        sys.exit(__doc__)
    command, names = sys.argv[1], sys.argv[2:]
    if command == "args":
        print("\n".join(harbor_args(entry(names[0]), names[1])))
    elif command == "key":
        print("\n".join(dict.fromkeys(entry(n)["key"] for n in names)))
    elif command == "route":
        print(entry(names[0])["route"])
    elif command == "wallet":
        found = entry(names[0])
        if not found.get("pay"):
            sys.exit(f"{names[0]} isn't an x402 route")
        try:
            print(x402_solana.Wallet(os.environ.get(found["key"], "")).address)
        except ValueError as e:
            sys.exit(f"{found['key']}: {e}")
    elif command == "keys":
        print("\n".join(key_names()))
    elif command == "token":
        print(route_token(entry(names[0])["route"]))
    elif command == "stamp-unguarded":
        stamp_unguarded(Path(names[0]))
    elif command == "fetch-scan":
        for trial, flags in fetch_scan(Path(names[0])).items():
            print(f"fetch attempt in {trial}: {', '.join(flags)}")
    elif command == "native-guard":
        refusal = native_guard(Path(names[0]), names[1:], bool(os.environ.get("MTX_UNGUARDED")))
        if refusal:
            sys.exit(refusal)
    elif command == "scan":
        sys.exit(scan(Path(names[0])))
    else:
        sys.exit(preflight(names))


if __name__ == "__main__":
    main()
