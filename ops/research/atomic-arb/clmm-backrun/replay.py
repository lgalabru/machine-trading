"""Replay compact block records into per-pool state timelines (sqrt price, liquidity estimate)."""
import json, math, os, sys
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import lib, pools
Q = 2.0 ** 64
META = pools.get([])

def load(path):
    blocks = {}
    for l in open(path):
        b = json.loads(l)
        if not b.get("skipped"): blocks[b["slot"]] = b
    return blocks

def sol_is_a(pool):
    return META[pool].get("mint_a") == lib.SOL

def fee_of(pool):
    m = META[pool]
    return m.get("fee_rate", 0) / 1e6

def norm_s(pool, s):
    """return sqrt(USDC_raw/SOL_raw) whatever the token order."""
    return s if sol_is_a(pool) else 1.0 / s

def apply_event(state, e):
    """Update state[pool] = dict(s, L) from an event; return (pre_s, post_s, L, sol_in_signed)."""
    p = e["pool"]; st = state.get(p, {})
    a_is_sol = sol_is_a(p) if e["prog"] != "dlmm" else META[p].get("mint_a") == lib.SOL
    if e["prog"] == "whirl":
        pre, post = int(e["pre_sqrt"]) / Q, int(e["post_sqrt"]) / Q
        L = st.get("L")
        if abs(pre - post) / pre > 2e-7:
            if e["a_to_b"]: L = e["amt_out"] / (pre - post)            # B out = L * dS
            else: L = e["amt_out"] / (1 / pre - 1 / post)               # A out = L * d(1/S)
            L = abs(L)
    elif e["prog"] == "rclmm":
        post = int(e["post_sqrt"]) / Q; L = int(e["liquidity"])
        if "s" in st: pre = st["s_raw"]
        else:
            if e["zero_for_one"]: pre = post + e["amt_out"] / L if L else post
            else: pre = 1 / (1 / post + e["amt_out"] / L) if L else post
    elif e["prog"] == "dlmm":
        bs = META[p]["bin_step"]
        f = lambda b: math.sqrt((1 + bs / 1e4) ** b)
        pre, post, L = f(e["start_bin"]), f(e["end_bin"]), None
    else:
        return None
    # sol in (positive = SOL sold into pool)
    if a_is_sol: sol_in = e["amt_in"] if e["a_to_b"] else -e["amt_out"]
    else: sol_in = -e["amt_out"] if e["a_to_b"] else e["amt_in"]
    st.update(s_raw=post, s=norm_s(p, post), L=L)
    state[p] = st
    return norm_s(p, pre), norm_s(p, post), L, sol_in / 1e9

def px(s): return s * s * 1e3  # USDC per SOL for SOL(9dp)/USD(6dp)

def timeline(blocks):
    """Ordered list of (slot, idx, tx, [(event, pre_px, post_px, L, sol_in)]) and final state."""
    state = {}; out = []
    for slot in sorted(blocks):
        for tx in blocks[slot]["txs"]:
            evs = []
            for e in tx["ev"]:
                r = apply_event(state, e)
                if r: evs.append(dict(pool=e["pool"], prog=e["prog"], pre=px(r[0]), post=px(r[1]), L=r[2], sol_in=r[3],
                                      amt_in=e["amt_in"], amt_out=e["amt_out"]))
            out.append(dict(slot=slot, idx=tx["idx"], tx=tx, evs=evs))
    return out
