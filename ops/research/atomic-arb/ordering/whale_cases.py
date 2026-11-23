"""Find whale chunks on R (>=150 SOL single swap by one signer, no W leg) in the whale windows and the
first successful W<->R cycle by another signer afterwards (same or next 3 slots)."""
import sys, os, json, gzip
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import lib
W = "Czfq3xZZDmsdGdUyrNLtRhGc47cXcZtLG4crryfu44zE"; R = "3ucNos4NbumPLZNWztqGHNFFgkHeRMBQAVemeeomsUxv"
out = []
for name in ("whale_a", "whale_b"):
    txs = [json.loads(l) for l in gzip.open(f"/tmp/arb-research/cache/gtfa/R_{name}.jsonl.gz", "rt")]
    txs.sort(key=lambda t: (t["slot"], t["transactionIndex"]))
    rows = []
    for t in txs:
        if t["meta"]["err"] is not None: continue
        evs = lib.events_from_tx(t)
        pools = [e["pool"] for e in evs]
        rows.append((t["slot"], t["transactionIndex"], lib.all_keys(t)[0], t["transaction"]["signatures"][0], evs, pools))
    for i, (s, ix, signer, sig, evs, pools) in enumerate(rows):
        rev = [e for e in evs if e["pool"] == R]
        if len(rev) != 1 or W in pools: continue
        e = rev[0]
        sol = (e["amt0"] if e["zero_for_one"] else e["amt1"]) / 1e9
        if sol < 150: continue
        # first W<->R cycle after it
        for (s2, ix2, sg2, sig2, ev2, p2) in rows[i + 1:]:
            if s2 > s + 3: break
            if sg2 != signer and set(p2) == {W, R}:
                out.append(dict(source=name, whale_sig=sig, whale_signer=signer, whale_slot=s, whale_idx=ix,
                                whale_sol=round(sol, 1), sig=sig2, signer=sg2, slot=s2, idx=ix2))
                break
        else:
            print("no W<->R responder after", name, s, ix, signer[:8], round(sol))
for o in out: print(o["source"], o["whale_slot"], o["whale_idx"], o["whale_signer"][:8], o["whale_sol"], "->", o["slot"], o["idx"], o["signer"][:8], o["sig"][:10])
json.dump(out, open("/tmp/arb-research/explorer/whale_cases.json", "w"), indent=1)
