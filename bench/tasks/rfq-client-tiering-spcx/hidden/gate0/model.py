"""Gate 0 for rfq-client-tiering-spcx: a fast simulator of the family and its ladder.

It follows the grader's RFQ clients (crates/grader/src/actors/library/rfq.rs) step for step:

- a step is one turn of 2 slots (0.25 s each); after a step's blocks the clients send requests on one
  Poisson schedule (each client asks `asks` times a session on average), a client picked uniformly;
- an uninformed client wants either side at random and values SPCX at the level; an informed one wants
  the side the level `lead` slots ahead is on, and values it at level + corr * (ahead - level);
- the bot answers in the same turn; at the start of the next step the client takes the answer if it is
  within its reach of its value, reach = tol bp * sqrt(size / trade size), valued at the moment the bot saw;
- the desk is scored over the idle desk: each fill is worth (index at the end - price) * size for a desk
  buy, the other way for a sale. No hedging, inventory rides at the index.

The level walks SPCX's calibration (per-second sd 1.553e-4, Student-t 5.47 steps, a per-session level with
log-sd 0.54) with 0-2 jumps of 1-3% either way.

    python3 model.py <seed> '<overrides as JSON>' <sessions>
"""

import json
import sys

import numpy as np

BASE = dict(
    price=143.0,
    dur=(1200, 2000),
    sd_per_slot=1.55334e-4 * 0.5,  # 0.25 s slots
    vol_log_sd=0.54,
    df=5.47,
    jumps=(0, 2),
    jump_pct=(1.0, 3.0),
    # None: the card's 0-2 jumps of 1-3%; a rate: SPCX's calibrated jumps (0.674 an hour, median 0.519%,
    # p90 1.001%: log-sd 0.512).
    jumps_per_hour=0.674,
    jump_median_pct=0.5194,
    jump_log_sd=0.512,
    clients=(10, 16),
    share=(0.15, 0.45),
    lead=(30, 90),
    corr=(0.6, 0.95),
    asks=(3, 10),
    trade_size=0.021,
    size_mult=20.0,  # median request, in trade sizes
    size_sigma=0.5,
    tol=(0.2, 0.5),  # bp for one trade size
    cap=40.0,  # inventory bound, SPCX
    soft=1.0,  # the skew: decline what takes |inventory| past this, SPCX
    markout=60,  # slots, the reference's markout
    ref_prior=0.3,
    ref_flag=0.5,
    ref_mu=0.8,  # an informed request's mean markout, in markout-horizon sds
    k=1.5,
    eps=0.4,  # bp inside the reach: the index is the level to a cent (0.7 bp)
    probe_delta=0.5,  # bp past the widest reach the reference probes at
    probes=2,
    told_follow=True,
    ref_flow=None,
    ref_follow=60,  # slots: the reference reads its flagged clients' requests as the sign this long  # (window, lean): the reference also declines the side recent requests lean to
)


def lo_hi(rng, r):
    return rng.uniform(r[0], r[1])


def draw(rng, p):
    D = int(rng.integers(p["dur"][0], p["dur"][1] + 1))
    lead = int(rng.integers(p["lead"][0], p["lead"][1] + 1))
    horizon = D + p["lead"][1] + p["markout"] + 4
    v = np.exp(p["vol_log_sd"] * rng.standard_normal())
    df = p["df"]
    z = rng.standard_t(df, horizon) / np.sqrt(df / (df - 2))
    logret = p["sd_per_slot"] * v * z
    if p["jumps_per_hour"] is None:
        count = int(rng.integers(p["jumps"][0], p["jumps"][1] + 1))
    else:
        count = rng.poisson(p["jumps_per_hour"] * D * 0.25 / 3600)
    for _ in range(count):
        at = int(rng.integers(1, D))
        if p["jumps_per_hour"] is None:
            size = lo_hi(rng, p["jump_pct"])
        else:
            size = p["jump_median_pct"] * np.exp(p["jump_log_sd"] * rng.standard_normal())
        logret[at] += np.sign(rng.standard_normal()) * size / 100.0
    L = p["price"] * np.exp(np.cumsum(logret))
    # The path holds its last level past the end, as level_at does past the last waypoint.
    L[D:] = L[D]
    N = int(rng.integers(p["clients"][0], p["clients"][1] + 1))
    s = lo_hi(rng, p["share"])
    k = min(N, round(s * N))
    informed = np.zeros(N, bool)
    informed[rng.permutation(N)[:k]] = True
    corr = lo_hi(rng, p["corr"])
    tol = lo_hi(rng, p["tol"])
    asks = lo_hi(rng, p["asks"])
    rate = N * asks / D  # per slot
    reqs = []
    for at in range(2, D - 1, 2):
        for _ in range(rng.poisson(rate * 2)):
            c = int(rng.integers(N))
            ahead = L[at + lead]
            if informed[c] and ahead != L[at]:
                side = 1 if ahead > L[at] else -1  # +1: the client buys
            else:
                side = 1 if rng.random() < 0.5 else -1
            n = p["size_mult"] * np.exp(p["size_sigma"] * rng.standard_normal())
            reqs.append((at, c, side, n))
    # Phoenix's index is the level in whole ticks (a cent): what the bot sees and the desk is valued at.
    I = np.round(L, 2)
    return dict(D=D, L=L, I=I, N=N, informed=informed, corr=corr, tol=tol, lead=lead, reqs=reqs, s=k / N)


def value(sess, c, at):
    L = sess["L"]
    if not sess["informed"][c]:
        return L[at]
    return L[at] + sess["corr"] * (L[at + sess["lead"]] - L[at])


def run(sess, p, policy):
    """Grade one policy on one session. `policy(view, req)` returns a quote in bp past the level
    (None declines) and an optional size cap in trade sizes."""
    L, I, D, ts = sess["L"], sess["I"], sess["D"], p["trade_size"]
    end = I[D]
    inv = 0.0
    pnl = 0.0
    view = dict(fills=[], reqs=[], inv=0.0, L=I, tol=sess["tol"], sess=sess, p=p)
    for at, c, side, n in sess["reqs"]:
        view["now"] = at
        view["inv"] = inv
        quote = policy(view, (at, c, side, n))
        view["reqs"].append((at, c, side, n))
        if quote is None:
            continue
        q, cap_n = quote if isinstance(quote, tuple) else (quote, None)
        size_n = n if cap_n is None else min(n, cap_n)
        units = size_n * ts
        # The skew: no fill takes the desk past its soft bound.
        new_inv = inv - side * units
        if abs(new_inv) > p["soft"] and abs(new_inv) > abs(inv):
            continue
        if abs(new_inv) > p["cap"]:
            continue
        reach = sess["tol"] / 1e4 * np.sqrt(size_n)
        val = value(sess, c, at)
        price = I[at] * (1 + side * q / 1e4)
        ok = price <= val * (1 + reach) if side > 0 else price >= val * (1 - reach)
        if not ok:
            view["fills"].append((at, c, side, size_n, None))
            continue
        inv = new_inv
        pnl += side * (price - end) * units
        view["fills"].append((at, c, side, size_n, price))
    return pnl




def reach_bp(view, n):
    return view["tol"] * np.sqrt(n)


def nop(view, r):
    return None


def quote_all(view, r):
    return reach_bp(view, r[3]) - view["p"]["eps"]


def quote_wide(view, r):
    return reach_bp(view, r[3]) + 3.0


def fixed_width(w):
    return lambda view, r: w


def size_tier(threshold, big_quoted):
    """Quote one size class at the reach and decline the other."""

    def f(view, r):
        big = r[3] >= threshold
        return reach_bp(view, r[3]) - view["p"]["eps"] if big == big_quoted else None

    return f


def side_skew(window, lean):
    """Quote at the reach, but decline a request on the side the recent requests lean to."""

    def f(view, r):
        at, side = r[0], r[2]
        recent = [q[2] for q in view["reqs"] if q[0] > at - window]
        if len(recent) >= 3 and side * np.mean(recent) >= lean:
            return None
        return reach_bp(view, r[3]) - view["p"]["eps"]

    return f


def told(view, r):
    """Told who the informed clients are, the lead and the correlation. It declines them, and reads their
    requests as the sign of the next lead's move: until the lead has passed, it declines an uninformed
    request on the side the last informed one asked for."""
    sess, at, side = view["sess"], r[0], r[2]
    if sess["informed"][r[1]]:
        return None
    if view["p"]["told_follow"]:
        last = [q for q in view["reqs"] if sess["informed"][q[1]] and q[0] > at - sess["lead"]]
        if last and last[-1][2] == side:
            return None
    return reach_bp(view, r[3]) - view["p"]["eps"]


class Reference:
    """Per-client request markouts, shrunk to a prior; decline once a client looks informed. Learns the
    reach from its own declines. Keeps fills small until a client has two requests on record."""

    def __init__(self, p, prior=0.3, flag=0.5, small=4.0, need=2, learn=True):
        self.p, self.prior, self.flag, self.small, self.need, self.learn = p, prior, flag, small, need, learn

    def __call__(self, view, r):
        at, c, side, n = r
        L, m = view["L"], self.p["markout"]
        if not hasattr(view, "_init"):
            pass
        st = view.setdefault("ref", dict(lo=self.p["tol"][0], hi=self.p["tol"][1] * 1.0, seen=0))
        # Reach learning from the outcome of earlier quotes: a decline means the quote was past the reach.
        while st["seen"] < len(view["fills"]):
            fat, fc, fside, fn, price = view["fills"][st["seen"]]
            st["seen"] += 1
            t = st.get(("q", fat, fc))
            if t is None:
                continue
            if price is None:
                st["hi"] = min(st["hi"], t)
                if st["lo"] >= st["hi"]:
                    st["lo"] = self.p["tol"][0]
            else:
                st["lo"] = max(st["lo"], min(t, st["hi"]))
        # The session's volatility, from the index so far.
        past = L[max(0, at - 400) : at + 1]
        if len(past) > 20:
            sd = np.std(np.diff(np.log(past)))
        else:
            sd = self.p["sd_per_slot"]
        sd_m = max(sd * np.sqrt(m), 1e-6)
        mu = self.p["ref_mu"] * sd_m  # an informed client is right about the sign
        llr = np.log(self.prior / (1 - self.prior))
        count = 0
        for qat, qc, qside, qn in view["reqs"]:
            if qc != c or qat + m > at:
                continue
            x = qside * (L[qat + m] - L[qat]) / L[qat]
            x = float(np.clip(x, -3 * sd_m, 3 * sd_m))
            llr += (x * mu - mu * mu / 2) / (sd_m * sd_m)
            count += 1
        post = 1 / (1 + np.exp(-llr))
        if post >= self.flag:
            return None
        if self.learn:
            t = 0.5 * (st["lo"] + st["hi"]) if st["hi"] - st["lo"] > 0.06 else st["lo"]
        else:
            t = view["tol"]
        st[("q", at, c)] = t
        cap = self.small if count < self.need else None
        size_n = n if cap is None else min(n, cap)
        return (t * np.sqrt(size_n) - view["p"]["eps"], cap)


class Prober(Reference):
    """The reference's markouts, and a probe: each client's first requests are answered for one trade size
    just past the widest reach the family allows. No uninformed client takes it; an informed one does when
    its view is worth more than the gap. A client that takes a probe is declined from then on."""

    def __call__(self, view, r):
        at, c, side, n = r
        pr = view.setdefault("probe", dict(count={}, sent={}, flagged=set(), seen=0))
        while pr["seen"] < len(view["fills"]):
            fat, fc, fside, fn, price = view["fills"][pr["seen"]]
            pr["seen"] += 1
            if price is not None and (fat, fc) in pr["sent"]:
                pr["flagged"].add(fc)
        if c in pr["flagged"]:
            # Keep the reference's reach learning in step.
            Reference.__call__(self, view, r)
            return None
        if pr["count"].get(c, 0) < self.p["probes"]:
            pr["count"][c] = pr["count"].get(c, 0) + 1
            pr["sent"][(at, c)] = True
            return (self.p["tol"][1] + self.p["probe_delta"], 1.0)
        quote = Reference.__call__(self, view, r)
        follow = self.p["ref_follow"]
        if quote is not None and follow:
            last = [q for q in view["reqs"] if q[1] in pr["flagged"] and q[0] > at - follow]
            if last and last[-1][2] == side:
                return None
        flow = self.p["ref_flow"]
        if quote is not None and flow is not None:
            recent = [q[2] for q in view["reqs"] if q[0] > at - flow[0]]
            if len(recent) >= 3 and side * np.mean(recent) >= flow[1]:
                return None
        return quote


def weighted(x, k):
    return np.where(x > 0, x, k * x)


def main():
    seed = int(sys.argv[1]) if len(sys.argv) > 1 else 1
    over = json.loads(sys.argv[2]) if len(sys.argv) > 2 else {}
    n_sess = int(sys.argv[3]) if len(sys.argv) > 3 else 1000
    p = dict(BASE, **{k: tuple(v) if isinstance(v, list) else v for k, v in over.items()})
    rng = np.random.default_rng(seed)
    # Generator-aware fixed policies pick their parameter on a training set of their own.
    train = [draw(rng, p) for _ in range(300)]
    test = [draw(rng, p) for _ in range(n_sess)]

    def total(policy, sessions):
        return sum(run(s, p, policy) for s in sessions)

    def best(cands):
        scored = [(total(f, train), name, f) for name, f in cands]
        scored.sort(key=lambda t: -t[0])
        return scored[0][1], scored[0][2]

    widths = [0.5, 1, 1.5, 2, 3, 4, 6, 8, 12]
    fw_name, fw = best([(f"w={w}", fixed_width(w)) for w in widths])
    st_name, st = best(
        [(f"n>={t}{'q' if b else 'd'}", size_tier(t, b)) for t in [2, 5, 10, 20, 40, 80] for b in (True, False)]
    )
    sk_name, sk = best(
        [(f"W={w},lean={l}", side_skew(w, l)) for w in (20, 40, 80, 120, 200, 300) for l in (0.1, 0.2, 0.3, 0.5, 0.7)]
    )
    rungs = dict(
        nop=nop,
        quote_all=quote_all,
        quote_wide=quote_wide,
        fixed_width=fw,
        size_tier=st,
        side_skew=sk,
        markouts=Reference(p, prior=p["ref_prior"], flag=p["ref_flag"]),
        reference=Prober(p, prior=p["ref_prior"], flag=p["ref_flag"]),
        told=told,
    )
    careful = ["quote_all", "quote_wide", "fixed_width", "size_tier", "side_skew"]
    pnl = {name: np.array([run(s, p, f) for s in test]) for name, f in rungs.items()}
    print(f"seed {seed} sessions {n_sess} overrides {json.dumps(over)}")
    print(f"picked: fixed_width {fw_name}, size_tier {st_name}, side_skew {sk_name}")
    shares = np.array([s["s"] for s in test])
    hi = shares >= np.median(shares)
    print(f"{'rung':12s} {'total':>9s} {'mean':>8s} {'sd':>7s} {'low s':>8s} {'high s':>8s}")
    for name, x in pnl.items():
        print(f"{name:12s} {x.sum():9.1f} {x.mean():8.4f} {x.std():7.3f} {x[~hi].mean():8.4f} {x[hi].mean():8.4f}")
    best_careful = max(careful, key=lambda n: pnl[n].sum())
    base = max(pnl[best_careful].sum(), 0.0)
    ratio = pnl["told"].sum() / base if base > 0 else float("inf")
    print(f"told / best careful rung ({best_careful}): {ratio:.2f}  (need >= 2)")
    blind = [n for n in careful if n != "side_skew"]
    best_blind = max(blind, key=lambda n: pnl[n].sum())
    b = max(pnl[best_blind].sum(), 0.0)
    r2 = pnl["told"].sum() / b if b > 0 else float("inf")
    print(f"told / best rung that reads nothing of the session ({best_blind}): {r2:.2f}")

    def checks(idx):
        t = {n: pnl[n][idx].sum() for n in pnl}
        mc = max(t[n] for n in careful)
        return dict(
            nop_lt_ref=t["nop"] < t["reference"],
            naive_lt_ref=max(t["quote_all"], t["quote_wide"]) < t["reference"],
            careful_lt_ref=mc < t["reference"],
            ref_lt_told=t["reference"] < t["told"],
            told_2x=t["told"] >= 2 * max(mc, 0.0),
        )

    brng = np.random.default_rng(seed + 7)
    B = 2000
    rows = [checks(brng.integers(0, n_sess, 72)) for _ in range(B)]
    order_keys = ["nop_lt_ref", "naive_lt_ref", "careful_lt_ref", "ref_lt_told"]
    holds = np.mean([all(r[k] for k in order_keys) for r in rows])
    holds2 = np.mean([all(r.values()) for r in rows])
    print("  " + ", ".join(f"{k}: {np.mean([r[k] for r in rows]):.3f}" for k in rows[0]))
    print(f"order (nop, naive, careful < reference < told) holds on {holds:.3f} of {B} 72-session sets (need >= 0.85); with told >= 2x careful: {holds2:.3f}")
    w = {n: weighted(x, p["k"]).sum() for n, x in pnl.items()}
    print("v2 weighted, share of told's: " + ", ".join(f"{n} {w[n] / w['told']:.2f}" for n in pnl))
    r = pnl["reference"]
    print(f"reference per-session Sharpe {r.mean() / r.std():.2f}; told per-session Sharpe {pnl['told'].mean() / pnl['told'].std():.2f}")
    reqs = np.mean([len(s["reqs"]) for s in test])
    print(f"requests per session {reqs:.0f}")


if __name__ == "__main__":
    main()
