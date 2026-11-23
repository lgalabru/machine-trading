"""Split swaps on the two main pools into directional (signer's SOL/USD balance moves with the swap) vs
flow-neutral (signer ends ~flat: cross-venue arb/MM bots, incl. legs against undecoded prop AMMs)."""
import sys, os, json, collections
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import replay, lib
D = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "data")
W, R = "Czfq3xZZDmsdGdUyrNLtRhGc47cXcZtLG4crryfu44zE", "3ucNos4NbumPLZNWztqGHNFFgkHeRMBQAVemeeomsUxv"
path = sys.argv[1] if len(sys.argv) > 1 else os.path.join(D, "bg.jsonl")
tl = replay.timeline(replay.load(path))
stat = collections.defaultdict(lambda: collections.Counter()); vol = collections.defaultdict(lambda: collections.Counter())
sizes = collections.defaultdict(list); signers = collections.defaultdict(collections.Counter)
for t in tl:
    tx = t["tx"]
    for e in t["evs"]:
        if e["pool"] not in (W, R): continue
        s = abs(e["sol_in"]); dsol = tx["d"].get("sol", 0); dusd = tx["d"].get("usdc", 0) + tx["d"].get("usdt", 0)
        if s < 1e-6: continue
        directional = abs(dsol) > 0.5 * s or abs(dusd) > 0.5 * s * e["pre"]
        router = "jup" if lib.JUP6 in tx["progs"] + tx["inner"] else ("raydium_router" if "routeUGWgWzqBWFcrCfv8tritsqukccJPu3q5GPP3xS" in tx["progs"] else "direct")
        k = ("directional/" + router) if directional else "flow_neutral"
        stat[e["pool"][:6]][k] += 1; vol[e["pool"][:6]][k] += round(s * e["pre"])
        if directional: sizes[e["pool"][:6]].append(s)
        signers[e["pool"][:6]][(tx["signer"][:8], k)] += 1
out = {}
for p in stat:
    xs = sorted(sizes[p])
    out[p] = dict(counts=dict(stat[p]), usd=dict(vol[p]), directional_size_sol_q={q: round(xs[int(q * (len(xs) - 1))], 2) for q in (0.5, 0.75, 0.9, 0.99, 1.0)} if xs else None,
                  n_directional_ge={str(th): sum(x >= th for x in xs) for th in (10, 50, 100, 200, 500)},
                  top_signers=signers[p].most_common(8))
print(json.dumps(out, indent=1))
json.dump(out, open(os.path.join(D, "flowclass_" + os.path.basename(path).replace(".jsonl", ".json")), "w"), indent=1)
