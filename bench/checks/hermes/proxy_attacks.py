"""Attacks on the bench's key proxy (bench/harness/key_proxy.py) and the
key scan, each with what must happen. Runs the stand-in provider and the
proxy on this machine, with throwaway keys; no model, no real key.

    python3 -I proxy_attacks.py --bind <address> [--other <another address>] [--container-image <image>]

`--other` checks the proxy can't be reached on any other address (the
bench binds it to the docker bridge, not the machine's network address).
`--container-image` calls it from a container on its own network, as a
trial does, while this machine's own address holds its share of slots.
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
import threading
import time
import urllib.parse
from pathlib import Path

HERE = Path(__file__).resolve().parent
HARNESS = HERE.parent.parent / "harness"
sys.path.insert(0, str(HARNESS))
import limits  # noqa: E402
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

[slow]
api = "chat"
model = "slow-upstream"
upstream = "http://127.0.0.1:{port}/v1"
key = "ATTACK_KEY"

[hold]
api = "chat"
model = "hold-1s"
upstream = "http://127.0.0.1:{port}/v1"
key = "ATTACK_KEY"

[hose]
api = "chat"
model = "firehose"
upstream = "http://127.0.0.1:{port}/v1"
key = "ATTACK_KEY"

[drop]
api = "chat"
model = "drop-mid-stream"
upstream = "http://127.0.0.1:{port}/v1"
key = "ATTACK_KEY"

[o]
api = "openai"
model = "gpt-x"
upstream = "http://127.0.0.1:{port}/v1"
key = "ATTACK_KEY"

[or]
api = "chat"
model = "vendor/m-or"
reasoning = "openrouter"
upstream = "http://127.0.0.1:{port}/api/v1"
key = "ATTACK_KEY"

[openai-x]
api = "openai"
model = "x"
upstream = "http://127.0.0.1:{port}/v1"
key = "ATTACK_KEY"
"""
ROUTES = ("a", "b", "anth", "echo", "drop", "o", "or", "slow", "hold", "hose")
LIMIT = 6
PER_CLIENT = 4

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
    parser.add_argument("--container-image", help="an image with bash, to call the proxy from a container")
    args = parser.parse_args()
    work = Path(tempfile.mkdtemp(prefix="proxy-attacks-"))
    key = "sk-attack-" + secrets.token_hex(20)
    env = {
        **os.environ,
        "ATTACK_KEY": key,
        "MOCK_API_KEY": key,
        "MTX_PROXY_SECRET": secrets.token_hex(32),
        "MTX_MODELS": str(work / "models.toml"),
        "MTX_PROXY_MAX_CONNECTIONS": str(LIMIT),
        "MTX_PROXY_MAX_PER_CLIENT": str(PER_CLIENT),
        "MTX_PROXY_CLIENT_TIMEOUT": "3",
        "MTX_PROXY_HEAD_TIMEOUT": "1",
        "MTX_PROXY_HEAD_DEADLINE": "2",
        "MTX_PROXY_UPSTREAM_TIMEOUT": "2",
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
        [sys.executable, "-I", str(HARNESS / "key_proxy.py"), "--host", args.bind, "--port", "0", *ROUTES],
        env=env, stdout=subprocess.PIPE, stderr=open(work / "proxy.log", "w"), text=True,
    )
    try:
        url = urllib.parse.urlsplit(proxy.stdout.readline().strip())
        time.sleep(0.5)
        token = {r: models.route_token(r) for r in ROUTES}

        seen_headers = set()

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
            seen_headers.clear()
            seen_headers.update(k.lower() for k in response.headers.keys())
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
        # 9. The body: an allowlist per API, re-serialised.
        def last_body():
            return json.loads(log.read_text().splitlines()[-1])

        def model_call(route, body, expect, what, raw=None, extra=None):
            before = seen()
            path = {"o": "/o/responses", "anth": "/anth/v1/messages"}.get(route, f"/{route}/chat/completions")
            headers = {**auth(route), **(extra or {})}
            if route == "anth":
                headers = {"x-api-key": token["anth"], "Content-Type": "application/json", "anthropic-version": "2023-06-01"}
            status, content = call(path, body, headers, raw=raw)
            reached = seen() > before
            ok = status == expect and reached == (expect == 200)
            check(9, what, ok, f"{status} reached={reached} {content[:100]!r}")
            return last_body() if reached else None

        fn = {"type": "function", "name": "terminal", "parameters": {}}
        chat_fn = {"type": "function", "function": {"name": "terminal", "parameters": {}}}
        o = lambda **kw: {"model": "gpt-x", "input": [], "tools": [fn], **kw}
        model_call("o", o(background=True), 403, "responses: background:true is refused")
        sent = model_call("o", o(background=False, store=True, service_tier="priority"), 200,
                          "responses: background:false, store:true and service_tier pass, rewritten")
        check(9, "responses: store forced false, background and service_tier dropped",
              bool(sent) and sent["body_values"].get("store") is False and "background" not in sent["body_keys"]
              and "service_tier" not in sent["body_keys"], str(sent and sent["body_keys"]))
        for tool in ({"type": "code_interpreter", "container": {"type": "auto"}}, {"type": "image_generation"},
                     {"type": "mcp", "server_label": "x", "server_url": "https://example.com"},
                     {"type": "web_search"}, {"type": "file_search"}, {"type": "computer_use_preview"}):
            model_call("o", o(tools=[tool]), 403, f"responses: a {tool['type']} tool is refused")
        model_call("o", o(tools=[{"type": "custom", "name": "x"}]), 200, "responses: a custom tool passes")
        for field in ("previous_response_id", "conversation", "prompt", "user", "safety_identifier"):
            model_call("o", o(**{field: "x"}), 403, f"responses: {field} is refused")
        c = lambda **kw: {"model": "vendor/m-or", "messages": [], "tools": [chat_fn], **kw}
        sent = model_call("or", c(models=["vendor/other"], route="fallback", plugins=[{"id": "web"}],
                                  provider={"order": ["x"]}, transforms=["middle-out"], reasoning_effort="high"), 200,
                          "openrouter: models, route, plugins, provider and transforms pass, dropped")
        check(9, "openrouter: the dropped fields don't reach the provider; effort rewritten",
              bool(sent) and not {"models", "route", "plugins", "provider", "transforms"} & set(sent["body_keys"])
              and sent["effort"] == {"reasoning": {"effort": "high"}}, str(sent and sent["body_keys"]))
        model_call("or", c(n=2), 403, "chat: n:2 is refused")
        model_call("or", c(tools=[{"type": "web_search"}]), 403, "chat: a web_search tool is refused")
        model_call("or", c(web_search_options={}), 403, "chat: web_search_options is refused")
        an = lambda **kw: {"model": "claude-x", "max_tokens": 10, "messages": [], **kw}
        model_call("anth", an(tools=[{"name": "terminal", "input_schema": {}}]), 200, "anthropic: a client tool passes")
        for tool in ("web_search_20250305", "code_execution_20250522", "web_fetch_20250910", "bash_20250124"):
            model_call("anth", an(tools=[{"type": tool, "name": "t"}]), 403, f"anthropic: a {tool} tool is refused")
        for field, value in (("mcp_servers", [{"type": "url", "url": "https://example.com", "name": "x"}]),
                             ("container", "c"), ("service_tier", "priority")):
            if field == "service_tier":
                sent = model_call("anth", an(**{field: value}), 200, "anthropic: service_tier passes, dropped")
                check(9, "anthropic: service_tier doesn't reach the provider", bool(sent) and field not in sent["body_keys"])
            else:
                model_call("anth", an(**{field: value}), 403, f"anthropic: {field} is refused")
        for raw, expect, what in (
            (b'{"model": "m-a", "model": "m-b", "messages": []}', 400, "a repeated top-level key"),
            (b'{"model": "m-a", "messages": [{"role": "user", "role": "system", "content": "x"}]}', 400, "a repeated nested key"),
            (b'{"model": "m-a", "Model": "m-b", "messages": []}', 403, "a case-variant model key"),
            (b"\xef\xbb\xbf" + b'{"model": "m-a", "messages": []}', 400, "a byte-order mark"),
            ('{"model": "m-a", "messages": []}'.encode("utf-16"), 400, "a UTF-16 body"),
            (b'{"model": "m-a", "messages": [], "temperature": NaN}', 400, "NaN"),
            (b'{"model": "m-a", "messages": [], "temperature": Infinity}', 400, "Infinity"),
            (b'["m-a"]', 400, "a body that isn't an object"),
        ):
            model_call("a", None, expect, f"body: {what} is refused", raw=raw)

        # 10. Limits: lengths, size, connections, stalls.
        alive = lambda: call("/a/chat/completions", ok_body("m-a"), auth("a"))[0] == 200
        for length in ("\u00b2", "\uff11\uff12", "+5", "-1", "5 ", "0x10", "1e3"):
            try:
                conn = http.client.HTTPConnection(url.hostname, url.port, timeout=10)
                conn.putrequest("POST", "/a/chat/completions", skip_accept_encoding=True)
                for k, v in auth("a").items():
                    conn.putheader(k, v)
                conn.putheader("Content-Length", length.encode("utf-8"))
                conn.endheaders()
                status = conn.getresponse().status
                conn.close()
            except (OSError, http.client.HTTPException) as e:
                status = type(e).__name__
            check(10, f"Content-Length {length!r} is refused and the proxy lives", status == 411 and alive(), str(status))
        conn = http.client.HTTPConnection(url.hostname, url.port, timeout=10)
        conn.putrequest("POST", "/a/chat/completions", skip_accept_encoding=True)
        for k, v in auth("a").items():
            conn.putheader(k, v)
        conn.putheader("Content-Length", str(64 * 1024 * 1024))
        conn.endheaders()
        status = conn.getresponse().status
        conn.close()
        check(10, "a 64 MB body is refused before it's read", status == 413, str(status))
        retry_after = {}

        def tried(port=None, route="a", model="m-a"):
            try:
                conn = http.client.HTTPConnection(url.hostname, port or url.port, timeout=30)
                data = json.dumps(ok_body(model)).encode()
                conn.request("POST", f"/{route}/chat/completions", body=data, headers=auth(route))
                response = conn.getresponse()
                response.read()
                retry_after[response.status] = response.getheader("Retry-After")
                conn.close()
                return response.status
            except (OSError, http.client.HTTPException) as e:
                return type(e).__name__

        def opened(n, port=None):
            return [socket.create_connection((url.hostname, port or url.port), timeout=10) for _ in range(n)]

        # 12. No token is needed to hold a slot, so one client (address)
        # gets few, and only for the head's few seconds.
        idle = opened(PER_CLIENT)
        time.sleep(0.3)
        over = tried()
        check(12, f"one address holding {PER_CLIENT} idle connections gets a 503 with Retry-After for another",
              over == 503 and retry_after.get(503) == "2", f"{over} {retry_after.get(503)}")
        if args.container_image:
            # A trial's container has its own address on its own network.
            network = f"proxy-attacks-{secrets.token_hex(4)}"
            subprocess.run(["docker", "network", "create", network], check=True, capture_output=True)
            data = json.dumps(ok_body("m-a"))
            request = (f"POST /a/chat/completions HTTP/1.0\r\nAuthorization: Bearer $TOKEN\r\n"
                       f"Content-Type: application/json\r\nContent-Length: {len(data)}\r\n\r\n{data}")
            try:
                out = subprocess.run(
                    ["docker", "run", "--rm", "--name", network, "--network", network, "-e", "TOKEN",
                     "--entrypoint", "bash", args.container_image, "-c",
                     f"exec 3<>/dev/tcp/{url.hostname}/{url.port}; printf '%s' \"{request.replace(chr(34), chr(92) + chr(34))}\" >&3; head -c 12 <&3"],
                    env={**os.environ, "TOKEN": token["a"]}, capture_output=True, text=True, timeout=120)
            finally:
                subprocess.run(["docker", "network", "rm", network], capture_output=True)
            check(12, "while a container on another network is still served", out.stdout.startswith("HTTP/1.0 200"),
                  repr(out.stdout[:40] + out.stderr[-200:]))
        start = time.monotonic()
        dropped = sum(1 for sock in idle if sock.recv(1) == b"")
        took = time.monotonic() - start
        for sock in idle:
            sock.close()
        check(12, "connections that send no head are dropped at the head deadline",
              dropped == len(idle) and took < 4, f"{dropped}/{len(idle)} in {took:.1f}s")
        time.sleep(0.3)
        check(12, "the proxy serves again once they're gone", alive())
        trickle = socket.create_connection((url.hostname, url.port), timeout=10)
        trickle.sendall(b"POST /a/chat/completions HTTP/1.1\r\n")
        start, closed = time.monotonic(), False
        while time.monotonic() - start < 6:
            try:
                trickle.sendall(b"X")
                time.sleep(0.5)
                trickle.setblocking(False)
                try:
                    closed = trickle.recv(1) == b""
                except BlockingIOError:
                    pass
                trickle.setblocking(True)
            except OSError:
                closed = True
            if closed:
                break
        took = time.monotonic() - start
        trickle.close()
        check(12, "a head sent a byte at a time is cut at the deadline", closed and took < 4, f"closed={closed} {took:.1f}s")
        slow = socket.create_connection((url.hostname, url.port), timeout=10)
        data = json.dumps(ok_body("m-a")).encode()
        head = "".join(f"{k}: {v}\r\n" for k, v in auth("a").items())
        slow.sendall(f"POST /a/chat/completions HTTP/1.1\r\nHost: x\r\n{head}Content-Length: {len(data)}\r\n\r\n".encode())
        time.sleep(1.5)
        slow.sendall(data)
        answer = slow.recv(64)
        slow.close()
        check(12, "a body sent after the head timeout but within the client timeout is served",
              answer.startswith(b"HTTP/1.0 200"), repr(answer[:30]))

        # 12. The global cap, on a second proxy with no per-address cap.
        second = subprocess.Popen(
            [sys.executable, "-I", str(HARNESS / "key_proxy.py"), "--host", args.bind, "--port", "0", "a"],
            env={**env, "MTX_PROXY_MAX_PER_CLIENT": "100"}, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True)
        try:
            port = urllib.parse.urlsplit(second.stdout.readline().strip()).port
            held = opened(LIMIT, port)
            time.sleep(0.3)
            over = tried(port)
            check(12, f"with {LIMIT} open in all, a new connection gets a 503", over == 503, str(over))
            for sock in held:
                sock.close()
            time.sleep(0.3)
            check(12, "and served once they close", tried(port) == 200, "refused")
        finally:
            second.terminate()
            second.wait()

        # 15. The caps fit the most a Hermes trial calls at once (limits.py):
        # a proxy at the real caps serves that many from one address.
        third = subprocess.Popen(
            [sys.executable, "-I", str(HARNESS / "key_proxy.py"), "--host", args.bind, "--port", "0", "hold"],
            env={k: v for k, v in env.items() if not k.startswith("MTX_PROXY_") or k == "MTX_PROXY_SECRET"},
            stdout=subprocess.PIPE, stderr=open(work / "third.log", "w"), text=True)
        try:
            port = urllib.parse.urlsplit(third.stdout.readline().strip()).port
            statuses = []
            lock = threading.Lock()

            def one():
                status = tried(port, "hold", "hold-1s")
                with lock:
                    statuses.append(status)

            def fan(n):
                threads = [threading.Thread(target=one) for _ in range(n)]
                for t in threads:
                    t.start()
                for t in threads:
                    t.join()

            fan(limits.WORST_CASE_CALLS)
            check(15, f"{limits.WORST_CASE_CALLS} calls at once from one trial's address are all served",
                  statuses.count(200) == limits.WORST_CASE_CALLS, str(sorted(set(map(str, statuses)))))
            statuses.clear()
            fan(limits.MAX_PER_CLIENT + 8)
            check(15, f"beyond {limits.MAX_PER_CLIENT} at once, the rest get a 503 the client can retry",
                  statuses.count(200) >= limits.MAX_PER_CLIENT - 2 and 503 in statuses
                  and set(statuses) <= {200, 503}, f"{statuses.count(200)} served, {statuses.count(503)} 503")
        finally:
            third.terminate()
            third.wait()
        third_log = (work / "third.log").read_text()
        check(15, "the over-cap log is one line per interval, with a count",
              third_log.count("answered 503") == 1 and "from " in third_log, third_log[-300:])

        # 16. A client that stops reading is logged as that, not as the provider.
        conn = socket.create_connection((url.hostname, url.port), timeout=10)
        data = json.dumps(ok_body("firehose")).encode()
        head = "".join(f"{k}: {v}\r\n" for k, v in auth("hose").items())
        conn.sendall(f"POST /hose/chat/completions HTTP/1.1\r\nHost: x\r\n{head}Content-Length: {len(data)}\r\n\r\n".encode() + data)
        conn.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 4096)
        time.sleep(6)
        conn.close()
        time.sleep(0.5)
        proxy_log = (work / "proxy.log").read_text()
        check(16, "a client that stops reading is logged as such",
              "client stopped reading" in proxy_log and "upstream silent" not in proxy_log.split("client stopped reading")[0][-200:],
              proxy_log[-300:])

        # 13. tool_choice, overflowing numbers, bodies that break the checks.
        model_call("o", o(tool_choice="auto"), 200, "responses: tool_choice auto passes")
        model_call("o", o(tool_choice={"type": "function", "name": "terminal"}), 200, "responses: a function tool_choice passes")
        for choice in ({"type": "web_search"}, {"type": "mcp", "server_label": "x"}, {"type": "allowed_tools", "tools": []},
                       {"type": "image_generation"}, {"type": "code_interpreter"}, "bogus", 5):
            model_call("o", o(tool_choice=choice), 403, f"responses: tool_choice {choice!r} is refused")
        model_call("or", c(tool_choice={"type": "function", "function": {"name": "terminal"}}), 200, "chat: a function tool_choice passes")
        model_call("or", c(tool_choice={"type": "web_search"}), 403, "chat: tool_choice web_search is refused")
        model_call("anth", an(tools=[{"name": "terminal", "input_schema": {}}], tool_choice={"type": "tool", "name": "terminal"}), 200,
                   "anthropic: a tool tool_choice passes")
        for choice in ({"type": "web_search"}, "auto"):
            model_call("anth", an(tool_choice=choice), 403, f"anthropic: tool_choice {choice!r} is refused")
        for raw, what in ((b'{"model": "m-a", "messages": [], "temperature": 1e400}', "1e400"),
                          (b'{"model": "m-a", "messages": [], "temperature": -1e400}', "-1e400"),
                          (b'{"model": "m-a", "messages": [], "seed": 1' + b"0" * 5000 + b"}", "a 5000-digit integer"),
                          (b'{"model": "m-a", "messages": ' + b"[" * 200000 + b"]" * 200000 + b"}", "200000 nested lists")):
            before_log = (work / "proxy.log").read_text().count("\n")
            model_call("a", None, 400, f"body: {what} is refused with a 400", raw=raw)
            logged = (work / "proxy.log").read_text().count("\n") > before_log
            check(13, f"body: {what} is logged and the proxy lives", logged and alive())
        for tools in (5, {"type": "function"}, "terminal"):
            model_call("a", {"model": "m-a", "messages": [], "tools": tools}, 403, f"body: tools {tools!r} is refused with a 403")

        # 14. Answers name no account; a silent provider is logged.
        status, _ = call("/a/chat/completions", ok_body("m-a"), auth("a"))
        leaked = {"openai-organization", "anthropic-organization-id", "set-cookie"} & seen_headers
        check(14, "an answer's account headers are dropped", status == 200 and not leaked, f"{status} {sorted(leaked)}")
        status, _ = call("/echo/chat/completions", ok_body("echo-key"), auth("echo"))
        leaked = {"openai-organization", "anthropic-organization-id", "set-cookie"} & seen_headers
        check(14, "an error's account headers are dropped", status == 400 and not leaked, f"{status} {sorted(leaked)}")
        status, _ = call("/slow/chat/completions", ok_body("slow-upstream"), auth("slow"))
        check(14, "a provider silent past the upstream timeout gets a 502, logged",
              status == 502 and "upstream silent" in (work / "proxy.log").read_text(), str(status))

        # 11. Two names on one route; a scan with no key.
        out = subprocess.run([sys.executable, "-I", str(HARNESS / "key_proxy.py"), "--host", "127.0.0.1", "openai-x", "openai/x"],
                             env=env, capture_output=True, text=True, timeout=30)
        check(11, "two model names on one route are refused", out.returncode != 0 and "share the route" in out.stderr,
              out.stderr[-200:])
        bare = {k: v for k, v in env.items() if k not in models.key_names() and k not in ("ATTACK_KEY", "MOCK_API_KEY")}
        out = subprocess.run([sys.executable, "-I", str(HARNESS / "models.py"), "scan", str(tree / "clean")],
                             env=bare, capture_output=True, text=True)
        check(11, "a scan with no key set fails", out.returncode == 1 and "no key is set" in out.stdout, out.stdout)
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
