"""Keeps provider keys out of the trials: a forwarding proxy on the bench
machine.

A trial's agent sends its model calls here, to `http://<host>:<port>/<route>/...`,
carrying its route's token instead of a key. The keys stay in this
process's environment on the host; a trial container never holds one.

What a token can do: make the one model call its route allows (POST to
the route's model-call path) for the route's own model, with a body the
proxy forwards as is, while the proxy runs. Its trials can spend on that
model, as any agent can. What it can't do: open another route (each
route has its own token, an HMAC of the route's name under the batch's
secret), reach any other endpoint of the provider (files, batches,
fine-tuning, admin, model lists, counts: everything but the model-call
path is refused, after rejecting any path with `..`, `//`, a backslash,
a percent-encoding or a query), call another model (a body whose `model`
isn't the route's is refused), pick an account or organisation (only an
allowlist of headers is forwarded), or reach the proxy from outside the
machine (it listens on the docker bridge the trials reach the host by).

Routes come from models.toml, for the models named on the command line.
The batch's secret is read from MTX_PROXY_SECRET; the address it listens
on is printed once it is up. Refused requests are logged (method, status,
reason, route) to stderr.

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
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
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
MAX_BODY = 64 * 1024 * 1024


class Refused(Exception):
    def __init__(self, status, reason, route="-"):
        super().__init__(reason)
        self.status, self.reason, self.route = status, reason, route


class Proxy(BaseHTTPRequestHandler):
    # Close-delimited answers: a stream is relayed as it comes.
    protocol_version = "HTTP/1.0"
    server_version = "mtx-key-proxy"

    def log_message(self, *args):
        pass

    def refuse(self, refused):
        print(f"refused {self.command} {refused.status} {refused.reason} route={refused.route}", file=sys.stderr, flush=True)
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
        length = self.headers.get("Content-Length")
        if length is None or not length.isdigit() or not 0 < int(length) <= MAX_BODY:
            raise Refused(411, "a model call needs a Content-Length", name)
        body = self.rfile.read(int(length))
        try:
            payload = json.loads(body)
        except json.JSONDecodeError:
            raise Refused(400, "the body isn't JSON", name) from None
        if not isinstance(payload, dict) or payload.get("model") != route["model"]:
            raise Refused(403, "the body's model isn't this route's", name)
        if route["openrouter"] and payload.get("reasoning_effort") and "reasoning" not in payload:
            payload["reasoning"] = {"effort": payload.pop("reasoning_effort")}
            body = json.dumps(payload).encode()
        return name, route, body

    def do_POST(self):
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
        connection = connection_class(upstream.netloc, timeout=1800)
        started = False
        try:
            connection.request("POST", upstream.path.rstrip("/") + CALL_PATHS[route["api"]], body=body, headers=headers)
            response = connection.getresponse()
            if response.status >= 400:
                data = response.read()
                for form in route["forms"]:
                    data = data.replace(form, b"<key>")
                self.send_response(response.status, response.reason)
                self.send_header("Content-Type", response.getheader("Content-Type", "application/json"))
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)
                return
            self.send_response(response.status, response.reason)
            for k, v in response.getheaders():
                if k.lower() not in HOP:
                    self.send_header(k, v)
            self.end_headers()
            started = True
            while chunk := response.read1(65536):
                self.wfile.write(chunk)
                self.wfile.flush()
        except (OSError, http.client.HTTPException) as e:
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
        self.refuse(Refused(404, "only the route's model call is allowed"))

    do_PUT = do_DELETE = do_PATCH = do_HEAD = do_OPTIONS = do_GET


def routes_for(names):
    routes = {}
    for name in names:
        found = entry(name)
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
    server = ThreadingHTTPServer((args.host, args.port), Proxy)
    server.daemon_threads = True
    server.routes = routes_for(args.models)
    print(f"http://{args.host}:{server.server_address[1]}", flush=True)
    server.serve_forever()


if __name__ == "__main__":
    main()
