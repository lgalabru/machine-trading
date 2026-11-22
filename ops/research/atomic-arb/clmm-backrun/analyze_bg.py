"""Background distribution over consecutive blocks: swaps on SOL/USDC pools, W/R gap episodes, competition.
usage: analyze_bg.py [bg.jsonl ...] -> data/bg_summary.json, data/bg_swaps.csv, data/bg_episodes.csv"""
import json, os, sys, csv, statistics, collections, math
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import replay, arbmath, lib
D = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "data")
W, R = "Czfq3xZZDmsdGdUyrNLtRhGc47cXcZtLG4crryfu44zE", "3ucNos4NbumPLZNWztqGHNFFgkHeRMBQAVemeeomsUxv"
BAND = 8.0

def q(xs, ps=(0.5, 0.9, 0.99)):
    xs = sorted(xs)
    return {str(p): (round(xs[min(len(xs) - 1, int(p * len(xs)))], 3) if xs else None) for p in ps}

def classify(t):
    tx = t["tx"]; evs = t["evs"]
    if not evs: return "noop" if not tx["err"] else "failed"
    pools = {e["pool"] for e in evs}
    net = sum(e["sol_in"] for e in evs); gross = sum(abs(e["sol_in"]) for e in evs)
    if len(pools) >= 2 and gross > 0 and abs(net) < 0.02 * gross: return "arb"
    if lib.JUP6 in tx["progs"] + tx["inner"]: return "jup"
    if "routeUGWgWzqBWFcrCfv8tritsqukccJPu3q5GPP3xS" in tx["progs"]: return "raydium_router"
    return "other"

def main(paths):
    blocks = {}
    for p in paths: blocks.update(replay.load(p))
    slots = sorted(blocks); n_slots = slots[-1] - slots[0] + 1
    tl = replay.timeline(blocks)
    # seed start prices
    last = {}
    for t in tl:
        for e in t["evs"]:
            last.setdefault(e["pool"], e["pre"])
    swaps = []; per_pool = collections.defaultdict(list); cls_count = collections.Counter()
    Lw, Lr = [], []
    episodes = []; ep = None
    fail_wr = collections.Counter(); noop_wr = collections.Counter(); signers_fail = collections.Counter()
    for t in tl:
        tx = t["tx"]; c = classify(t); cls_count[c] += 1
        if not t["evs"]:
            if set(tx["touch"]) & {W, R}:
                (fail_wr if tx["err"] else noop_wr)[t["slot"]] += 1
                if tx["err"]: signers_fail[tx["signer"]] += 1
            continue
        for e in t["evs"]:
            imp = (e["post"] / e["pre"] - 1) * 1e4
            row = dict(slot=t["slot"], idx=t["idx"], sig=tx["sig"], signer=tx["signer"], cls=c, pool=e["pool"], prog=e["prog"],
                       sol_in=round(e["sol_in"], 4), usd=round(abs(e["sol_in"]) * e["pre"], 2), pre=round(e["pre"], 5), post=round(e["post"], 5),
                       impact_bps=round(imp, 3), L=int(e["L"]) if e["L"] else None, tip=tx["tip"], fee=tx["fee"])
            swaps.append(row); per_pool[e["pool"]].append(row)
            if e["pool"] == W and e["L"] and abs(imp) > 0.2: Lw.append(e["L"])
            if e["pool"] == R and e["L"]: Lr.append(e["L"])
            last[e["pool"]] = e["post"]
        if W in last and R in last:
            g = (last[W] / last[R] - 1) * 1e4
            if abs(g) > BAND + 0.5 and ep is None:
                ep = dict(start_slot=t["slot"], start_idx=t["idx"], start_sig=tx["sig"], opener=tx["signer"], opener_cls=c,
                          opener_sol=round(sum(e["sol_in"] for e in t["evs"] if e["pool"] in (W, R)), 3),
                          opener_pool=",".join(sorted({e["pool"][:6] for e in t["evs"] if e["pool"] in (W, R)})),
                          gap0=round(g, 2), max_gap=abs(g), n_txs_touching=0)
                Lw_ = Lw[-1] if Lw else None; Lr_ = Lr[-1] if Lr else None
                if Lw_ and Lr_:
                    o = arbmath.optimal((math.sqrt(last[W] / 1e3), Lw_, 4e-4), (math.sqrt(last[R] / 1e3), Lr_, 4e-4))
                    ep["opt_profit_sol"] = round(o["profit_sol"], 4); ep["opt_in_sol"] = round(o["x_sol"], 2)
            elif ep is not None:
                ep["max_gap"] = max(ep["max_gap"], abs(g)); ep["n_txs_touching"] += 1
                if abs(g) <= BAND + 0.5:
                    ep.update(end_slot=t["slot"], end_idx=t["idx"], closer=tx["signer"], closer_cls=c, closer_sig=tx["sig"],
                              closer_tip=tx["tip"], closer_fee=tx["fee"], closer_gross=round(tx["d"]["sol"] + tx["fee"] + tx["tip"], 5),
                              dslots=t["slot"] - ep["start_slot"], gap_after=round(g, 2))
                    episodes.append(ep); ep = None
    # summaries
    names = {p: m for p, m in replay.META.items()}
    pools_sum = []
    for p, rows in sorted(per_pool.items(), key=lambda kv: -sum(r["usd"] for r in kv[1])):
        m = names.get(p, {})
        user = [r for r in rows if r["cls"] not in ("arb",)]
        pools_sum.append(dict(pool=p, prog=m.get("prog"), quote="USDC" if lib.USDC in (m.get("mint_a"), m.get("mint_b")) else "USDT",
            fee_bps=m.get("fee_rate", 0) / 100, tick_spacing=m.get("tick_spacing") or m.get("bin_step"), n_swaps=len(rows),
            usd_volume=round(sum(r["usd"] for r in rows)), n_user_swaps=len(user), user_usd=round(sum(r["usd"] for r in user)),
            swaps_per_min=round(len(rows) / (n_slots * 0.4 / 60), 2),
            user_size_sol_q=q([abs(r["sol_in"]) for r in user], (0.5, 0.9, 0.99, 1.0)),
            n_user_ge_100sol=sum(1 for r in user if abs(r["sol_in"]) >= 100), n_user_ge_300sol=sum(1 for r in user if abs(r["sol_in"]) >= 300),
            cls=dict(collections.Counter(r["cls"] for r in rows))))
    def impact_per_100(pool):
        xs = [abs(r["impact_bps"]) / abs(r["sol_in"]) * 100 for r in per_pool.get(pool, []) if abs(r["sol_in"]) >= 20]
        return q(xs)
    summary = dict(first_slot=slots[0], last_slot=slots[-1], n_slots_fetched=len(slots), n_slots_span=n_slots,
                   minutes=round(n_slots * 0.4 / 60, 1),
                   tx_classes=dict(cls_count), pools=pools_sum[:15],
                   W_impact_bps_per_100sol=impact_per_100(W), R_impact_bps_per_100sol=impact_per_100(R),
                   W_L_q=q(Lw), R_L_q=q(Lr),
                   failed_txs_touching_WR_per_slot=round(sum(fail_wr.values()) / len(slots), 2),
                   noop_ok_txs_touching_WR_per_slot=round(sum(noop_wr.values()) / len(slots), 2),
                   top_failing_signers=signers_fail.most_common(10),
                   n_episodes=len(episodes), open_episode=ep,
                   episode_dslots=dict(collections.Counter(e["dslots"] for e in episodes)),
                   episode_closer_cls=dict(collections.Counter(e["closer_cls"] for e in episodes)),
                   episode_opener_cls=dict(collections.Counter(e["opener_cls"] for e in episodes)),
                   episode_gap0_q=q([abs(e["gap0"]) for e in episodes]),
                   episode_opt_profit_q=q([e.get("opt_profit_sol", 0) for e in episodes]))
    # gap distribution sampled at each W/R event
    json.dump(summary, open(os.path.join(D, "bg_summary.json"), "w"), indent=1)
    with open(os.path.join(D, "bg_swaps.csv"), "w") as f:
        w = csv.DictWriter(f, list(swaps[0].keys())); w.writeheader(); [w.writerow(r) for r in swaps]
    with open(os.path.join(D, "bg_episodes.csv"), "w") as f:
        keys = sorted({k for e in episodes for k in e})
        w = csv.DictWriter(f, keys); w.writeheader(); [w.writerow(e) for e in episodes]
    print(json.dumps(summary, indent=1)[:6000])
    for e in episodes: print({k: e.get(k) for k in ("start_slot", "start_idx", "opener", "opener_cls", "opener_sol", "opener_pool", "gap0", "opt_profit_sol", "dslots", "closer", "closer_cls", "closer_gross", "closer_tip", "closer_fee")})

if __name__ == "__main__":
    main(sys.argv[1:] or [os.path.join(D, "bg.jsonl")])
