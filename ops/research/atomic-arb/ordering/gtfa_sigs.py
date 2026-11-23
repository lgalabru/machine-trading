"""Signatures-mode getTransactionsForAddress over [lo,hi) slots -> jsonl.gz (sig, slot, idx, err).
usage: python3 -I gtfa_sigs.py ADDRESS LO HI OUT"""
import sys, os, json, gzip, time
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import rpc
addr, lo, hi, outp = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), sys.argv[4]
tok = None; n = 0
with gzip.open(outp, "wt") as f:
    while True:
        p = {"transactionDetails": "signatures", "limit": 1000, "sortOrder": "asc", "filters": {"slot": {"gte": lo, "lt": hi}}}
        if tok: p["paginationToken"] = tok
        g = rpc.call("getTransactionsForAddress", [addr, p])
        if "__error" in g: print("error", g["__error"], flush=True); time.sleep(3); continue
        for t in g["data"]:
            f.write(json.dumps([t["signature"], t["slot"], t["transactionIndex"], t["err"], t["blockTime"]]) + "\n"); n += 1
        tok = g.get("paginationToken")
        if n % 20000 < 1000: print(n, g["data"][-1]["slot"] if g["data"] else None, flush=True)
        if not g["data"] or not tok: break
print("n", n)
