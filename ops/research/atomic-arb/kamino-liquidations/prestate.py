"""Reconstruct what made each liquidation episode's first liquidation possible.
For each episode start (LtvExceeded), fetch every successful Scope-program tx in [slot-W, slot] (cached), parse
RefreshPriceList logs (tk, old, new, source slot) per oracle account, and walk the obligation's health backwards
from the liquidation-time composition (refresh_obligation logs) to find the Scope update that flipped it.
usage: python3 -I prestate.py <scripts_dir> <liquidations_ltv.json> <episodes.json> <reserves.json> <cache_dir> <out.json> [W]
"""
import sys, json, gzip, os, re, collections
sys.path.insert(0, sys.argv[1])
import rpc
from b58 import b58decode

SCOPE = "HFn8GnPADiny6XqUoWE8uRPPxb29ikn4yTuPa9MF2fWJ"
D_RPL = bytes.fromhex("53bacf83cbfec682")
LIQ = json.load(open(sys.argv[2])); EPS = json.load(open(sys.argv[3])); RES = json.load(open(sys.argv[4]))
CACHE = sys.argv[5]; OUTP = sys.argv[6]; W = int(sys.argv[7]) if len(sys.argv) > 7 else 1500
os.makedirs(CACHE, exist_ok=True)
RX = re.compile(r"tk (\d+), (\w+): (\d+) to (\d+) \| prev_slot: (\d+), new_slot: (\d+), crt_slot: (\d+)")

def scope_updates(tx):
    m = tx["transaction"]["message"]; la = tx["meta"].get("loadedAddresses") or {}
    keys = m["accountKeys"] + la.get("writable", []) + la.get("readonly", [])
    inner = {g["index"]: g["instructions"] for g in tx["meta"].get("innerInstructions") or []}
    accts = []
    for i, ix in enumerate(m["instructions"]):
        for x in [ix] + inner.get(i, []):
            if keys[x["programIdIndex"]] == SCOPE:
                d = b58decode(x["data"]) if x.get("data") else b""
                accts.append(keys[x["accounts"][0]] if d[:8] == D_RPL and x["accounts"] else None)
    out, k, cur = [], -1, None
    for l in tx["meta"].get("logMessages") or []:
        if l.startswith("Program " + SCOPE + " invoke"):
            k += 1; cur = accts[k] if k < len(accts) else None
            continue
        mm = RX.search(l)
        if mm and cur:
            tk, kind, old, new, ps, ns, cs = mm.groups()
            out.append([cur, int(tk), kind, int(old), int(new), int(ps), int(ns), int(cs)])
    return keys[0], out

def fetch_window(lo, hi):
    path = os.path.join(CACHE, f"{lo}_{hi}.json.gz")
    if os.path.exists(path):
        return json.load(gzip.open(path, "rt"))
    rows, token = [], None
    while True:
        p = {"transactionDetails": "full", "limit": 100, "encoding": "json", "maxSupportedTransactionVersion": 1,
             "sortOrder": "asc", "filters": {"slot": {"gte": lo, "lt": hi}, "status": "succeeded"}}
        if token: p["paginationToken"] = token
        g = rpc.call("getTransactionsForAddress", [SCOPE, p])
        if "__error" in g: raise SystemExit(g)
        for tx in g["data"]:
            signer, ups = scope_updates(tx)
            if ups:
                rows.append({"slot": tx["slot"], "idx": tx.get("transactionIndex"), "sig": tx["transaction"]["signatures"][0],
                             "signer": signer, "ups": ups})
        token = g.get("paginationToken")
        if not token or not g["data"]: break
    json.dump(rows, gzip.open(path, "wt"))
    return rows

liq_by_sig = {x["sig"]: x for x in LIQ}
# reserve lookup by (market, name)
by_name = collections.defaultdict(dict)
for a, R in RES.items():
    by_name[R["lending_market"]][R["name"]] = a

def chain_of(res):
    R = RES.get(res)
    if not R: return None
    return [(R["scope_prices"], e) for e in R["price_chain"] if e != 65535]

targets = sorted((liq_by_sig[e["first_sig"]] for e in EPS if e["first_sig"] in liq_by_sig), key=lambda x: x["slot"])
# merge windows
wins = []
for x in targets:
    lo, hi = x["slot"] - W, x["slot"] + 1
    if wins and lo <= wins[-1][1]: wins[-1][1] = max(wins[-1][1], hi)
    else: wins.append([lo, hi])
print(len(targets), "targets in", len(wins), "windows,", sum(b - a for a, b in wins), "slots", flush=True)
chunks = sorted({a for lo, hi in wins for a in range(lo // 5000 * 5000, hi, 5000)})
print(len(chunks), "chunks", flush=True)
from concurrent.futures import ThreadPoolExecutor
events = []
with ThreadPoolExecutor(int(os.environ.get("PRESTATE_WORKERS", "10"))) as ex:
    for i, rows in enumerate(ex.map(lambda a: fetch_window(a, a + 5000), chunks)):
        events += rows
        if i % 10 == 0: print("chunk", i, len(events), flush=True)
events.sort(key=lambda e: (e["slot"], e["idx"] or 0))
by_entry = collections.defaultdict(list)  # (acct, tk) -> [(slot, idx, sig, signer, old, new, src_slot, crt)]
for e in events:
    for acct, tk, kind, old, new, ps, ns, cs in e["ups"]:
        if old != new:
            by_entry[(acct, tk)].append((e["slot"], e["idx"] or 0, e["sig"], e["signer"], old, new, ns, cs, ps))

def walk(x):
    deps, bors, rr = x.get("deposits") or [], x.get("borrows") or [], x.get("refresh_reserves") or []
    if not deps or not bors or len(rr) != len(deps) + len(bors):
        return {"status": "no_composition"}
    pos = []  # (kind, reserve, value_for_health)
    for (s, amt, val), res in zip(deps, rr[:len(deps)]):
        lt = RES.get(res, {}).get("liq_threshold_pct", 0) / 100
        pos.append(["dep", res, val * lt])
    for (s, amt, val, vbf), res in zip(bors, rr[len(deps):]):
        pos.append(["bor", res, vbf])
    U = sum(p[2] for p in pos if p[0] == "dep"); B = sum(p[2] for p in pos if p[0] == "bor")
    el_ratio = 1 + (x["health_overshoot"] or 0)  # bf_debt/unhealthy from the log
    # normalise deposit weights so U/B matches the program's own figure (covers e-mode thresholds)
    if U > 0 and B > 0:
        k = (B / el_ratio) / U
        for p in pos:
            if p[0] == "dep": p[2] *= k
    entries = {}
    for i, p in enumerate(pos):
        for ent in chain_of(p[1]) or []:
            entries.setdefault(ent, []).append(i)
    ups = []
    for ent, idxs in entries.items():
        for u in by_entry.get(ent, []):
            if (u[0], u[1]) <= (x["slot"], x["idx"]) and u[0] >= x["slot"] - W:
                ups.append((u, ent, idxs))
    ups.sort(key=lambda z: (z[0][0], z[0][1]), reverse=True)
    vals = [p[2] for p in pos]
    def health(v):
        u = sum(v[i] for i, p in enumerate(pos) if p[0] == "dep"); b = sum(v[i] for i, p in enumerate(pos) if p[0] == "bor")
        return u / b if b else None
    h_liq = health(vals)
    path = []
    for (slot, idx, sig, signer, old, new, src_slot, crt, prev_slot), ent, idxs in ups:
        r = old / new
        for i in idxs: vals[i] *= r
        hb = health(vals)
        path.append({"slot": slot, "idx": idx, "entry": ent[1], "move": new / old - 1, "h_before": hb})
        if hb is not None and hb > 1:
            return {"status": "trigger_found", "h_at_liq": h_liq, "trigger_slot": slot, "trigger_idx": idx,
                    "trigger_sig": sig, "trigger_signer": signer, "trigger_is_liq_tx": sig == x["sig"],
                    "trigger_signer_is_winner": signer == x["signer"],
                    "trigger_entry": ent[1], "trigger_account": ent[0], "trigger_move": new / old - 1,
                    "trigger_src_age_slots": crt - src_slot, "trigger_prev_age_slots": crt - prev_slot,
                    "lag_slots": x["slot"] - slot, "n_updates_walked": len(path), "h_before_trigger": hb,
                    "n_scope_entries": len(entries)}
    return {"status": "liquidatable_before_window", "h_at_liq": h_liq, "h_at_window_start": health(vals),
            "n_updates_walked": len(path), "n_scope_entries": len(entries)}

out = []
for x in targets:
    rec = {"sig": x["sig"], "slot": x["slot"], "idx": x["idx"], "obligation": x["obligation"], "signer": x["signer"],
           "pair": f'{x["repay_sym"]}<-{x["withdraw_sym"]}', "market": x["market"], "debt_usd": x["debt_usd"],
           "repaid_usd": x["repaid_usd"], "overshoot_at_liq": x["health_overshoot"], "n_deposits": x["n_deposits"],
           "n_borrows": x["n_borrows"]}
    rec.update(walk(x))
    out.append(rec)
json.dump(out, open(OUTP, "w"), indent=0)
print(collections.Counter(r["status"] for r in out))
