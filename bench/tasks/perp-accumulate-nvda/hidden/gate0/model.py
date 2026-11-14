"""Gate 0 for perp-accumulate-nvda: a fast simulator of the family and its ladder.

One step is one turn (2 slots, 0.5 s). Per session:

- the fair price walks (NVDA's per-second sd, a per-session level), and in an
  informed session moves M over a ramp from its onset;
- takers arrive at a per-session rate, either side at random; ahead of the
  onset an informed burst trades the move's way, and after it the noise leans
  that way; fake-out bursts (any regime) trade one way with nothing after;
- the ask side is a touch `s` bp over fair with `rho` base units per bp
  behind it; a take opens a gap that closes with the session's half-life;
- a resting bid at the touch fills against noise sells (adverse selection
  comes from the path itself).

PnL per session, in USD: the broker would buy all of Q at close*(1+h); the
desk pays its fills and fees and the remainder at close*(1+h).

    python3 model.py <seed> '<overrides as JSON>' <sessions>
"""

import json
import sys

import numpy as np

BASE = dict(
    price=159.92,
    dur=(1200, 2000),
    sd_per_s=8.29717e-05,
    vol_log_sd=0.5,
    odds=(0.30, 0.35, 0.35),  # calm, up, down
    jump_med_pct=0.2842,
    move_mult=(1.5, 3.0),
    ramp=(40, 240),
    onset=(0.15, 0.70),
    lead=(20, 150),
    burst=(6, 14),
    fake=(0, 2),
    lean=(0.10, 0.25),
    rate=(0.20, 0.45),  # taker orders per slot
    size_med=0.35,  # base units
    size_sig=0.7,
    half_spread=(3.0, 6.0),  # bp, the touch
    rho=(1.5, 4.0),  # base units per bp behind the touch
    half_life=(4.0, 40.0),  # slots
    Q=(40.0, 90.0),
    cap=(0.8, 2.0),  # pct over arrival
    haircut=(15.0, 50.0),  # bp
    taker_fee=3.5,
    maker_fee=0.5,
    slice_cap=4.0,  # largest IOC, base units
    maker_size=(2.0, 5.0),  # the grader's maker's quote, base units
    react=(2, 12),  # slots between its looks
    makers_bps=(6.0, 12.0),  # the snapshot's makers, widened
)


def draw(rng, p):
    """One session's latent state and its tape."""
    D = int(rng.integers(p["dur"][0], p["dur"][1] + 1))
    regime = rng.choice(3, p=p["odds"])
    vol = np.exp(p["vol_log_sd"] * rng.normal())
    sd_slot = p["sd_per_s"] * np.sqrt(0.25) * vol
    walk = np.cumsum(rng.standard_t(6.6, D) * sd_slot / np.sqrt(6.6 / 4.6))
    M = rng.uniform(*p["move_mult"]) * p["jump_med_pct"] / 100
    onset = int(rng.uniform(*p["onset"]) * D)
    ramp = int(rng.integers(p["ramp"][0], p["ramp"][1] + 1))
    sign = {0: 0, 1: 1, 2: -1}[int(regime)]
    move = np.zeros(D)
    if sign:
        t = np.clip((np.arange(D) - onset) / ramp, 0, 1)
        # uneven steps: a random monotone ramp
        steps = np.sort(rng.uniform(0, 1, 12))
        move = sign * M * np.interp(t, np.linspace(0, 1, 12), steps / steps[-1])
    logp = walk + move
    rate = rng.uniform(*p["rate"])
    lean = rng.uniform(*p["lean"])
    n = rng.random(D) < rate
    side = np.where(rng.random(D) < 0.5, 1, -1)
    if sign:
        after = np.arange(D) >= onset
        lean_side = np.where(rng.random(D) < 0.5 + lean, sign, -sign)
        side = np.where(after, lean_side, side)
    size = np.where(n, p["size_med"] * np.exp(p["size_sig"] * rng.normal(size=D)), 0.0)
    flow = side * size  # signed taker volume per slot
    bursts = []
    if sign:
        bursts.append((onset, sign))
    for _ in range(int(rng.integers(p["fake"][0], p["fake"][1] + 1))):
        bursts.append((int(rng.uniform(0.15, 0.85) * D), 1 if rng.random() < 0.5 else -1))
    for at, s in bursts:
        lead = int(rng.integers(p["lead"][0], p["lead"][1] + 1))
        for _ in range(int(rng.integers(p["burst"][0], p["burst"][1] + 1))):
            k = max(0, at - int(rng.integers(1, lead + 1)))
            flow[k] += s * p["size_med"] * np.exp(p["size_sig"] * rng.normal())
    return dict(
        D=D, regime=int(regime), sign=sign, M=M, onset=onset, ramp=ramp, logp=logp,
        flow=flow, rate=rate,
        s=rng.uniform(*p["half_spread"]), rho=rng.uniform(*p["rho"]),
        c=rng.uniform(*p["maker_size"]), react=int(rng.integers(p["react"][0], p["react"][1] + 1)),
        W=rng.uniform(*p["makers_bps"]),
        H=rng.uniform(*p["half_life"]), Q=float(int(rng.uniform(*p["Q"]))),
        cap=rng.uniform(*p["cap"]) / 100, h=rng.uniform(*p["haircut"]) / 1e4,
    )


class Book:
    """The ask side as the engine builds it: the grader's maker quotes `c`
    units at `s` bp over fair plus 1 bp per unit it is short, and requotes
    every `react` slots; the snapshot's makers stand at `W` bp, deep enough
    to be flat. Noise sells that reach the maker's bid cover its short."""

    def __init__(self, x, p):
        self.x, self.p = x, p
        self.short = 0.0  # units the maker is short to the bot
        self.left = x["c"]  # units on its ask until its next look
        self.next = 0

    def look(self, k, sells_to_maker):
        self.short = max(0.0, self.short - sells_to_maker)
        if k >= self.next:
            self.left = x_c = self.x["c"] if self.short < 25 else 0.0
            self.next = k + self.x["react"]

    def ask_bp(self, k):
        return min(self.x["s"] + self.short, self.x["W"]) if self.left > 0 else self.x["W"]

    def take(self, k, q):
        """Buy q now: average price in bp over fair."""
        x = self.x
        from_maker = 0.0
        cost = 0.0
        while q - from_maker > 1e-9 and self.left > 1e-9 and x["s"] + self.short < x["W"]:
            u = min(self.left, q - from_maker, 1.0)
            cost += u * (x["s"] + self.short + u / 2)
            self.short += u
            self.left -= u
            from_maker += u
        rest = q - from_maker
        cost += rest * x["W"]
        return cost / q


def run(x, p, policy):
    """Play a policy; returns (PnL USD, units bought)."""
    P0 = p["price"]
    Q, D, h = x["Q"], x["D"], x["h"]
    capx = P0 * (1 + x["cap"])
    book = Book(x, p)
    held, cost = 0.0, 0.0
    state = {}
    seen = np.zeros(D)
    for k in range(0, D, 2):
        fair = P0 * np.exp(x["logp"][k])
        sells = -x["flow"][k:k + 2][x["flow"][k:k + 2] < 0].sum()
        seen[k:k + 2] = x["flow"][k:k + 2]
        act = policy(x, p, k, held, fair, seen[:k + 2], book, state)
        take, rest = act
        if take > 0:
            take = min(take, Q - held)
        # the bot stops at the cap: it never sends a buy the cap refuses
        if take > 1e-9 and fair * (1 + book.ask_bp(k) / 1e4) <= capx:
            price = fair * (1 + book.take(k, take) / 1e4)
            held += take
            cost += take * price * (1 + p["taker_fee"] / 1e4)
        to_maker = sells
        if rest > 0 and held < Q:
            # a bid a tick inside the maker's takes the sells until the
            # maker steps back inside it: about half of them
            q = min(rest, Q - held, 0.5 * sells)
            to_maker -= q
            if q > 0:
                price = fair * (1 - x["s"] / 1e4)
                if price <= capx:
                    held += q
                    cost += q * price * (1 + p["maker_fee"] / 1e4)
        book.look(k + 2, to_maker)
    close = P0 * np.exp(x["logp"][-1])
    broker = close * (1 + h)
    pnl = Q * broker - (cost + (Q - held) * broker)
    return pnl, held


# ---- the rungs -------------------------------------------------------------

def nop(x, p, k, held, fair, seen, book, st):
    return 0.0, 0.0


def dump(x, p, k, held, fair, seen, book, st):
    """The whole order in one IOC at the open."""
    return (x["Q"], 0.0) if k == 0 else (0.0, 0.0)


def twap_cross(x, p, k, held, fair, seen, book, st):
    """Equal IOC slices over the session, each no bigger than the maker's quote."""
    left = x["Q"] - held
    steps = max(1, (x["D"] - k) // 2)
    return min(left / steps, x["c"]), 0.0


def twap_passive(x, p, k, held, fair, seen, book, st):
    """Rest at the touch; cross the shortfall over the last fifth."""
    behind = x["Q"] * min(1, k / x["D"]) - held
    if k > 0.8 * x["D"]:
        steps = max(1, (x["D"] - k) // 2)
        return (x["Q"] - held) / steps, 0.0
    return 0.0, max(0.0, behind) + 0.5


def signal(x, seen, k, half=30.0):
    """EWMA of signed taker volume, z-scored against the noise it has seen."""
    w = 0.5 ** ((k + 1 - np.arange(len(seen))) / half)
    e = (w * seen).sum()
    vol = max(np.mean(seen ** 2), 1e-6) if len(seen) > 60 else 0.05
    sd = np.sqrt(vol * (w ** 2).sum())
    return e / sd


def rate(x, st, noisy=True):
    """Units a turn the maker refills at its price: its quote over its
    look delay, which a bot measures from its own takes."""
    if "r" not in st:
        e = np.random.default_rng(int(x["Q"] * 7 + x["D"])).normal() if noisy else 0.0
        st["r"] = x["c"] * 2 / (x["react"] * np.exp(0.3 * e))
    return st["r"]


def worth(x, p, book, k, edge_bp=0.0):
    """Whether crossing now beats the broker by `edge_bp`."""
    return book.ask_bp(k) + p["taker_fee"] < x["h"] * 1e4 + edge_bp


def reference(x, p, k, held, fair, seen, book, st, thr=2.5, persist=8, move_bp=15.0):
    """Rest a bid a tick inside the maker and keep up with a back-loaded
    floor, crossing only while that beats the broker. A one-sided burst puts
    it on watch: a buy burst lifts its floor to half the order, a sell burst
    pulls its bid and stops it. The price confirms: once it has moved
    `move_bp` the burst's way with the flow still leaning that way, it buys
    out (up) or waits for the leaning to stop (down)."""
    D, Q = x["D"], x["Q"]
    z = signal(x, seen, k)
    zl = signal(x, seen, k, half=120.0)
    hist = st.setdefault("hist", [])
    hist.append(fair)
    moved = (fair / hist[max(0, len(hist) - 100)] - 1) * 1e4
    st.setdefault("watch", 0)
    st["up"] = st.get("up", 0) + 2 if z > thr else 0
    st["dn"] = st.get("dn", 0) + 2 if z < -thr else 0
    if st["up"] >= persist:
        st["watch"], st["at"] = 1, k
    elif st["dn"] >= persist:
        st["watch"], st["at"] = -1, k
    if st["watch"] and k - st["at"] > 300 and abs(zl) < 1.0:
        st["watch"] = 0
    confirmed = st["watch"] != 0 and st["watch"] * moved > move_bp and st["watch"] * zl > 0.5
    r = rate(x, st)
    floor = Q * min(1, (k + 2) / (0.95 * D)) ** 3
    if st["watch"] == 1:
        # ahead of a rise, paying up to the move it expects
        floor = max(floor, Q if confirmed else 0.5 * Q)
        if held < floor and worth(x, p, book, k, 30.0 if confirmed else 10.0):
            return min(max(r, 2.0 if confirmed else r), floor - held), 0.0
        return 0.0, 0.5
    if st["watch"] == -1 and k < 0.85 * D:
        return 0.0, 0.0
    rest = 0.5 if z > -1.0 else 0.0
    if held < floor and worth(x, p, book, k):
        return min(r, floor - held), rest
    return 0.0, rest


def told(x, p, k, held, fair, seen, book, st):
    """Knows the regime, the onset and the maker's pace."""
    D, Q = x["D"], x["Q"]
    r = rate(x, st, noisy=False)
    if x["sign"] > 0 and k < x["onset"] + x["ramp"] // 2:
        if worth(x, p, book, k, x["M"] * 1e4 * 0.8):
            return max(r, min(p["slice_cap"], Q - held)), 0.0
        return 0.0, 0.5
    if x["sign"] < 0 and k < x["onset"] + x["ramp"]:
        return 0.0, 0.0
    floor = Q * min(1, (k + 2) / (0.95 * D))
    if held < floor and worth(x, p, book, k):
        return min(r, floor - held), 0.5
    return 0.0, 0.5


RUNGS = dict(nop=nop, dump=dump, twap_cross=twap_cross, twap_passive=twap_passive,
             reference=reference, told=told)


def main():
    seed = int(sys.argv[1]) if len(sys.argv) > 1 else 1
    over = json.loads(sys.argv[2]) if len(sys.argv) > 2 else {}
    n = int(sys.argv[3]) if len(sys.argv) > 3 else 300
    p = dict(BASE, **{k: tuple(v) if isinstance(v, list) else v for k, v in over.items()})
    rng = np.random.default_rng(seed)
    xs = [draw(rng, p) for _ in range(n)]
    pnl = {r: np.array([run(x, p, f)[0] for x in xs]) for r, f in RUNGS.items()}
    reg = np.array([x["regime"] for x in xs])
    print(f"seed {seed} sessions {n} overrides {json.dumps(over)}")
    print(f"{'rung':14s} {'total':>10s} {'mean':>8s} {'sd':>8s} {'calm':>8s} {'up':>8s} {'down':>8s}")
    for r, v in pnl.items():
        by = [v[reg == i].mean() for i in range(3)]
        print(f"{r:14s} {v.sum():10.0f} {v.mean():8.2f} {v.std():8.2f} {by[0]:8.2f} {by[1]:8.2f} {by[2]:8.2f}")
    careful = max(0.0, pnl["twap_cross"].sum(), pnl["twap_passive"].sum())
    print(f"told / best careful-unaware: {pnl['told'].sum() / max(careful, 1e-9):.2f}  (need >= 2)")
    # the order rule on bootstrapped sets of 72
    B, hold = 2000, 0
    idx = rng.integers(0, n, size=(B, 72))
    t = {r: v[idx].sum(1) for r, v in pnl.items()}
    rules = {
        "dump < min(twap)": t["dump"] < np.minimum(t["twap_cross"], t["twap_passive"]),
        "max(twap) < reference": np.maximum(t["twap_cross"], t["twap_passive"]) < t["reference"],
        "reference < told": t["reference"] < t["told"],
        "nop < reference": t["nop"] < t["reference"],
    }
    ok = np.logical_and.reduce(list(rules.values()))
    print("  " + ", ".join(f"{k}: {v.mean():.3f}" for k, v in rules.items()))
    print(f"order holds on {ok.mean():.3f} of {B} bootstrapped 72-session sets (need >= 0.85)")
    k = 1.5
    w = {r: np.where(v >= 0, v, k * v).sum() for r, v in pnl.items()}
    print("v2 weighted E, as a share of told's: " + ", ".join(
        f"{r} {w[r] / w['told']:.2f}" for r in RUNGS))
    ref = pnl["reference"]
    print(f"reference per-session Sharpe {ref.mean() / ref.std():.2f}")


if __name__ == "__main__":
    main()
