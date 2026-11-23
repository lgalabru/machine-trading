"""Cached getTransaction (jsonParsed) with a small thread pool."""
import json, os, sys
from concurrent.futures import ThreadPoolExecutor
sys.path.insert(0, os.path.dirname(__file__))
import rpc
D = os.path.join(os.path.dirname(__file__), "../data/tx"); os.makedirs(D, exist_ok=True)

def get(sig):
    p = f"{D}/{sig}.json"
    if os.path.exists(p):
        return json.load(open(p))
    t = rpc.call("getTransaction", [sig, {"encoding": "jsonParsed", "maxSupportedTransactionVersion": 1, "commitment": "confirmed"}])
    if t is not None: json.dump(t, open(p, "w"))
    return t

def get_many(sigs, workers=8):
    with ThreadPoolExecutor(workers) as ex:
        return list(ex.map(get, sigs))
