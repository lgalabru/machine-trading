"""For each multi-token cycle in round1: preceding swaps on the same pools (same slot or up to LOOKBACK slots
before), their size and price impact, and the per-leg pre-trade mid prices of the arb. Writes data/round1_triggers.json"""
import json, math, os, sys
from concurrent.futures import ThreadPoolExecutor
sys.path.insert(0, os.path.dirname(__file__))
import rpc
from gettx import get, get_many
from legs import legs

D = os.path.join(os.path.dirname(__file__), "../data")
LOOKBACK = 150

def prev_on_pool(pool, sig, slot):
    try: r = rpc.call("getSignaturesForAddress", [pool, {"before": sig, "limit": 15}])
    except Exception as e: return []
    return [x for x in r if x["err"] is None and x["slot"] >= slot - LOOKBACK]

def leg_mid(l):
    """pre-trade mid (out per in) and its source."""
    if l.get("pre_px"): return l["pre_px"], "event_pre"
    if l.get("ray"):
        r = l["ray"]  # reserves before: coin, pc ; direction 1: coin->pc? use vault balances orientation
        return None, None
    if l.get("exec_px") and l.get("post_px"):
        return l["exec_px"] ** 2 / l["post_px"], "exec2/post"
    return None, None

def analyse(c):
    try: return _analyse(c)
    except Exception as e: return {"sig": c["sig"], "error": repr(e)}

def _analyse(c):
    tx = json.load(open(f"/tmp/arb-research/round1/tx/{c['sig']}.json")); tx = tx.get("result", tx); idx = tx.get("transactionIndex")
    out = {"sig": c["sig"], "slot": c["slot"], "idx": idx, "signer": c["signer"], "profit_sol": c["profit_sol"], "path": c["path"],
           "venues": c["venues"], "pools": c["pools"], "lst": c["lst"], "slot_in_epoch": c["slot_in_epoch"], "set": c["set"]}
    legs_info = []
    for l in c["legs"]:
        m, src = leg_mid(l)
        e = {"venue": l["venue"], "pool": l["pool"], "in": l["mint_in"], "out": l["mint_out"],
             "amt_in_ui": l["amt_in"] / 10 ** (l["dec_in"] or 0), "exec_px": l.get("exec_px"), "pre_mid": m, "mid_src": src,
             "post_px": l.get("post_px")}
        if l.get("exec_px") and l.get("post_px"): e["impact_bps"] = (l["post_px"] / l["exec_px"] - 1) * 1e4
        legs_info.append(e)
    out["legs"] = legs_info
    # gross edge from the best mids available (falls back to exec, which already includes fee+impact)
    lg = [math.log(e["pre_mid"] or e["exec_px"]) for e in legs_info if (e["pre_mid"] or e["exec_px"])]
    out["edge_bps_mid_or_exec"] = (math.exp(sum(lg)) - 1) * 1e4 if len(lg) == len(legs_info) else None
    out["edge_bps_exec"] = (math.exp(sum(math.log(e["exec_px"]) for e in legs_info if e["exec_px"])) - 1) * 1e4
    # triggers
    trig = []
    for pool in dict.fromkeys(c["pools"]):
        for x in prev_on_pool(pool, c["sig"], c["slot"]):
            t = get(x["signature"])
            if t is None: continue
            try: ls, sg = legs(t)
            except Exception: continue
            mine = [q for q in ls if q["pool"] == pool]
            if not mine: continue
            q = mine[0]
            path = [ls[0]["mint_in"]] + [z["mint_out"] for z in ls]
            imp = None
            if q.get("pre_px") and q.get("post_px"): imp = (q["post_px"] / q["pre_px"] - 1) * 1e4
            elif q.get("exec_px") and q.get("post_px"): imp = 2 * (q["post_px"] / q["exec_px"] - 1) * 1e4
            trig.append({"pool": pool, "slot": x["slot"], "slot_lag": c["slot"] - x["slot"], "idx": t.get("transactionIndex"),
                         "same_block_before": x["slot"] == c["slot"] and (t.get("transactionIndex") or 0) < (idx or 0),
                         "signer": sg, "is_cycle": len(ls) >= 2 and path[0] == path[-1], "in": q["mint_in"], "out": q["mint_out"],
                         "amt_in_ui": q["amt_in"] / 10 ** (q["dec_in"] or 0), "impact_bps_on_pool": imp, "venue": q["venue"]})
    trig.sort(key=lambda z: (z["slot_lag"], -(z["idx"] or 0)))
    out["preceding"] = trig[:12]
    nc = [z for z in trig if not z["is_cycle"]]
    out["trigger"] = nc[0] if nc else None
    out["prev_any"] = trig[0] if trig else None
    return out

def main():
    cyc = [c for c in json.load(open(f"{D}/round1_cycles.json")) if len(set(c["path"])) >= 3 and c["path"] and c["path"][0] == c["path"][-1]]
    print(len(cyc), "multi-token cycles", flush=True)
    with ThreadPoolExecutor(6) as ex: res = list(ex.map(analyse, cyc))
    json.dump(res, open(f"{D}/round1_triggers.json", "w"), indent=1)

if __name__ == "__main__":
    main()
