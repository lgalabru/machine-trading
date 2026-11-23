"""Tiny JSON-RPC helper. Reads the RPC URL from the Solana CLI config; never prints it."""
import json, os, re, sys, time, urllib.request, urllib.error

def _url():
    cfg = open(os.path.expanduser("~/.config/solana/cli/config.yml")).read()
    return re.search(r"json_rpc_url:\s*\"?([^\s\"]+)", cfg).group(1)

URL = _url()

def call(method, params, retries=8):
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).encode()
    delay = 0.5
    for attempt in range(retries):
        try:
            req = urllib.request.Request(URL, body, {"Content-Type": "application/json"})
            with urllib.request.urlopen(req, timeout=60) as r:
                out = json.loads(r.read())
            if "error" in out:
                code = out["error"].get("code")
                if code in (-32429, 429, -32603, -32005):
                    raise RuntimeError(out["error"])
                return {"__error": out["error"]}
            return out["result"]
        except (urllib.error.HTTPError, urllib.error.URLError, RuntimeError, TimeoutError, OSError) as e:
            msg = str(e).replace(URL, "<rpc>")
            if attempt == retries - 1:
                sys.stderr.write(f"rpc {method} failed: {msg}\n")
                return {"__error": msg}
            time.sleep(delay)
            delay = min(delay * 2, 10)

def get_tx(sig):
    return call("getTransaction", [sig, {"encoding": "json", "maxSupportedTransactionVersion": 1, "commitment": "confirmed"}])
