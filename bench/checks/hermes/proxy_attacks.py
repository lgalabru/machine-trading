"""Attacks on the bench's key proxy (bench/harness/key_proxy.py) and the
key scan, each with what must happen. Runs the stand-in provider and the
proxy on this machine, with throwaway keys; no model, no real key.

    python3 -I proxy_attacks.py --bind <address the proxy listens on> [--other <another address of this machine>]

`--other` checks the proxy can't be reached on any other address (the
bench binds it to the docker bridge, not the machine's network address).
"""

import argparse
import base64
import http.client
import json
import os
import secrets
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import urllib.parse
from pathlib import Path

HERE = Path(__file__).resolve().parent
HARNESS = HERE.parent.parent / "harness"
sys.path.insert(0, str(HARNESS))
import models  # noqa: E402

REGISTRY = """
[a]
api = "chat"
model = "m-a"
upstream = "http://127.0.0.1:{port}/v1"
key = "ATTACK_KEY"

[b]
api = "chat"
model = "m-b"
upstream = "http://127.0.0.1:{port}/v1"
key = "ATTACK_KEY"

[anth]
api = "anthropic"
model = "claude-x"
upstream = "http://127.0.0.1:{port}"
key = "ATTACK_KEY"

[echo]
api = "chat"
model = "echo-key"
upstream = "http://127.0.0.1:{port}/v1"
key = "ATTACK_KEY"

[drop]
api = "chat"
model = "drop-mid-stream"
upstream = "http://127.0.0.1:{port}/v1"
key = "ATTACK_KEY"
"""

results = []


def check(item, what, ok, detail=""):
    results.append(ok)
    print(f"{'pass' if ok else 'FAIL'}  [{item}] {what}{'' if ok else ': ' + detail}")


def free_port(host):
    s = socket.socket()
    s.bind((host, 0))
    port = s.getsockname()[1]
    s.close()
    return port


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--bind", default="127.0.0.1")
    parser.add_argument("--other")
    args = parser.parse_args()
    work = Path(tempfile.mkdtemp(prefix="proxy-attacks-"))
    key = "sk-attack-" + secrets.token_hex(20)
    env = {
        **os.environ,
        "ATTACK_KEY": key,
        "MOCK_API_KEY": key,
        "MTX_PROXY_SECRET": secrets.token_hex(32),
        "MTX_MODELS": str(work / "models.toml"),
    }
    os.environ.update({k: env[k] for k in ("MTX_PROXY_SECRET", "MTX_MODELS")})
    mock_port = free_port("127.0.0.1")
    (work / "models.toml").write_text(REGISTRY.format(port=mock_port))
    models.REGISTRY = work / "models.toml"
    log = work / "requests.jsonl"
    log.touch()
    mock = subprocess.Popen(
        [sys.executable, "-I", str(HERE / "mock_provider.py"), "--host", "127.0.0.1", "--port", str(mock_port), "--log", str(log)],
        env=env,
    )
    proxy = subprocess.Popen(
        [sys.executable, "-I", str(HARNESS / "key_proxy.py"), "--host", args.bind, "--port", "0", "a", "b", "anth", "echo", "drop"],
        env=env, stdout=subprocess.PIPE, stderr=open(work / "proxy.log", "w"), text=True,
    )
    try:
        url = urllib.parse.urlsplit(proxy.stdout.readline().strip())
        time.sleep(0.5)
        token = {r: models.route_token(r) for r in ("a", "b", "anth", "echo", "drop")}

        def call(path, body=None, headers=None, method="POST", raw=None, host=None):
            conn = http.client.HTTPConnection(host or url.hostname, url.port, timeout=30)
            data = raw if raw is not None else (json.dumps(body).encode() if body is not None else None)
            conn.putrequest(method, path, skip_accept_encoding=True)
            for k, v in (headers or {}).items():
                conn.putheader(k, v)
            if data is not None and "Transfer-Encoding" not in (headers or {}):
                conn.putheader("Content-Length", str(len(data)))
            conn.endheaders()
            if data is not None:
                conn.send(data)
            response = conn.getresponse()
            try:
                content = response.read()
            except (http.client.IncompleteRead, ConnectionError) as e:
                content = getattr(e, "partial", b"")
            conn.close()
            return response.status, content

        def seen():
            return len(log.read_text().splitlines())

        auth = lambda r: {"Authorization": f"Bearer {token[r]}", "Content-Type": "application/json"}
        ok_body = lambda m: {"model": m, "messages": [{"role": "user", "content": "hi"}]}

        status, _ = call("/a/chat/completions", ok_body("m-a"), auth("a"))
        check(1, "route a's token opens route a", status == 200, str(status))

        # 1. One token per route.
        status, _ = call("/b/chat/completions", ok_body("m-b"), auth("a"))
        check(1, "route a's token on route b is refused", status == 401, str(status))
        status, _ = call("/b/chat/completions", ok_body("m-b"), {"x-api-key": token["a"]})
        check(1, "route a's token as x-api-key on route b is refused", status == 401, str(status))

        # 2. Only the model-call path.
        before = seen()
        paths = [
            ("POST", "/a/files"), ("POST", "/a/batches"), ("POST", "/a/fine_tuning/jobs"),
            ("POST", "/a/organization/admin_api_keys"), ("GET", "/a/models"), ("GET", "/a/chat/completions"),
            ("POST", "/a/../b/chat/completions"), ("POST", "/a/%2e%2e/b/chat/completions"),
            ("POST", "/a/chat/completions%2F"), ("POST", "/a//chat/completions"),
            ("POST", "/a/chat/completions?x=1"), ("POST", "/a/chat\\completions"),
            ("POST", "/a/chat/completions/../../files"), ("POST", "/a/%252e%252e/files"),
            ("POST", "/anth/v1/messages/batches"), ("POST", "/anth/v1/messages/count_tokens"),
            ("POST", "/anth/v1/files"), ("DELETE", "/a/chat/completions"),
        ]
        for method, path in paths:
            r = "anth" if path.startswith("/anth") else "a"
            status, _ = call(path, ok_body("claude-x" if r == "anth" else "m-a"), auth(r), method=method)
            check(2, f"{method} {path} is refused", status in (401, 403, 404, 405), str(status))
        check(2, "no refused path reached the provider", seen() == before, f"{seen() - before} requests")

        # 3. The route's model only.
        before = seen()
        status, _ = call("/a/chat/completions", ok_body("m-b"), auth("a"))
        check(3, "another model on route a is refused with 403", status == 403, str(status))
        status, _ = call("/a/chat/completions", {"messages": []}, auth("a"))
        check(3, "a body with no model is refused with 403", status == 403, str(status))
        status, _ = call("/a/chat/completions", raw=b"not json", headers=auth("a"))
        check(3, "a body that isn't JSON is refused", status == 400, str(status))
        check(3, "none of them reached the provider", seen() == before, f"{seen() - before} requests")

        # 4. Only allowlisted headers reach the provider.
        status, _ = call("/anth/v1/messages", ok_body("claude-x"), {
            "x-api-key": token["anth"], "Content-Type": "application/json", "anthropic-version": "2023-06-01",
            "OpenAI-Organization": "org-attacker", "OpenAI-Project": "proj-attacker",
            "x-goog-user-project": "attacker", "Cookie": "a=b", "X-Forwarded-For": "1.2.3.4",
            "anthropic-beta": "message-batches-2024-09-24,files-api-2025-04-14,interleaved-thinking-2025-05-14",
        })
        last = json.loads(log.read_text().splitlines()[-1])
        leaked = {"openai-organization", "openai-project", "x-goog-user-project", "cookie", "x-forwarded-for"} & set(last["headers"])
        check(4, "account, project and other headers are dropped", status == 200 and not leaked, f"{status} {sorted(leaked)}")
        check(4, "only feature betas pass in anthropic-beta", last["anthropic_beta"] == "interleaved-thinking-2025-05-14",
              str(last["anthropic_beta"]))

        # 5. Reachable on its bind address only.
        if args.other:
            try:
                socket.create_connection((args.other, url.port), timeout=5).close()
                check(5, f"the proxy can't be reached on {args.other}", False, "connected")
            except OSError:
                check(5, f"the proxy can't be reached on {args.other}", True)

        # 6. The key in no form in an error body.
        status, content = call("/echo/chat/completions", ok_body("echo-key"), auth("echo"))
        found = [f for f in models.key_forms(key) if f in content]
        check(6, "an error body that repeats the key reaches the client with no form of it",
              status == 400 and not found and b"<key>" in content, f"{status} {len(found)} forms")

        # 7. The scan: every form, unreadable files, a clean tree.
        tree = work / "tree"
        (tree / "clean").mkdir(parents=True)
        (tree / "clean" / "a.json").write_text('{"nothing": "here"}')
        os.environ["ATTACK_KEY"] = key
        models_keys = models.key_names
        models.key_names = lambda: ["ATTACK_KEY"]
        planted = {
            "raw.txt": key.encode(),
            "b64.txt": base64.b64encode(key.encode()),
            "b64-shifted.txt": b"blob:" + base64.b64encode(b"ab" + key.encode() + b"tail"),
            "b64-urlsafe.txt": base64.urlsafe_b64encode(b"q" + key.encode()),
            "url.txt": urllib.parse.quote(key, safe="").encode(),
        }
        hits = tree / "hits"
        hits.mkdir()
        for name, data in planted.items():
            (hits / name).write_bytes(data)
        out = subprocess.run([sys.executable, "-I", str(HARNESS / "models.py"), "scan", str(hits)],
                             env={**env, "MTX_MODELS": str(work / "models.toml")}, capture_output=True, text=True)
        flagged = {Path(line.split(": ", 1)[1]).name for line in out.stdout.splitlines() if line.startswith("key: ")}
        check(7, "the scan finds the key raw, base64 (each alignment, url-safe) and URL-encoded",
              out.returncode == 1 and flagged == set(planted), f"{out.returncode} {sorted(flagged)}")
        out = subprocess.run([sys.executable, "-I", str(HARNESS / "models.py"), "scan", str(tree / "clean")],
                             env=env, capture_output=True, text=True)
        check(7, "a clean tree passes", out.returncode == 0, out.stdout)
        locked = tree / "clean" / "locked.json"
        locked.write_text("{}")
        locked.chmod(0)
        out = subprocess.run([sys.executable, "-I", str(HARNESS / "models.py"), "scan", str(tree / "clean")],
                             env=env, capture_output=True, text=True)
        locked.chmod(0o600)
        check(7, "an unreadable file fails the scan, by name", out.returncode == 1 and "unreadable" in out.stdout
              and "locked.json" in out.stdout, out.stdout)
        models.key_names = models_keys

        # 8. A broken stream stays broken; chunked bodies are refused.
        status, content = call("/drop/chat/completions", ok_body("drop-mid-stream"), auth("drop"))
        check(8, "a stream cut mid-answer ends without a second status in it",
              status == 200 and b"partial" in content and b"502" not in content and b"HTTP/" not in content,
              f"{status} {content[:120]!r}")
        before = seen()
        chunked = b"1a\r\n" + json.dumps(ok_body("m-a")).encode()[:26] + b"\r\n0\r\n\r\n"
        status, _ = call("/a/chat/completions", raw=chunked, headers={**auth("a"), "Transfer-Encoding": "chunked"})
        check(8, "a chunked request body is refused, not sent on empty", status == 411 and seen() == before,
              f"{status} {seen() - before}")
        status, _ = call("/a/chat/completions", headers=auth("a"))
        check(8, "a model call with no body is refused", status == 411, str(status))
    finally:
        proxy.terminate()
        mock.terminate()
        proxy.wait()
        mock.wait()
    failed = results.count(False)
    print(f"proxy attacks: {'pass' if not failed else 'FAIL'}, {len(results) - failed}/{len(results)} checks")
    print("refused, as the proxy logged them:")
    print((work / "proxy.log").read_text().strip())
    shutil.rmtree(work)
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
