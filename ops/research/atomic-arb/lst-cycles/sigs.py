"""Page getSignaturesForAddress back to a slot; cache to data/sigs/<addr>.json."""
import json, os, sys
sys.path.insert(0, os.path.dirname(__file__))
import rpc
D = os.path.join(os.path.dirname(__file__), "../data/sigs"); os.makedirs(D, exist_ok=True)

def sigs_until(addr, min_slot, before=None, max_pages=400):
    path = f"{D}/{addr}.json"
    cache = json.load(open(path)) if os.path.exists(path) else []
    if cache and cache[-1]["slot"] <= min_slot and before is None: return [s for s in cache if s["slot"] >= min_slot]
    out = [] if before else []
    b = before
    for _ in range(max_pages):
        p = {"limit": 1000}
        if b: p["before"] = b
        res = rpc.call("getSignaturesForAddress", [addr, p])
        if not res: break
        out += [{"sig": r["signature"], "slot": r["slot"], "err": r["err"] is not None, "bt": r.get("blockTime")} for r in res]
        b = res[-1]["signature"]
        if res[-1]["slot"] < min_slot: break
    if before is None: json.dump(out, open(path, "w"))
    return [s for s in out if s["slot"] >= min_slot]

if __name__ == "__main__":
    s = sigs_until(sys.argv[1], int(sys.argv[2]))
    print(len(s), s[-1]["slot"] if s else None)
