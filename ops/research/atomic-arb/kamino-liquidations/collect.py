"""Page every klend program transaction (Helius getTransactionsForAddress, full) backwards
from now to a cutoff unix time. Writes:
  <out>/liq_txs.jsonl.gz   trimmed txs that carry a klend liquidate ix (top-level or CPI), success or failure
  <out>/all_compact.jsonl.gz one line per klend tx: sig, slot, idx, time, err, signer, klend ix names
  <out>/state.json          pagination token for resume
usage: python3 -I collect.py <scripts_dir> <out_dir> <cutoff_unix> [slot_lo slot_hi]
"""
import sys, json, gzip, os, time
sys.path.insert(0, sys.argv[1])
import rpc
from b58 import b58decode

KLEND = "KLend2g3cP87fffoy8q1mQqGKjrxjC8boSyAYavgmjD"
LIQ = {bytes.fromhex("a2a1238f1ebbb967"), bytes.fromhex("b1479abce2854a37")}
out, cutoff = sys.argv[2], int(sys.argv[3])
slot_lo, slot_hi = (int(sys.argv[4]), int(sys.argv[5])) if len(sys.argv) > 5 else (None, None)
os.makedirs(out, exist_ok=True)
state_p = os.path.join(out, "state.json")
state = json.load(open(state_p)) if os.path.exists(state_p) else {"token": None, "n": 0, "nliq": 0}

def keys_of(tx):
    m = tx["transaction"]["message"]
    la = tx["meta"].get("loadedAddresses") or {}
    return m["accountKeys"] + la.get("writable", []) + la.get("readonly", [])

def all_ixs(tx):
    m = tx["transaction"]["message"]
    for i, ix in enumerate(m["instructions"]):
        yield ix
    for g in tx["meta"].get("innerInstructions") or []:
        for ix in g["instructions"]:
            yield ix

def klend_names(logs):
    names = []
    for i, l in enumerate(logs):
        if l.startswith("Program " + KLEND + " invoke") and i + 1 < len(logs) and logs[i + 1].startswith("Program log: Instruction: "):
            names.append(logs[i + 1][len("Program log: Instruction: "):])
    return names

def trim(tx):
    meta = tx["meta"]
    return {
        "slot": tx["slot"], "idx": tx.get("transactionIndex"), "blockTime": tx.get("blockTime"),
        "version": tx.get("version"),
        "sig": tx["transaction"]["signatures"][0],
        "keys": keys_of(tx), "header": tx["transaction"]["message"]["header"],
        "ixs": tx["transaction"]["message"]["instructions"],
        "inner": meta.get("innerInstructions"), "logs": meta.get("logMessages"),
        "err": meta.get("err"), "fee": meta.get("fee"), "cu": meta.get("computeUnitsConsumed"),
        "pre": meta.get("preBalances"), "post": meta.get("postBalances"),
        "preTok": meta.get("preTokenBalances"), "postTok": meta.get("postTokenBalances"),
    }

fl = gzip.open(os.path.join(out, "liq_txs.jsonl.gz"), "at")
fc = gzip.open(os.path.join(out, "all_compact.jsonl.gz"), "at")
while True:
    params = {"transactionDetails": "full", "limit": 100, "encoding": "json",
              "maxSupportedTransactionVersion": 1, "sortOrder": "desc"}
    if slot_lo is not None:
        params["filters"] = {"slot": {"gte": slot_lo, "lt": slot_hi}}
    if state["token"]:
        params["paginationToken"] = state["token"]
    g = rpc.call("getTransactionsForAddress", [KLEND, params])
    if "__error" in g:
        print("error", g["__error"], flush=True); time.sleep(5); continue
    data = g["data"]
    if not data:
        break
    oldest = None
    for tx in data:
        keys = keys_of(tx)
        is_liq = False
        for ix in all_ixs(tx):
            if keys[ix["programIdIndex"]] == KLEND and b58decode(ix["data"])[:8] in LIQ:
                is_liq = True; break
        logs = tx["meta"].get("logMessages") or []
        rec = {"sig": tx["transaction"]["signatures"][0], "slot": tx["slot"], "idx": tx.get("transactionIndex"),
               "t": tx.get("blockTime"), "err": tx["meta"].get("err"), "signer": keys[0],
               "kl": klend_names(logs), "liq": is_liq}
        fc.write(json.dumps(rec) + "\n")
        if is_liq:
            fl.write(json.dumps(trim(tx)) + "\n"); state["nliq"] += 1
        state["n"] += 1
        oldest = tx.get("blockTime") or oldest
    state["token"] = g.get("paginationToken")
    state["oldest"] = oldest
    fl.flush(); fc.flush()
    json.dump(state, open(state_p, "w"))
    if state["n"] % 5000 < 100:
        print(state["n"], state["nliq"], oldest, time.strftime("%m-%d %H:%M", time.gmtime(oldest or 0)), flush=True)
    if not state["token"] or (oldest and oldest < cutoff):
        break
print("done", state, flush=True)
