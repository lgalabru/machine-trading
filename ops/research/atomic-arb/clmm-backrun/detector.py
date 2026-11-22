"""Backrun detector for two concentrated-liquidity SOL/USDC pools.

Rule: after any swap that touches pool A or B, with each pool's state (sqrt price s, active liquidity L, fee f):
  1. gap_bps = |pA/pB - 1| * 1e4.  No trade unless (1-fA)(1-fB) * p_hi/p_lo > 1, i.e. gap > fA+fB (8 bps for 4+4).
  2. treat each pool as a constant-product pool with virtual reserves X = L/s, Y = L*s inside the current range;
     the SOL->USDC(hi)->SOL(lo) cycle has the closed-form optimum
        x* = (sqrt(a*b) - b) / c,  a = gh*gl*Yh*Xl,  b = Xh*Yl,  c = gh*(Yl + gl*Yh)
  3. expected net = gross(x*) * (1 - leader_share) - base_fee; fire if net > min_profit_sol.
  Optional tick-aware refinement: walk initialized ticks (depth.py) instead of constant L.
State inputs available right after the trigger, without looking at the arb itself:
  - Raydium CLMM: SwapEvent gives post sqrt_price_x64 and liquidity exactly.
  - Whirlpool: Traded gives pre/post sqrt price; L is the last value implied by a Traded event (out / d(sqrt)),
    or the pool account's liquidity field when reading live state.
usage: detector.py cases | bg [--share 0.35] [--min 0.01]
"""
import sys, os, json, math, argparse, collections
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import arbmath, replay
D = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "data")
W, R = "Czfq3xZZDmsdGdUyrNLtRhGc47cXcZtLG4crryfu44zE", "3ucNos4NbumPLZNWztqGHNFFgkHeRMBQAVemeeomsUxv"

def detect(pa, La, fa, pb, Lb, fb, leader_share=0.35, base_fee=5e-6, min_profit=0.01):
    """pa/pb: USDC per SOL; La/Lb raw liquidity; fa/fb fee fractions. Returns dict."""
    o = arbmath.optimal((math.sqrt(pa / 1e3), La, fa), (math.sqrt(pb / 1e3), Lb, fb))
    net = o["profit_sol"] * (1 - leader_share) - base_fee
    o.update(net_expected_sol=net, fire=net > min_profit,
             sell_sol_in="A" if o["direction"] == "sell_in_1" else "B")
    return o

def run_cases(args):
    rows = json.load(open(os.path.join(D, "cases.json")))
    allblocks = replay.load(os.path.join(D, "cases.jsonl"))
    out = []
    for r in rows:
        blocks = {s: b for s, b in allblocks.items() if r["slot"] - 8 <= s <= r["slot"] + 3}
        tl = replay.timeline(blocks)
        price = {}; L = {}
        for t in tl:
            for e in t["evs"]: price.setdefault(e["pool"], e["pre"])
        # state strictly before the arb tx, using only information from earlier txs
        for t in tl:
            if t["tx"]["sig"] == r["sig"]: arb = t; break
            for e in t["evs"]:
                price[e["pool"]] = e["post"]
                if e["L"]: L[e["pool"]] = e["L"]
        prof = json.load(open(os.path.join(D, "depth_profile.json")))
        Lw = L.get(W) or prof["whirl_" + W]["liquidity"]; Lr = L.get(R) or prof["rclmm_" + R]["liquidity"]
        d = detect(price[W], Lw, replay.fee_of(W), price[R], Lr, replay.fee_of(R), args.share, min_profit=args.min)
        first = arb["evs"][0]
        actual_sell = "A" if (first["pool"] == W) == (first["sol_in"] > 0) else "B"
        out.append(dict(slot=r["slot"], signer=r["signer"][:6], gap=round(d["gap_bps"], 1), fire=d["fire"],
                        dir_ok=d["sell_sol_in"] == actual_sell, pred_in=round(d["x_sol"], 1), act_in=r["arb_in_sol"],
                        pred_gross=round(d["profit_sol"], 4), act_gross=r["measured_gross_sol"],
                        L_w_source="event" if W in L else "now", L_r_source="event" if R in L else "now"))
    for o in out: print(o)
    hits = sum(o["fire"] and o["dir_ok"] for o in out)
    ratio = sorted(o["act_gross"] / o["pred_gross"] for o in out if o["pred_gross"] > 0)
    print("cases", len(out), "hits", hits, "median actual/pred gross", round(ratio[len(ratio) // 2], 3) if ratio else None)
    json.dump(out, open(os.path.join(D, "detector_cases.json"), "w"), indent=1)

def run_bg(args):
    blocks = replay.load(args.file or os.path.join(D, "bg.jsonl"))
    tl = replay.timeline(blocks)
    price = {}; L = {}
    for t in tl:
        for e in t["evs"]: price.setdefault(e["pool"], e["pre"])
    fired = []; prev_fire = False; n_evals = 0
    gaps = []
    for k, t in enumerate(tl):
        touched = False
        for e in t["evs"]:
            price[e["pool"]] = e["post"]
            if e["L"]: L[e["pool"]] = e["L"]
            touched |= e["pool"] in (W, R)
        if not touched or W not in L or R not in L: continue
        n_evals += 1
        d = detect(price[W], L[W], replay.fee_of(W), price[R], L[R], replay.fee_of(R), args.share, min_profit=args.min)
        gaps.append(d["gap_bps"])
        if d["fire"] and not prev_fire:
            # who closed it, and when
            nxt = None
            for t2 in tl[k + 1:]:
                ps = {e["pool"] for e in t2["evs"]}
                if ps & {W, R}:
                    nxt = t2; break
            fired.append(dict(slot=t["slot"], idx=t["idx"], trigger=t["tx"]["signer"][:8], trig_sig=t["tx"]["sig"],
                              gap=round(d["gap_bps"], 1), pred_in=round(d["x_sol"], 1), pred_gross=round(d["profit_sol"], 4),
                              next_slot=nxt and nxt["slot"], next_idx=nxt and nxt["idx"], next_signer=nxt and nxt["tx"]["signer"][:8],
                              next_pools=nxt and sorted({e["pool"][:6] for e in nxt["evs"]}),
                              next_gross=nxt and round(nxt["tx"]["d"]["sol"] + nxt["tx"]["fee"] + nxt["tx"]["tip"]
                                                       + (nxt["tx"]["d"].get("usdc", 0) + nxt["tx"]["d"].get("usdt", 0)) / price[W], 4),
                              next_err=nxt and nxt["tx"]["err"], next_tip=nxt and nxt["tx"]["tip"], next_fee=nxt and nxt["tx"]["fee"],
                              next_progs=nxt and [p_[:8] for p_ in nxt["tx"]["progs"]]))
        prev_fire = d["fire"]
    gaps.sort()
    hist = collections.Counter(min(int(g // 2) * 2, 40) for g in gaps)
    summ = dict(n_evals=n_evals, n_fire_episodes=len(fired),
                gap_bps_q={str(p): round(gaps[int(p * (len(gaps) - 1))], 2) for p in (0.5, 0.9, 0.99, 0.999, 1.0)},
                frac_gap_above_8=round(sum(g > 8 for g in gaps) / len(gaps), 4), gap_hist_2bps=sorted(hist.items()), fired=fired)
    json.dump(summ, open(os.path.join(D, args.out), "w"), indent=1)
    print(json.dumps({k: v for k, v in summ.items() if k != "fired"}))
    for f in fired: print(f)

if __name__ == "__main__":
    ap = argparse.ArgumentParser(); ap.add_argument("mode"); ap.add_argument("--share", type=float, default=0.35)
    ap.add_argument("--min", type=float, default=0.01); ap.add_argument("--file"); ap.add_argument("--out", default="detector_bg.json"); a = ap.parse_args()
    {"cases": run_cases, "bg": run_bg}[a.mode](a)
