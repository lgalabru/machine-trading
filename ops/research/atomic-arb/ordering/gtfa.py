"""Helius getTransactionsForAddress (full) for an address over [lo, hi) slots, cached as jsonl.gz.
usage: python3 -I gtfa.py ADDRESS LO HI OUT.jsonl.gz"""
import sys, os, json, gzip, time
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import rpc
addr, lo, hi, outp = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), sys.argv[4]
if os.path.exists(outp): print("exists"); sys.exit()
tok = None; n = 0
with gzip.open(outp + ".part", "wt") as f:
    while True:
        p = {"transactionDetails": "full", "limit": 100, "encoding": "json", "maxSupportedTransactionVersion": 1,
             "sortOrder": "asc", "filters": {"slot": {"gte": lo, "lt": hi}}}
        if tok: p["paginationToken"] = tok
        g = rpc.call("getTransactionsForAddress", [addr, p])
        if "__error" in g: print("error", g["__error"]); time.sleep(3); continue
        for t in g["data"]: f.write(json.dumps(t) + "\n"); n += 1
        tok = g.get("paginationToken")
        if not g["data"] or not tok: break
os.rename(outp + ".part", outp); print("n", n)
