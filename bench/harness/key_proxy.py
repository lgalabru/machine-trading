"""Keeps provider keys out of the trials: a forwarding proxy on the bench
machine.

A trial's agent sends its model calls here, to `http://<host>:<port>/<route>/...`,
carrying its route's token instead of a key. The keys stay in this
process's environment on the host; a trial container never holds one.

What a token can do: make the one model call its route allows (POST to
the route's model-call path) for the route's own model, with a body
limited to what Hermes sends for one answer (body_policy.py: an allowlist
of fields, the model's own tools only, no background or stored responses,
re-serialised from the parsed JSON), while the proxy runs. Its trials can spend on that
model, as any agent can. What it can't do: open another route (each
route has its own token, an HMAC of the route's name under the batch's
secret), reach any other endpoint of the provider (files, batches,
fine-tuning, admin, model lists, counts: everything but the model-call
path is refused, after rejecting any path with `..`, `//`, a backslash,
a percent-encoding or a query), call another model (a body whose `model`
isn't the route's is refused), pick an account or organisation (only an
allowlist of headers is forwarded), or reach the proxy from outside the
machine (it listens on the docker bridge the trials reach the host by). It takes
at most MAX_CONNECTIONS connections (MAX_PER_CLIENT from one address) and
MAX_BODY bytes a request, drops a client that hasn't sent its request
head within HEAD_DEADLINE seconds or stalls for CLIENT_TIMEOUT seconds,
logs a provider silent for UPSTREAM_TIMEOUT seconds, and keeps headers
naming the account (organisation, project, cookies) out of answers.

Routes come from models.toml, for the models named on the command line.
The batch's secret is read from MTX_PROXY_SECRET; the address it listens
on is printed once it is up. Refused requests are logged (method, path,
status, reason, route) to stderr.

    MTX_PROXY_SECRET=... python3 -I key_proxy.py --host <docker bridge address> --port 0 <model>...

For a model with `reasoning = "openrouter"`, an OpenAI-style
`reasoning_effort` becomes OpenRouter's `reasoning: {effort}`: Hermes sends
OpenRouter its own form only when it talks to openrouter.ai directly.
"""

import argparse
import hmac
import http.client
import json
import os
import re
import sys
import urllib.parse
import socket
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
import body_policy  # noqa: E402
import limits  # noqa: E402
from models import entry, key_forms, route_token  # noqa: E402

# Each API's one model-call path, under the route.
CALL_PATHS = {"openai": "/responses", "anthropic": "/v1/messages", "chat": "/chat/completions"}
# Headers forwarded from the client; the proxy adds the key. Account and
# project headers (OpenAI-Organization, OpenAI-Project, x-goog-user-project,
# ...) and everything else are dropped.
FORWARDED = {"content-type", "accept", "anthropic-version"}
# Anthropic betas that change how a model call streams or thinks, not which
# account or API it reaches.
ANTHROPIC_BETAS = re.compile(
    r"^(interleaved-thinking|fine-grained-tool-streaming|context-1m|output-128k|token-efficient-tools|"
    r"extended-cache-ttl|prompt-caching|effort|context-management)-[0-9-]+$"
)
UNSAFE = re.compile(r"\.\.|//|\\|%", re.IGNORECASE)
HOP = {"connection", "transfer-encoding", "content-length", "keep-alive", "content-encoding"}
# A long agent conversation is a few MB; the cap keeps one request from
# holding the proxy's memory.
MAX_BODY = 16 * 1024 * 1024
LENGTH = re.compile(r"[0-9]{1,9}")
# At most this many connections at once, and this many from one address
# (one trial's container), so one container can't take every slot: twice
# the most model calls a Hermes trial can have open at once (limits.py).
# One more gets a 503 with Retry-After, which the providers' clients
# retry.
MAX_CONNECTIONS = limits.MAX_CONNECTIONS
MAX_PER_CLIENT = limits.MAX_PER_CLIENT
RETRY_AFTER = 2
# Over-cap connections are logged once per this many seconds, with a count.
OVER_CAP_LOG_INTERVAL = 10
# Seconds a client has to send a request's head (request line and headers):
# per read, and in all. A connection holds a slot until then, before any
# token is checked.
HEAD_TIMEOUT = 5
HEAD_DEADLINE = 10
# Seconds a client may stall while sending a body or reading an answer,
# and the provider between bytes of one.
CLIENT_TIMEOUT = 120
UPSTREAM_TIMEOUT = 600
# Response headers that name the account behind the key, kept from the agent.
ACCOUNT_HEADERS = {"openai-organization", "openai-project", "anthropic-organization-id", "set-cookie"}

class Refused(Exception):
    def __init__(self, status, reason, route="-"):
        super().__init__(reason)
        self.status, self.reason, self.route = status, reason, route


class Server(ThreadingHTTPServer):
    daemon_threads = True
    # A burst of a trial's calls queues for accept instead of being dropped.
    request_queue_size = 256

    def __init__(self, *args, **kwargs):
        super().__init__(*args, **kwargs)
        self.lock = threading.Lock()
        self.open = 0
        self.per_client: dict[str, int] = {}
        self.over_cap: dict[str, int] = {}
        self.over_cap_logged = 0.0

    def process_request(self, request, client_address):
        address = client_address[0]
        with self.lock:
            full = self.open >= MAX_CONNECTIONS or self.per_client.get(address, 0) >= MAX_PER_CLIENT
            if not full:
                self.open += 1
                self.per_client[address] = self.per_client.get(address, 0) + 1
        if full:
            self.note_over_cap(address)
            threading.Thread(target=self.busy, args=(request,), daemon=True).start()
            return
        super().process_request(request, client_address)

    def note_over_cap(self, address):
        now = time.monotonic()
        with self.lock:
            self.over_cap[address] = self.over_cap.get(address, 0) + 1
            if now - self.over_cap_logged < OVER_CAP_LOG_INTERVAL:
                return
            self.over_cap_logged = now
            counts, self.over_cap = self.over_cap, {}
        summary = ", ".join(f"{n} from {a}" for a, n in sorted(counts.items()))
        print(f"answered 503 (too many open connections): {summary}", file=sys.stderr, flush=True)

    def busy(self, request):
        """A 503 the client can retry, then close; the request is never read."""
        data = b'{"error": {"message": "too many open connections, retry", "type": "proxy_busy"}}'
        try:
            request.settimeout(2)
            request.sendall(
                b"HTTP/1.0 503 Service Unavailable\r\nRetry-After: %d\r\nContent-Type: application/json\r\n"
                b"Content-Length: %d\r\nConnection: close\r\n\r\n%s" % (RETRY_AFTER, len(data), data)
            )
            request.shutdown(socket.SHUT_WR)
            # Read what the client sent, briefly, so closing doesn't reset
            # the connection before it reads the answer.
            deadline = time.monotonic() + 1
            while time.monotonic() < deadline and request.recv(65536):
                pass
        except OSError:
            pass
        finally:
            request.close()

    def process_request_thread(self, request, client_address):
        try:
            super().process_request_thread(request, client_address)
        finally:
            address = client_address[0]
            with self.lock:
                self.open -= 1
                self.per_client[address] -= 1
                if not self.per_client[address]:
                    del self.per_client[address]


class Proxy(BaseHTTPRequestHandler):
    # Close-delimited answers: a stream is relayed as it comes.
    protocol_version = "HTTP/1.0"
    server_version = "mtx-key-proxy"
    # Until the head is in; then CLIENT_TIMEOUT.
    timeout = HEAD_TIMEOUT
    head_deadline = HEAD_DEADLINE
    client_timeout = CLIENT_TIMEOUT

    def log_message(self, *args):
        pass

    def setup(self):
        super().setup()
        # A head trickled in a byte at a time still ends at the deadline.
        self.head_timer = threading.Timer(self.head_deadline, self.head_overdue)
        self.head_timer.daemon = True
        self.head_timer.start()

    def head_overdue(self):
        print(f"dropped {self.client_address[0]}: no request head within {self.head_deadline} s", file=sys.stderr, flush=True)
        try:
            self.connection.shutdown(2)
        except OSError:
            pass

    def head_in(self):
        self.head_timer.cancel()
        self.connection.settimeout(self.client_timeout)

    def finish(self):
        self.head_timer.cancel()
        super().finish()

    def refuse(self, refused):
        path = self.path[:120].encode("ascii", "backslashreplace").decode()
        print(f"refused {self.command} {path} {refused.status} {refused.reason} route={refused.route}", file=sys.stderr, flush=True)
        data = json.dumps({"error": {"message": refused.reason, "type": "proxy_error"}}).encode()
        self.send_response(refused.status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def checked(self):
        """The route and the body to forward, or Refused."""
        if UNSAFE.search(self.path) or "?" in self.path or "#" in self.path:
            raise Refused(404, "path not allowed")
        name, _, rest = self.path.lstrip("/").partition("/")
        route = self.server.routes.get(name)
        if route is None:
            raise Refused(404, "no such route")
        bearer = self.headers.get("Authorization", "")
        offered = bearer[7:] if bearer.startswith("Bearer ") else self.headers.get("x-api-key", "")
        if not hmac.compare_digest(offered.encode(), route["token"].encode()):
            raise Refused(401, "wrong token for this route", name)
        if self.command != "POST" or "/" + rest != CALL_PATHS[route["api"]]:
            raise Refused(404, "only the route's model call is allowed", name)
        if "chunked" in self.headers.get("Transfer-Encoding", "").lower():
            raise Refused(411, "chunked request bodies aren't accepted", name)
        lengths = self.headers.get_all("Content-Length") or []
        if len(lengths) != 1 or not LENGTH.fullmatch(lengths[0]):
            raise Refused(411, "a model call needs one Content-Length, in digits", name)
        if not 0 < int(lengths[0]) <= MAX_BODY:
            raise Refused(413, f"a model call's body is 1 to {MAX_BODY} bytes", name)
        body = self.rfile.read(int(lengths[0]))
        try:
            payload = body_policy.parse(body)
        except body_policy.BodyRefused as e:
            raise Refused(400, str(e), name) from None
        except (TypeError, ValueError, RecursionError) as e:
            raise Refused(400, f"the body can't be read: {type(e).__name__}", name) from None
        try:
            # Re-serialised: what the provider gets is what was checked.
            body = body_policy.apply(route["api"], payload, route["model"], route["openrouter"])
        except body_policy.BodyRefused as e:
            raise Refused(403, str(e), name) from None
        except (TypeError, ValueError, RecursionError, AttributeError) as e:
            raise Refused(403, f"the body doesn't fit a model call: {type(e).__name__}", name) from None
        return name, route, body

    def do_POST(self):
        self.head_in()
        try:
            name, route, body = self.checked()
        except Refused as refused:
            self.refuse(refused)
            return
        headers = {k: v for k, v in self.headers.items() if k.lower() in FORWARDED}
        betas = [b.strip() for b in self.headers.get("anthropic-beta", "").split(",") if b.strip()]
        if route["api"] == "anthropic" and betas:
            kept = [b for b in betas if ANTHROPIC_BETAS.match(b)]
            if kept:
                headers["anthropic-beta"] = ",".join(kept)
        headers["Accept-Encoding"] = "identity"
        headers["Content-Length"] = str(len(body))
        key = route["key"]
        if route["api"] == "anthropic":
            headers["x-api-key"] = key
        else:
            headers["Authorization"] = f"Bearer {key}"
        upstream = route["url"]
        connection_class = http.client.HTTPSConnection if upstream.scheme == "https" else http.client.HTTPConnection
        connection = connection_class(upstream.netloc, timeout=UPSTREAM_TIMEOUT)
        started = False
        side = "upstream"
        try:
            connection.request("POST", upstream.path.rstrip("/") + CALL_PATHS[route["api"]], body=body, headers=headers)
            response = connection.getresponse()
            if response.status >= 400:
                data = response.read()
                for form in route["forms"]:
                    data = data.replace(form, b"<key>")
                side = "client"
                self.send_response(response.status, response.reason)
                self.send_header("Content-Type", response.getheader("Content-Type", "application/json"))
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)
                return
            side = "client"
            self.send_response(response.status, response.reason)
            for k, v in response.getheaders():
                if k.lower() not in HOP and k.lower() not in ACCOUNT_HEADERS:
                    self.send_header(k, v)
            self.end_headers()
            started = True
            while True:
                side = "upstream"
                chunk = response.read1(65536)
                if not chunk:
                    break
                side = "client"
                self.wfile.write(chunk)
                self.wfile.flush()
        except (OSError, http.client.HTTPException) as e:
            if isinstance(e, TimeoutError) and side == "upstream":
                print(f"upstream silent for {UPSTREAM_TIMEOUT} s route={name}", file=sys.stderr, flush=True)
            elif isinstance(e, TimeoutError):
                print(f"client stopped reading for {self.client_timeout} s route={name}", file=sys.stderr, flush=True)
            if started:
                # The answer has begun: cut it short, so the client sees a
                # broken stream rather than a second status inside it.
                print(f"upstream broke mid-answer route={name}: {type(e).__name__}", file=sys.stderr, flush=True)
                self.close_connection = True
            else:
                self.refuse(Refused(502, f"upstream: {type(e).__name__}", name))
        finally:
            connection.close()

    def do_GET(self):
        self.head_in()
        self.refuse(Refused(404, "only the route's model call is allowed"))

    do_PUT = do_DELETE = do_PATCH = do_HEAD = do_OPTIONS = do_GET


def routes_for(names):
    routes, owners = {}, {}
    for name in dict.fromkeys(names):
        found = entry(name)
        if found["route"] in routes:
            sys.exit(f"{name} and {owners[found['route']]} share the route {found['route']!r}: rename one in models.toml")
        owners[found["route"]] = name
        key = os.environ.get(found["key"], "")
        if not key:
            sys.exit(f"{found['key']} is not set, for {name}")
        routes[found["route"]] = {
            "url": urllib.parse.urlsplit(found["upstream"]),
            "key": key,
            "forms": sorted(key_forms(key), key=len, reverse=True),
            "api": found["api"],
            "model": found["model"],
            "token": route_token(found["route"]),
            "openrouter": found.get("reasoning") == "openrouter",
        }
    return routes


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--host", required=True)
    parser.add_argument("--port", type=int, default=0)
    parser.add_argument("models", nargs="+")
    args = parser.parse_args()
    # The attack checks shrink the limits to test them quickly.
    global MAX_CONNECTIONS, MAX_PER_CLIENT, UPSTREAM_TIMEOUT
    MAX_CONNECTIONS = int(os.environ.get("MTX_PROXY_MAX_CONNECTIONS", MAX_CONNECTIONS))
    MAX_PER_CLIENT = int(os.environ.get("MTX_PROXY_MAX_PER_CLIENT", MAX_PER_CLIENT))
    UPSTREAM_TIMEOUT = float(os.environ.get("MTX_PROXY_UPSTREAM_TIMEOUT", UPSTREAM_TIMEOUT))
    Proxy.timeout = float(os.environ.get("MTX_PROXY_HEAD_TIMEOUT", HEAD_TIMEOUT))
    Proxy.head_deadline = float(os.environ.get("MTX_PROXY_HEAD_DEADLINE", HEAD_DEADLINE))
    Proxy.client_timeout = float(os.environ.get("MTX_PROXY_CLIENT_TIMEOUT", CLIENT_TIMEOUT))
    server = Server((args.host, args.port), Proxy)
    server.routes = routes_for(args.models)
    print(f"http://{args.host}:{server.server_address[1]}", flush=True)
    server.serve_forever()


if __name__ == "__main__":
    main()
