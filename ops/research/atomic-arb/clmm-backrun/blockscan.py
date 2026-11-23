"""Fetch full blocks and keep a compact record of every tx touching a SOL/USD(C|T) pool.
usage: python3 -I blockscan.py OUT.jsonl SLOT_FROM SLOT_TO [workers]"""
import sys, os, json, concurrent.futures as cf
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import lib, rpc, pools

JITO_TIP = {"96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5", "HFqU5x63VTqvQss8hp11i4wVV8bD44PvwucfZ2bU7gRe",
    "Cw8CFyM9FkoMi7K7Crf6HNQqf4uEMzpKw6QNghXLvLkY", "ADaUMid9yfUytqMBgopwjb2DTLSokTSzL1zt6iGPaS49",
    "DfXygSm4jCyNCybVYYK6DwvWqjKee8pbDmJGcLWNDXjh", "ADuUkR4vqLUMWXxW9gh6D6L8pMSawimctcNZ5pGwDcEt",
    "DttWaMuVvTiduZRnguLF7jNxTgiMBZ1hyAumKUiL2KRL", "3AVi9Tg9Uo68tJfuvoKvqKNWKkC5wPdSSdeBnizKZ6jT"}
RAMM4_SOLUSDC = "58oQChx4yWmvKdwLLZzBi4ChoCc2fqCUWBkwMihLYQo2"
RAMM4_VAULTS = {"DQyrAcCrDXQ7NeoqGgDCZwBvWDcYmFCjSb9JtteuvPpz": "sol", "HLmqeL62xR1QoZ1HKKbXRrdN1p3phKpxRMb2VVopvBBz": "usdc"}
SEED_MAIN = {"Czfq3xZZDmsdGdUyrNLtRhGc47cXcZtLG4crryfu44zE", "3ucNos4NbumPLZNWztqGHNFFgkHeRMBQAVemeeomsUxv", RAMM4_SOLUSDC}

def fetch(slot):
    r = rpc.call("getBlock", [slot, {"maxSupportedTransactionVersion": 1, "transactionDetails": "full",
                                     "rewards": False, "encoding": "json"}])
    return slot, r

def signer_deltas(tx, signer):
    m = tx["meta"]; ks = lib.all_keys(tx)
    d = {"sol": (m["postBalances"][0] - m["preBalances"][0]) / 1e9}
    agg = {}
    for side, sgn in (("preTokenBalances", -1), ("postTokenBalances", 1)):
        for b in m.get(side) or []:
            if b.get("owner") != signer: continue
            mint = b["mint"]; amt = int(b["uiTokenAmount"]["amount"])
            agg[mint] = agg.get(mint, 0) + sgn * amt
    for mint, v in agg.items():
        if mint == lib.SOL: d["sol"] += v / 1e9
        elif mint == lib.USDC: d["usdc"] = v / 1e6
        elif mint == lib.USDT: d["usdt"] = v / 1e6
        elif v: d.setdefault("other", 0); d["other"] += 1
    return d

def compact(slot, blk, main):
    out = []
    for i, tx in enumerate(blk["transactions"]):
        ks = lib.all_keys(tx)
        evs = lib.events_from_tx(tx) if tx["meta"]["err"] is None else []
        meta = pools.get([(e["prog"], e["pool"]) for e in evs]) if evs else {}
        evs = [e for e in evs if pools.is_main(meta[e["pool"]])]
        touch = sorted(set(ks) & main)
        if not evs and not touch: continue
        for e in evs:
            main.add(e["pool"])
            for k in ("pre_sqrt", "post_sqrt", "liquidity"):
                if k in e: e[k] = str(e[k])
        m = tx["meta"]
        tip = sum(m["postBalances"][j] - m["preBalances"][j] for j, k in enumerate(ks[:len(m["postBalances"])]) if k in JITO_TIP) / 1e9
        progs = sorted({ix.get("programId") or ks[ix["programIdIndex"]] for ix in tx["transaction"]["message"]["instructions"]})
        inner = sorted({ix.get("programId") or ks[ix["programIdIndex"]] for ii in m.get("innerInstructions") or [] for ix in ii["instructions"]})
        rec = dict(slot=slot, idx=i, sig=tx["transaction"]["signatures"][0], signer=ks[0], err=m["err"] is not None,
                   fee=m["fee"] / 1e9, cu=m.get("computeUnitsConsumed"), tip=tip, progs=progs, inner=inner,
                   touch=touch, ev=evs, d=signer_deltas(tx, ks[0]), nlogs=len(m.get("logMessages") or []),
                   trunc=any("Log truncated" in l for l in m.get("logMessages") or []))
        if RAMM4_SOLUSDC in ks:  # constant-product v4: state from vault post balances
            v = {}
            for b in m.get("postTokenBalances") or []:
                k = ks[b["accountIndex"]]
                if k in RAMM4_VAULTS: v[RAMM4_VAULTS[k]] = int(b["uiTokenAmount"]["amount"])
            pre = {}
            for b in m.get("preTokenBalances") or []:
                k = ks[b["accountIndex"]]
                if k in RAMM4_VAULTS: pre[RAMM4_VAULTS[k]] = int(b["uiTokenAmount"]["amount"])
            if len(v) == 2 and v != pre: rec["ramm4"] = dict(post=v, pre=pre)
        out.append(rec)
    return dict(slot=slot, ntx=len(blk["transactions"]), blockTime=blk.get("blockTime"), parent=blk.get("parentSlot"), txs=out)

def main_(outp, a, b, workers=4):
    main = set(SEED_MAIN) | {p for p, m in pools.get([]).items() if pools.is_main(m)}
    done = set()
    if os.path.exists(outp):
        for l in open(outp): done.add(json.loads(l)["slot"])
    slots = [s for s in range(a, b + 1) if s not in done]
    with open(outp, "a") as f, cf.ThreadPoolExecutor(workers) as ex:
        for slot, blk in ex.map(fetch, slots):
            if not isinstance(blk, dict) or "__error" in blk or blk is None:
                f.write(json.dumps(dict(slot=slot, skipped=True, err=str(blk)[:200])) + "\n"); continue
            f.write(json.dumps(compact(slot, blk, main)) + "\n"); f.flush()

if __name__ == "__main__":
    main_(sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), int(sys.argv[4]) if len(sys.argv) > 4 else 4)
