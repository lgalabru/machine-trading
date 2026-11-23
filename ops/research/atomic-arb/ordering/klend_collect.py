"""Full-mode klend txs over [lo,hi): keep raw tx JSON for txs with a liquidate ix (top-level or CPI,
success or failure) in liq.jsonl.gz.  usage: python3 -I klend_collect.py LO HI OUTDIR"""
import sys, os, json, gzip, time
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import rpc, lib
KLEND = "KLend2g3cP87fffoy8q1mQqGKjrxjC8boSyAYavgmjD"
LIQ = {bytes.fromhex("a2a1238f1ebbb967"), bytes.fromhex("b1479abce2854a37")}
lo, hi, out = int(sys.argv[1]), int(sys.argv[2]), sys.argv[3]
os.makedirs(out, exist_ok=True)
sp = os.path.join(out, "state.json")
st = json.load(open(sp)) if os.path.exists(sp) else {"token": None, "n": 0, "nliq": 0, "done": False}
if st["done"]: print("done already", st); sys.exit()
def is_liq(tx):
    ks = lib.all_keys(tx)
    ixs = list(tx["transaction"]["message"]["instructions"])
    for g in tx["meta"].get("innerInstructions") or []: ixs += g["instructions"]
    for ix in ixs:
        if ks[ix["programIdIndex"]] == KLEND and ix.get("data") and lib.b58d(ix["data"])[:8] in LIQ: return True
    return False
f = gzip.open(os.path.join(out, "liq.jsonl.gz"), "at")
while True:
    p = {"transactionDetails": "full", "limit": 100, "encoding": "json", "maxSupportedTransactionVersion": 1,
         "sortOrder": "asc", "filters": {"slot": {"gte": lo, "lt": hi}}}
    if st["token"]: p["paginationToken"] = st["token"]
    g = rpc.call("getTransactionsForAddress", [KLEND, p])
    if "__error" in g: print("error", g["__error"], flush=True); time.sleep(3); continue
    for t in g["data"]:
        st["n"] += 1
        if is_liq(t): f.write(json.dumps(t) + "\n"); st["nliq"] += 1
    f.flush()
    st["token"] = g.get("paginationToken")
    if not g["data"] or not st["token"]: st["done"] = True
    json.dump(st, open(sp, "w"))
    if st["n"] % 5000 < 100: print(st["n"], st["nliq"], g["data"][-1]["slot"] if g["data"] else None, flush=True)
    if st["done"]: break
print("done", st["n"], st["nliq"])
