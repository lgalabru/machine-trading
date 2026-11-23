"""Cached getBlock / getTransaction (json encoding, v1). Cache under /tmp/arb-research/cache."""
import os, json, gzip, sys
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import rpc
C = "/tmp/arb-research/cache"

def block(slot):
    p = f"{C}/blocks/{slot}.json.gz"
    if os.path.exists(p):
        return json.load(gzip.open(p, "rt"))
    r = rpc.call("getBlock", [slot, {"maxSupportedTransactionVersion": 1, "transactionDetails": "full",
                                     "rewards": False, "encoding": "json"}])
    if isinstance(r, dict) and "__error" in r:
        e = r["__error"]
        # skipped slot: cache as null
        if isinstance(e, dict) and e.get("code") in (-32007, -32009):
            json.dump(None, gzip.open(p, "wt")); return None
        raise RuntimeError(f"getBlock {slot}: {e}")
    json.dump(r, gzip.open(p, "wt"))
    return r

def tx(sig):
    p = f"{C}/tx/{sig}.json"
    if os.path.exists(p):
        return json.load(open(p))
    r = rpc.call("getTransaction", [sig, {"maxSupportedTransactionVersion": 1, "encoding": "json"}])
    if isinstance(r, dict) and "__error" in r:
        raise RuntimeError(f"getTransaction {sig}: {r['__error']}")
    json.dump(r, open(p, "w"))
    return r
