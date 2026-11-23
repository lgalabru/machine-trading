import sys, os, json, concurrent.futures as cf
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import cache, lib
W = "Czfq3xZZDmsdGdUyrNLtRhGc47cXcZtLG4crryfu44zE"; R = "3ucNos4NbumPLZNWztqGHNFFgkHeRMBQAVemeeomsUxv"
rows = json.load(open(sys.argv[1]))
def f(r):
    try: return r, cache.tx(r["txSignature"])
    except Exception as e: return r, None
out = []
with cf.ThreadPoolExecutor(4) as ex:
    for r, t in ex.map(f, rows):
        if t is None: print("fail", r["txSignature"]); continue
        evs = lib.events_from_tx(t)
        pools = [e["pool"] for e in evs]
        ks = set(lib.all_keys(t))
        if evs and set(pools) <= {W, R} and {W, R} <= set(pools):
            out.append(dict(r, pools=pools))
        elif W in ks or R in ks:
            print("touches W/R but other pools:", r["txSignature"][:10], r["signer"][:8], [p[:6] for p in pools])
print(len(out))
for o in sorted(out, key=lambda o: o["slot"]): print(o["slot"], o["timestamp"], o["signer"][:8], round(o["profitAmount"], 3), [p[:4] for p in o["pools"]])
json.dump(out, open(sys.argv[2], "w"), indent=1)
