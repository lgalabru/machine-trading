"""Summarise round1 top-200 Jito arbs that are liquidations or flash-loan funded (comparison set).
usage: python3 -I round1_cmp.py <scripts_dir> <round1_dir> <out.json>"""
import sys, json, os, collections
sys.path.insert(0, sys.argv[1])
from b58 import b58decode
R1 = sys.argv[2]
recs = json.load(open(os.path.join(R1, "top200.recs.json")))
KL = "KLend2g3cP87fffoy8q1mQqGKjrxjC8boSyAYavgmjD"; JF = "jupgfSgfuAXv4B6R2Uxu85Z1qdzgju79s6MfZekN6XS"
out = []
for x in recs:
    names = [n for _, n in x["ixnames"]]
    if x["cat"] != "liquidation" and not any("lash" in n for n in names): continue
    t = json.load(open(os.path.join(R1, "tx", x["sig"] + ".json")))
    m = t["transaction"]["message"]; la = t["meta"].get("loadedAddresses") or {}
    keys = m["accountKeys"] + la.get("writable", []) + la.get("readonly", [])
    fl = []
    for i, ix in enumerate(m["instructions"]):
        keys = [k["pubkey"] if isinstance(k, dict) else k for k in keys]
        p = ix.get("programId") or keys[ix["programIdIndex"]]
        acc = [a if isinstance(a, str) else keys[a] for a in ix.get("accounts", [])]
        d = b58decode(ix["data"]) if ix.get("data") else b""
        if p == KL and d[:8] == bytes.fromhex("87e734a70734d4c1"):
            fl.append({"lender": "kamino", "reserve": acc[3], "mint": acc[4], "amount": int.from_bytes(d[8:16], "little")})
        if p == JF and len(d) >= 16 and "FlashloanBorrow" in names:
            fl.append({"lender": "jupiter-lend", "amount_raw": int.from_bytes(d[8:16], "little")})
    out.append({"sig": x["sig"], "slot": x["slot"], "cat": x["cat"], "profit_sol": x["profit"], "signer": x["signer"],
                "venues": x["venues"], "tip_sol": x["tip"], "fee_sol": x["fee"], "cu": x["cu"], "botprog": x.get("botprog"),
                "flash": fl, "liq_programs": [n for n in x["ixnames"] if "iquidat" in n[1]]})
json.dump(out, open(sys.argv[3], "w"), indent=1)
c = collections.Counter((o["cat"], o["flash"][0]["lender"] if o["flash"] else "-") for o in out)
print(c)
print("signers", collections.Counter((o["cat"], o["signer"][:8]) for o in out).most_common(12))
for cat in ("liquidation", "flashloan-funded arb"):
    xs = [o for o in out if o["cat"] == cat]
    if xs: print(cat, len(xs), "profit_sol sum %.2f" % sum(o["profit_sol"] for o in xs), "tip>0:", sum(1 for o in xs if o["tip_sol"]), "median cu", sorted(o["cu"] for o in xs)[len(xs)//2])
kfl = [f for o in out for f in o["flash"] if f["lender"] == "kamino"]
print("kamino flash reserves", collections.Counter(f["reserve"][:8] for f in kfl), "amounts", sorted(f["amount"] for f in kfl)[:3], "...")
