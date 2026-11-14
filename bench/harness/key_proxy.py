"""Keeps provider keys out of the trials: a forwarding proxy on the bench
machine.

A trial's agent sends its requests here, to `http://<host>:<port>/<model>/...`,
carrying a token drawn for the batch instead of a key. The proxy checks the
token, puts the model's real key on the request (x-api-key for Anthropic,
a bearer token otherwise) and forwards it, streaming the answer back. The
keys stay in this process's environment on the host; a trial container
never holds one, so nothing in it (an agent reading /proc, a log, a
trajectory) can show one.

Routes come from models.toml, for the models named on the command line.
The token is read from MTX_PROXY_TOKEN; the address it listens on is
printed once it is up.

    MTX_PROXY_TOKEN=... python3 -I key_proxy.py --host 10.0.0.2 --port 0 <model>...

For a model with `reasoning = "openrouter"`, an OpenAI-style
`reasoning_effort` becomes OpenRouter's `reasoning: {effort}`: Hermes sends
OpenRouter its own form only when it talks to openrouter.ai directly.
"""

import argparse
import http.client
import json
import os
import secrets
import sys
import urllib.parse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
from models import entry  # noqa: E402

DROPPED = {"host", "authorization", "x-api-key", "content-length", "accept-encoding", "connection"}
HOP = {"connection", "transfer-encoding", "content-length", "keep-alive", "content-encoding"}


class Proxy(BaseHTTPRequestHandler):
    # Close-delimited answers: a stream is relayed as it comes.
    protocol_version = "HTTP/1.0"
    server_version = "mtx-key-proxy"

    def log_message(self, *args):
        pass

    def deny(self, code, message):
        data = json.dumps({"error": {"message": message, "type": "proxy_error"}}).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def authorized(self):
        token = self.server.token
        bearer = self.headers.get("Authorization", "")
        offered = bearer[7:] if bearer.startswith("Bearer ") else self.headers.get("x-api-key", "")
        return secrets.compare_digest(offered.encode(), token.encode())

    def relay(self):
        if not self.authorized():
            self.deny(401, "unknown token")
            return
        name, _, rest = self.path.lstrip("/").partition("/")
        route = self.server.routes.get(name)
        if route is None:
            self.deny(404, f"no route {name!r}")
            return
        body = self.rfile.read(int(self.headers.get("Content-Length", 0) or 0))
        if route["openrouter"] and body:
            try:
                payload = json.loads(body)
                if isinstance(payload, dict) and payload.get("reasoning_effort") and "reasoning" not in payload:
                    payload["reasoning"] = {"effort": payload.pop("reasoning_effort")}
                    body = json.dumps(payload).encode()
            except json.JSONDecodeError:
                pass
        upstream = route["url"]
        headers = {k: v for k, v in self.headers.items() if k.lower() not in DROPPED}
        headers["Accept-Encoding"] = "identity"
        key = route["key"]
        if route["api"] == "anthropic":
            headers["x-api-key"] = key
        else:
            headers["Authorization"] = f"Bearer {key}"
        if body:
            headers["Content-Length"] = str(len(body))
        target = upstream.path.rstrip("/") + "/" + rest
        connection_class = http.client.HTTPSConnection if upstream.scheme == "https" else http.client.HTTPConnection
        connection = connection_class(upstream.netloc, timeout=1800)
        try:
            connection.request(self.command, target, body=body or None, headers=headers)
            response = connection.getresponse()
            self.send_response(response.status, response.reason)
            for k, v in response.getheaders():
                if k.lower() not in HOP:
                    self.send_header(k, v)
            if response.status >= 400:
                data = response.read().replace(key.encode(), b"<key>")
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)
                return
            self.end_headers()
            while chunk := response.read1(65536):
                self.wfile.write(chunk)
                self.wfile.flush()
        except (OSError, http.client.HTTPException) as e:
            self.deny(502, f"upstream: {type(e).__name__}")
        finally:
            connection.close()

    do_GET = relay
    do_POST = relay


def routes_for(names):
    routes = {}
    for name in names:
        found = entry(name)
        key = os.environ.get(found["key"], "")
        if not key:
            sys.exit(f"{found['key']} is not set, for {name}")
        url = urllib.parse.urlsplit(found["upstream"])
        routes[found["route"]] = {
            "url": url,
            "key": key,
            "api": found["api"],
            "openrouter": found.get("reasoning") == "openrouter",
        }
    return routes


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--host", required=True)
    parser.add_argument("--port", type=int, default=0)
    parser.add_argument("models", nargs="+")
    args = parser.parse_args()
    token = os.environ.get("MTX_PROXY_TOKEN", "")
    if len(token) < 32:
        sys.exit("MTX_PROXY_TOKEN must hold a random token of 32 characters or more")
    server = ThreadingHTTPServer((args.host, args.port), Proxy)
    server.daemon_threads = True
    server.token = token
    server.routes = routes_for(args.models)
    print(f"http://{args.host}:{server.server_address[1]}", flush=True)
    server.serve_forever()


if __name__ == "__main__":
    main()
