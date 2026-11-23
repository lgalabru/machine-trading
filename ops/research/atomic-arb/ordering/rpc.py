"""Minimal JSON-RPC helper. Reads the URL from the Solana CLI config; never prints it."""
import json, os, re, time, urllib.request, urllib.error

def _url():
    p = os.path.expanduser("~/.config/solana/cli/config.yml")
    for line in open(p):
        m = re.match(r"\s*json_rpc_url:\s*\"?([^\"\s]+)", line)
        if m:
            return m.group(1)
    raise SystemExit("no json_rpc_url")

URL = _url()

def call(method, params, retries=6):
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).encode()
    delay = 0.5
    for i in range(retries):
        try:
            req = urllib.request.Request(URL, body, {"Content-Type": "application/json"})
            with urllib.request.urlopen(req, timeout=60) as r:
                out = json.loads(r.read())
            if "error" in out:
                e = out["error"]
                if method == "getProgramAccounts" or e.get("code") in (-32429, 429, -32005) or "rate" in str(e).lower():
                    time.sleep(delay); delay *= 2; continue
                return {"__error": e}
            return out["result"]
        except urllib.error.HTTPError as e:
            if e.code in (429, 500, 502, 503, 504):
                time.sleep(delay); delay *= 2; continue
            return {"__error": {"http": e.code}}
        except Exception as e:  # network
            time.sleep(delay); delay *= 2
    return {"__error": "retries exhausted"}
