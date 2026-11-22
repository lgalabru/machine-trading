"""Minimal JSON-RPC helper. Reads the URL from the Solana CLI config; never prints it."""
import json, os, re, time, urllib.request, urllib.error

def _url():
    cfg = open(os.path.expanduser("~/.config/solana/cli/config.yml")).read()
    return re.search(r"json_rpc_url:\s*\"?([^\"\n]+)", cfg).group(1).strip()

URL = _url()

def call(method, params, retries=6):
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).encode()
    for i in range(retries):
        try:
            req = urllib.request.Request(URL, body, {"Content-Type": "application/json"})
            with urllib.request.urlopen(req, timeout=120) as r:
                out = json.loads(r.read())
            if "error" in out:
                e = out["error"]
                if e.get("code") in (-32429, 429, -32005) or "rate" in str(e).lower():
                    time.sleep(1.5 * (i + 1)); continue
                raise RuntimeError(f"{method}: {e}")
            return out["result"]
        except urllib.error.HTTPError as e:
            if e.code == 429 or e.code >= 500:
                time.sleep(1.5 * (i + 1)); continue
            raise RuntimeError(f"{method}: HTTP {e.code}") from None
        except (urllib.error.URLError, TimeoutError):
            time.sleep(1.5 * (i + 1))
    raise RuntimeError(f"{method}: retries exhausted")
