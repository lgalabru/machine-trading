"""Gate 0 calibration model for book-or-oracle-tsla, redesign 1: the
competitor and the Phoenix makers peg to the Phoenix oracle.

A fast simulator of the family, vectorised over sessions, in half-second
steps (a bot turn of 2 slots). It runs the ladder's rungs on the same draws
and checks the card's Gate 0: told >= 2x the best careful-but-unaware rung,
nop and the naive rungs below the reference, and the order rule on >= 85%
of bootstrapped 100-session sets. It also reports the v2 strength band
(reference weighted PnL 0.2-0.7 of the informed bot's, Sharpe >= 0.25).

    python3 model.py <seed> [sessions] ['<family overrides as JSON>']
"""

import json
import sys

import numpy as np

MARK = 388.95
TICK = 0.01
CAP = 8.0
DD = 250.0
MAKER_FEE = 0.5e-4
TAKER_FEE = 3.5e-4
SLOT_S = 0.25
STEP = 2  # slots per step

P = dict(
    duration=(1000, 1600),
    sd_per_s=1.13956e-4,
    vol_logsd=0.37,
    vol_mult=(0.8127, 1.23),
    df=5.649,
    basis_sd_bp=(1.0, 4.0),  # the book's ordinary wobble around the index
    basis_tau_s=8.0,
    gap_s=(10, 45),  # episode start to next start
    p=(0.2, 0.8),  # odds an episode is oracle-wrong
    kinds=(1 / 3, 1 / 3, 1 / 3),  # stale, delayed, off_price
    hits=(0.4, 0.2, 0.4),  # phoenix, pyth, both
    stale_s=(5, 40),
    lag_slots=(4, 40),
    delayed_s=(5, 30),
    off_pct=(0.2, 1.0),
    off_s=(5, 30),
    crowd_pct=(0.2, 1.0),
    ramp_s=(2, 10),
    hold_s=(2, 15),
    half_life_s=(5, 30),
    crowd_rate=(0.15, 0.35),  # crowd orders per slot while pushing
    noise_rate=(0.2, 0.45),
    arb_rate=(0.1, 0.3),
    size_median=0.25,
    size_sigma=0.7,
    size_clamp=(0.03, 1.2),
    comp_bps=(4.0, 12.0),
    floor_bps=(1.5, 3.0),
    react_steps=(1, 4),
    limit_sigma=0.5,
    limit_clamp=(0.3, 4.0),
    pyth_cadence=4,
    peg="oracle",
)


def u(rng, r, n):
    return rng.uniform(r[0], r[1], n)


class World:
    """Shared draws: paths, oracles and the taker tape."""

    def __init__(self, rng, n, P=P):
        self.n = n
        D = rng.integers(P["duration"][0], P["duration"][1] + 1, n)
        self.T = T = D // STEP
        Tm = T.max()
        self.Tm = Tm
        alive = np.arange(Tm)[None, :] < T[:, None]
        self.alive = alive
        reg = np.where(rng.random(n) < 0.5, P["vol_mult"][0], P["vol_mult"][1])
        sd = P["sd_per_s"] * np.sqrt(STEP * SLOT_S) * reg * np.exp(P["vol_logsd"] * rng.standard_normal(n))
        df = P["df"]
        z = rng.standard_t(df, (n, Tm)) / np.sqrt(df / (df - 2))
        logf = np.cumsum(sd[:, None] * z, axis=1)
        logf -= logf[:, :1]
        F = MARK * np.exp(logf)
        self.F = F
        # The book's ordinary basis wobble (OU), in fractions.
        bsd = u(rng, P["basis_sd_bp"], n) * 1e-4
        a = np.exp(-STEP * SLOT_S / P["basis_tau_s"])
        w = np.zeros((n, Tm))
        e = rng.standard_normal((n, Tm)) * bsd[:, None] * np.sqrt(1 - a * a)
        for t in range(1, Tm):
            w[:, t] = a * w[:, t - 1] + e[:, t]
        prem = w.copy()  # book premium over fair
        O = F.copy()  # Phoenix oracle
        Py = F.copy()  # Pyth price
        Pyage = np.zeros((n, Tm))  # Pyth publish age, steps
        # Pyth publishes every cadence slots normally.
        cad = P["pyth_cadence"] // STEP
        base_age = (np.arange(Tm) % cad)[None, :].repeat(n, 0).astype(float)
        Pyage[:] = base_age
        for t in range(Tm):
            Py[:, t] = F[:, t - int(base_age[0, t])] if t >= base_age[0, t] else F[:, 0]
        self.p = u(rng, P["p"], n)
        # Episode labels per step: 0 none, 1 oracle-wrong, 2 book-wrong.
        lab = np.zeros((n, Tm), int)
        hitl = np.full((n, Tm), -1)
        crowd_side = np.zeros((n, Tm))
        hl = u(rng, P["half_life_s"], n)
        crowd_rate = u(rng, P["crowd_rate"], n)
        self.episodes = []
        sps = 1 / (STEP * SLOT_S)  # steps per second
        for i in range(n):
            t = int(rng.uniform(*P["gap_s"]) * sps)
            eps = []
            while t < T[i] - 20:
                if rng.random() < self.p[i]:
                    k = rng.choice(3, p=P["kinds"])
                    hit = rng.choice(3, p=P["hits"])
                    if k == 0:
                        dur = int(rng.uniform(*P["stale_s"]) * sps)
                    elif k == 1:
                        dur = int(rng.uniform(*P["delayed_s"]) * sps)
                        lag = max(1, int(rng.integers(P["lag_slots"][0], P["lag_slots"][1] + 1) / STEP))
                    else:
                        dur = int(rng.uniform(*P["off_s"]) * sps)
                        bias = rng.uniform(*P["off_pct"]) / 100 * (1 if rng.random() < 0.5 else -1)
                    end = min(t + dur, T[i] - 10)
                    for s in range(t, end):
                        if k == 0:
                            val = F[i, t - 1]
                            age = s - t + 1
                        elif k == 1:
                            val = F[i, max(0, s - lag)]
                            age = lag
                        else:
                            val = F[i, s] * (1 + bias)
                            age = None
                        if hit in (0, 2):
                            O[i, s] = val
                        if hit in (1, 2):
                            Py[i, s] = val
                            if age is not None:
                                Pyage[i, s] = age
                        lab[i, s] = 1
                        hitl[i, s] = hit
                    eps.append(("oracle", ["stale", "delayed", "off"][k], ["phoenix", "pyth", "both"][hit], t, end))
                    t_end = end
                else:
                    off = rng.uniform(*P["crowd_pct"]) / 100
                    sgn = 1 if rng.random() < 0.5 else -1
                    ramp = max(1, int(rng.uniform(*P["ramp_s"]) * sps))
                    hold = int(rng.uniform(*P["hold_s"]) * sps)
                    decay = np.exp(-np.log(2) / (hl[i] * sps))
                    s = t
                    cur = 0.0
                    while s < T[i] - 10:
                        if s < t + ramp:
                            cur = off * (s - t + 1) / ramp
                            crowd_side[i, s] = sgn
                        elif s < t + ramp + hold:
                            cur = off
                            crowd_side[i, s] = sgn
                        else:
                            cur *= decay
                            if cur < 0.0002:
                                break
                        prem[i, s] += sgn * cur
                        lab[i, s] = 2
                        s += 1
                    eps.append(("book", sgn * off, t, s))
                    t_end = s
                t = max(t + int(rng.uniform(*P["gap_s"]) * sps), t_end + int(2 * sps))
            self.episodes.append(eps)
        # Heal at the end: last 10 steps clean.
        self.O, self.Py, self.Pyage, self.lab, self.hit = O, Py, Pyage, lab, hitl
        self.B = (O if P["peg"] == "oracle" else F) * (1 + prem)
        self.prem = prem
        self.crowd_side = crowd_side
        # The tape: per step up to K orders of each kind.
        K = 4
        self.K = K
        cbps = u(rng, P["comp_bps"], n) * 1e-4
        self.comp = cbps
        self.floor = u(rng, P["floor_bps"], n) * 1e-4
        self.react = rng.integers(P["react_steps"][0], P["react_steps"][1] + 1, n)

        def sizes(shape):
            s = P["size_median"] * np.exp(P["size_sigma"] * rng.standard_normal(shape))
            return np.clip(s, *P["size_clamp"])

        nr = u(rng, P["noise_rate"], n) * STEP
        ar = u(rng, P["arb_rate"], n) * STEP
        cnt = rng.poisson(nr[:, None], (n, Tm)).clip(0, K)
        acnt = rng.poisson(ar[:, None], (n, Tm)).clip(0, K)
        ccnt = (rng.poisson(crowd_rate[:, None] * STEP, (n, Tm)) * (crowd_side != 0)).clip(0, K)
        # Orders: side (+1 buy), size, limit price.
        lim_m = np.clip(np.exp(P["limit_sigma"] * rng.standard_normal((n, Tm, K))), *P["limit_clamp"])
        self.noise = (
            np.where(rng.random((n, Tm, K)) < 0.5, 1.0, -1.0),
            sizes((n, Tm, K)),
            lim_m * cbps[:, None, None],
            np.arange(K)[None, None, :] < cnt[:, :, None],
        )
        self.arb = (
            np.where(rng.random((n, Tm, K)) < 0.5, 1.0, -1.0),
            sizes((n, Tm, K)),
            rng.uniform(0.0, 0.5e-4, (n, Tm, K)),
            np.arange(K)[None, None, :] < acnt[:, :, None],
        )
        # The crowd pays up to the book's far side: its limit is relative to
        # fair, past the book's premium.
        self.crowd = (
            np.repeat(crowd_side[:, :, None], K, 2),
            sizes((n, Tm, K)),
            np.abs(self.B / F - 1)[:, :, None] + lim_m * cbps[:, None, None],
            np.arange(K)[None, None, :] < ccnt[:, :, None],
        )
        self.noise_count = cnt
        self.crowd_count = ccnt
        # Signed noise+crowd volume per step, what any bot sees on the tape
        # (arb fills only show when someone is wrong; ignored here).
        self.tape = (self.noise[0] * self.noise[1] * self.noise[3]).sum(2) + (
            self.crowd[0] * self.crowd[1] * self.crowd[3]
        ).sum(2)


def run(world, policy, P=P):
    """One rung over every session. `policy(t, obs)` returns (bid, ask,
    size, take) arrays: quote prices (nan: none), quote size per side, and a
    signed taker order in base units."""
    w = world
    n, Tm = w.n, w.Tm
    inv = np.zeros(n)
    cash = np.zeros(n)
    peak = np.zeros(n)
    dd = np.zeros(n)
    comp_db = w.comp.copy()  # competitor's distance from the book, each side
    comp_da = w.comp.copy()
    hist_bd = [np.full(n, np.inf)] * 8
    hist_ad = [np.full(n, np.inf)] * 8
    bid = np.full(n, np.nan)
    ask = np.full(n, np.nan)
    size = np.zeros(n)
    state = {}
    for t in range(Tm):
        tape_now = np.zeros(n)
        live = w.alive[:, t]
        F = w.F[:, t]
        B = w.B[:, t]
        # Competitor: centred on the book, steps a tick inside the bot after
        # its reaction time, down to its floor.
        lagged_bd = np.choose(np.minimum(w.react, 7), [hist_bd[-1 - k] for k in range(8)])
        lagged_ad = np.choose(np.minimum(w.react, 7), [hist_ad[-1 - k] for k in range(8)])
        comp_db = np.maximum(w.floor, np.minimum(w.comp, lagged_bd - TICK / B))
        comp_da = np.maximum(w.floor, np.minimum(w.comp, lagged_ad - TICK / B))
        cb = B * (1 - comp_db)
        ca = B * (1 + comp_da)
        # The bot's quotes from last step's decision rest now.
        for kind in ("noise", "crowd", "arb"):
            side, sz, lim, on = getattr(w, kind)
            for k in range(w.K):
                m = on[:, t, k] & live
                if not m.any():
                    continue
                buy = side[:, t, k] > 0
                if kind == "arb":
                    limit = np.where(buy, F * (1 - lim[:, t, k]), F * (1 + lim[:, t, k]))
                else:
                    limit = np.where(buy, F * (1 + lim[:, t, k]), F * (1 - lim[:, t, k]))
                # Best quote the taker hits: the bot's if strictly better or
                # equal (time priority shared: equal splits half).
                bot_px = np.where(buy, ask, bid)
                comp_px = np.where(buy, ca, cb)
                ok_bot = np.isfinite(bot_px) & np.where(buy, bot_px <= limit, bot_px >= limit)
                better = np.where(buy, bot_px < comp_px, bot_px > comp_px)
                equal = np.isclose(bot_px, comp_px)
                share = np.where(better, 1.0, np.where(equal, 0.5, 0.0))
                room = np.where(buy, CAP + inv, CAP - inv)  # bot sells to a buyer
                q = np.minimum(np.minimum(sz[:, t, k] * share, size), np.maximum(room, 0))
                q = np.where(m & ok_bot, q, 0.0)
                best = np.where(buy, np.fmin(bot_px, comp_px), np.fmax(bot_px, comp_px))
                hits_any = m & np.where(buy, best <= limit, best >= limit)
                tape_now += np.where(hits_any, np.where(buy, 1.0, -1.0) * sz[:, t, k], 0.0)
                px = bot_px
                # Bot sells to a buyer.
                d = np.where(buy, -q, q)
                inv += d
                cash -= np.nan_to_num(d * px) + np.nan_to_num(q * px) * MAKER_FEE
        # Bot's turn: observe (this step's prices, the competitor's quotes,
        # the tape up to now) and decide the next step's quotes.
        obs = dict(
            O=w.O[:, t], Py=w.Py[:, t], Pyage=w.Pyage[:, t], B=B, cb=cb, ca=ca,
            tape=tape_now, inv=inv, F=F, lab=w.lab[:, t], hit=w.hit[:, t], t=t, p=w.p,
            prem=w.prem[:, t], world=w, state=state,
        )
        bid, ask, size, take = policy(obs)
        if P.get("no_take"):
            take = np.zeros(n)
        if P.get("oblig_bps") is not None and policy is not nop:
            wdt = comp_half(obs) + P["oblig_bps"] * 1e-4
            ref = obs["O"] if P.get("oblig_ref") == "oracle" else B
            ask = np.minimum(np.nan_to_num(ask, nan=np.inf), ref * (1 + wdt))
            bid = np.maximum(np.nan_to_num(bid, nan=-np.inf), ref * (1 - wdt))
        take = np.clip(take, -CAP - inv, CAP - inv) * live
        if np.any(take != 0):
            px = np.where(take > 0, ca, cb)
            cash -= take * px + np.abs(take) * px * TAKER_FEE
            inv += take
        hist_bd.append(np.where(np.isfinite(bid), (B - bid) / B, np.inf))
        hist_ad.append(np.where(np.isfinite(ask), (ask - B) / B, np.inf))
        hist_bd, hist_ad = hist_bd[-8:], hist_ad[-8:]
        val = np.where(live, cash + inv * w.O[:, t], np.nan)
        upd = live
        peak = np.where(upd, np.maximum(peak, val), peak)
        dd = np.where(upd, np.maximum(dd, peak - val), dd)
    Tend = w.T - 1
    Fend = w.F[np.arange(n), Tend]
    pnl = cash + inv * Fend
    return pnl, dd


def skewed(centre, half, inv, skew_bps=0.6):
    c = centre * (1 - inv * skew_bps * 1e-4)
    return c * (1 - half), c * (1 + half)


QSIZE = 1.5
SKEW = 0.25e-4


def comp_half(o):
    return (o["ca"] - o["cb"]) / (o["ca"] + o["cb"])


def nop(o):
    n = len(o["O"])
    return np.full(n, np.nan), np.full(n, np.nan), np.zeros(n), np.zeros(n)


def around(centre_fn):
    def f(o):
        h = np.maximum(comp_half(o) - TICK / o["B"], 0.5e-4)
        b, a = skewed(centre_fn(o), h, o["inv"])
        n = len(b)
        return b, a, np.full(n, QSIZE), np.zeros(n)

    return f


oracle_quoter = around(lambda o: o["O"])
book_quoter = around(lambda o: (o["ca"] + o["cb"]) / 2)
blend_quoter = around(lambda o: 0.5 * o["O"] + 0.5 * (o["ca"] + o["cb"]) / 2)


def inside(o, fair, margin):
    """A tick inside the competitor on each side, never past fair +- margin,
    skewed."""
    s = fair * o["inv"] * SKEW
    ask = np.maximum(np.maximum(o["ca"] - TICK, fair * (1 + margin)) - s, fair * (1 + 0.5e-4))
    bid = np.minimum(np.minimum(o["cb"] + TICK, fair * (1 - margin)) - s, fair * (1 - 0.5e-4))
    return bid, ask


def cautious(o):
    """Reads both, trusts neither: quotes outside both of them."""
    hi = np.maximum(o["O"], o["B"])
    lo = np.minimum(o["O"], o["B"])
    s = o["B"] * o["inv"] * 0.6e-4
    ask = np.maximum(o["ca"] - TICK, hi * (1 + 1e-4)) - s
    bid = np.minimum(o["cb"] + TICK, lo * (1 - 1e-4)) - s
    n = len(ask)
    return bid, ask, np.full(n, QSIZE), np.zeros(n)


def fair_told_v1(o):
    """What the informed bot believes fair is: the book when the oracle is
    wrong, the oracle when the book is, else the oracle and book blended."""
    lab = o["lab"]
    bm = (o["ca"] + o["cb"]) / 2
    return np.where(lab == 1, bm, np.where(lab == 2, o["O"], 0.5 * (o["O"] + bm)))


def fair_told(o):
    return o["F"]


def told(o, lean=True):
    fair = fair_told(o)
    bid, ask = inside(o, fair, 1e-4)
    n = len(bid)
    take = np.zeros(n)
    if lean:
        # Book wrong and far from fair: take toward reversion, past costs.
        gap = (o["B"] - fair) / fair
        cost = comp_half(o) + TAKER_FEE
        go = (o["lab"] > 0) & (np.abs(gap) > np.maximum(20e-4, 2.5 * cost))
        take = np.where(go, -np.sign(gap) * 2.0, 0.0)
        take = np.where(go, np.clip(take, -CAP - o["inv"], CAP - o["inv"]), 0.0)
    return bid, ask, np.full(n, QSIZE), take


def reference(o, lean=True):
    """Scores the sources from what is on chain, blends by precision, quotes
    a tick inside the competitor clipped to its fair, widens on doubt."""
    st = o["state"]
    n = len(o["O"])
    O, B, Py, age = o["O"], (o["ca"] + o["cb"]) / 2, o["Py"], o["Pyage"]
    if "prevO" not in st:
        st.update(prevO=O.copy(), still=np.zeros(n), imb=np.zeros(n), var=np.full(n, 0.3**2),
                  hist=[], Ohist=[])
    still = np.where(np.abs(O - st["prevO"]) < 0.5 * TICK, st["still"] + 1, 0)
    jump = np.abs(O - st["prevO"]) / O > 8e-4
    st["prevO"] = O.copy()
    st["still"] = still
    st["Ohist"].append(O.copy())
    st["hist"].append(B.copy())
    st["Ohist"], st["hist"] = st["Ohist"][-40:], st["hist"][-40:]
    # Delayed: the oracle matches the book of some earlier step better than
    # the book now.
    now_err = np.abs(O - B) / B
    best_lag_err = now_err.copy()
    for L in range(2, min(len(st["hist"]), 21)):
        best_lag_err = np.minimum(best_lag_err, np.abs(O - st["hist"][-L]) / B)
    delayed = (best_lag_err < 0.25 * now_err) & (now_err > 3e-4)
    tape = o["tape"]
    st["imb"] = 0.85 * st["imb"] + tape
    st["var"] = 0.995 * st["var"] + 0.005 * tape**2
    z = st["imb"] / np.sqrt(st["var"] * 6.0)
    pyth_bad = (age > 4) | (np.abs(Py - B) / B > 3e-4) & (np.abs(Py - O) / O < 1e-4) & False
    gap = np.abs(O - B) / B
    oracle_suspect = (still >= 6) | delayed | (age > 4) | (st.get("jumped", np.zeros(n)) > 0)
    st["jumped"] = np.where(jump & (np.abs(z) < 1.5), 40, np.maximum(st.get("jumped", np.zeros(n)) - 1, 0))
    oracle_suspect = oracle_suspect | (st["jumped"] > 0)
    crowd = (np.abs(z) > 2.0) & ~oracle_suspect
    w_o = np.where(oracle_suspect, 0.05, np.where(crowd, 0.95, 0.5))
    fair = w_o * O + (1 - w_o) * B
    doubt = (gap > 3e-4) & ~oracle_suspect & ~crowd
    margin = np.where(doubt, 0.5 * gap + 1e-4, 1e-4)
    bid, ask = inside(o, fair, margin)
    take = np.zeros(n)
    if lean:
        g = (B - O) / O
        go = crowd & (np.abs(g) > 15e-4)
        take = np.where(go, -np.sign(g) * 1.0, 0.0)
    return bid, ask, np.full(n, QSIZE), take


def oracle_taker(o):
    """Trusts the oracle always: fades the book to it when they part."""
    n = len(o["O"])
    B = (o["ca"] + o["cb"]) / 2
    g = (B - o["O"]) / o["O"]
    take = np.where(np.abs(g) > 15e-4, -np.sign(g) * 1.0, 0.0)
    nb, na, s, _ = oracle_quoter(o)
    return nb, na, s, take


def cautious3(o, k=1e-4):
    """Quotes outside the oracle, the book and Pyth: never decides."""
    hi = np.maximum(np.maximum(o["O"], o["B"]), o["Py"])
    lo = np.minimum(np.minimum(o["O"], o["B"]), o["Py"])
    s = o["B"] * o["inv"] * SKEW
    ask = np.maximum(o["ca"] - TICK, hi * (1 + k)) - s
    bid = np.minimum(o["cb"] + TICK, lo * (1 - k)) - s
    n = len(ask)
    return bid, ask, np.full(n, QSIZE), np.zeros(n)


def fade_to(target_fn, thr=15e-4):
    def f(o):
        B = (o["ca"] + o["cb"]) / 2
        tgt = target_fn(o)
        g = (B - tgt) / tgt
        return np.where(np.abs(g) > thr, -np.sign(g) * 1.0, 0.0)
    return f


book_fade = fade_to(lambda o: o["O"])
pyth_fade = fade_to(lambda o: o["Py"])


def flow_shy(o, base=cautious3):
    """Pulls the side the tape is hitting, whatever the cause."""
    st = o["state"].setdefault("shy", {})
    n = len(o["O"])
    if "imb" not in st:
        st.update(imb=np.zeros(n), var=np.full(n, 0.3**2))
    st["imb"] = 0.85 * st["imb"] + o["tape"]
    st["var"] = 0.995 * st["var"] + 0.005 * o["tape"] ** 2
    z = st["imb"] / np.sqrt(st["var"] * 6.0)
    bid, ask, size, take = base(o)
    ask = np.where(z > 2.0, np.nan, ask)  # buyers hitting: pull the ask
    bid = np.where(z < -2.0, np.nan, bid)
    return bid, ask, size, take


def combo(quote, *fades):
    def f(o):
        b, a, s, t = quote(o)
        for fd in fades:
            t = t + fd(o)
        return b, a, s, np.clip(t, -1.0, 1.0)
    return f


def reference2(o, lean=False):
    """Reads Pyth's age, the oracle's behaviour, the book against the oracle
    and the tape; blends a fair value; widens on doubt."""
    st = o["state"].setdefault("ref2", {})
    n = len(o["O"])
    O, B, Py, age = o["O"], (o["ca"] + o["cb"]) / 2, o["Py"], o["Pyage"]
    if "prevO" not in st:
        st.update(prevO=O.copy(), still=np.zeros(n), imb=np.zeros(n), var=np.full(n, 0.3**2),
                  jumped=np.zeros(n), lastgood=O.copy())
    still = np.where(np.abs(O - st["prevO"]) < 0.5 * TICK, st["still"] + 1, 0)
    jump = np.abs(O - st["prevO"]) / O > 8e-4
    st["prevO"] = O.copy()
    st["still"] = still
    st["imb"] = 0.85 * st["imb"] + o["tape"]
    st["var"] = 0.995 * st["var"] + 0.005 * o["tape"] ** 2
    z = st["imb"] / np.sqrt(st["var"] * 6.0)
    pyth_fresh = age <= 2
    gap_bo = (B - O) / O
    gap_po = (Py - O) / O
    crowd = (np.abs(gap_bo) > 6e-4) & (np.sign(z) == np.sign(gap_bo)) & (np.abs(z) > 1.5)
    st["jumped"] = np.where(jump & ~crowd, 40, np.maximum(st["jumped"] - 1, 0))
    oracle_bad = ((still >= 6) | (st["jumped"] > 0) | ((np.abs(gap_po) > 5e-4) & pyth_fresh)
                  | ((np.abs(z) > 2.5) & ~crowd))
    # Oracle suspect: fair from fresh Pyth if it disagrees, else lean the
    # oracle by the tape's direction.
    fair_o = np.where(pyth_fresh & (np.abs(gap_po) > 5e-4), Py, O * (1 + np.clip(z, -3, 3) * 5e-4))
    fair = np.where(crowd, O, np.where(oracle_bad, fair_o, O))
    margin = np.where(oracle_bad, 3e-4, 1e-4)
    bid, ask = inside(o, fair, margin)
    take = np.zeros(n)
    if lean:
        g = (B - fair) / fair
        take = np.where(np.abs(g) > 20e-4, -np.sign(g), 0.0)
    return bid, ask, np.full(n, QSIZE), take


RUNGS = {
    "nop": nop,
    "oracle-quoter": oracle_quoter,
    "book-quoter": book_quoter,
    "midpoint-blend": blend_quoter,
    "cautious": cautious,
    "oracle-taker": oracle_taker,
    "reference": reference2,
    "reference-lean": lambda o: reference2(o, lean=True),
    "told": told,
    "told-nolean": lambda o: told(o, lean=False),
    "cautious-fader": lambda o: (lambda q, t: (q[0], q[1], q[2], t[3]))(cautious(o), oracle_taker(o)),
    "cautious3": cautious3,
    "book-fader": combo(oracle_quoter, book_fade),
    "pyth-fader": combo(oracle_quoter, pyth_fade),
    "cautious3-fade-both": combo(cautious3, book_fade, pyth_fade),
    "flow-shy": flow_shy,
    "flow-shy-fade-both": combo(flow_shy, book_fade, pyth_fade),
}
NON_READING = ["oracle-quoter", "book-quoter"]
# The card's order: nop < each careful-unaware rung < midpoint-blend <
# reference < told.
ORDER = [("nop", "oracle-quoter"), ("nop", "book-quoter"), ("oracle-quoter", "midpoint-blend"),
         ("book-quoter", "midpoint-blend"), ("midpoint-blend", "reference"), ("reference", "told")]
# The gate's weaker form: nop and the naive rungs below the reference, and
# the reference below told.
ORDER_MIN = [("nop", "reference"), ("oracle-quoter", "reference"), ("book-quoter", "reference"),
             ("midpoint-blend", "reference"), ("reference", "told")]


NODEC = ["cautious", "cautious-fader", "oracle-taker", "cautious3", "book-fader", "pyth-fader",
         "cautious3-fade-both", "flow-shy", "flow-shy-fade-both", "midpoint-blend"]


def weighted(x, k=1.5):
    return np.where(x >= 0, x, k * x).sum()


def main():
    seed = int(sys.argv[1]) if len(sys.argv) > 1 else 1
    n = int(sys.argv[2]) if len(sys.argv) > 2 else 400
    if len(sys.argv) > 3:
        P.update({k: tuple(v) if isinstance(v, list) else v for k, v in json.loads(sys.argv[3]).items()})
    rng = np.random.default_rng(seed)
    w = World(rng, n)
    res = {}
    for name, pol in RUNGS.items():
        pnl, dd = run(w, pol)
        res[name] = (pnl, dd)
        print(f"{name:16s} mean {pnl.mean():8.2f} se {pnl.std() / np.sqrt(n):6.2f} "
              f"sharpe {pnl.mean() / (pnl.std() + 1e-9):5.2f} dd>L {np.mean(dd > DD):.3f} "
              f"skill {pnl.sum() / res.get('reference', (pnl,))[0].sum() if 'reference' in res else float('nan'):5.2f}")
    ref = res["reference"][0].sum()
    print("skill vs reference:", {k: round(v[0].sum() / ref, 2) for k, v in res.items()})
    best_nr = max(max(res[k][0].sum(), 0) for k in NON_READING)
    tot = res["told"][0].sum()
    print(f"GATE0 told/best-non-reading = {tot / max(best_nr, 1e-9):.2f} (told {tot:.0f}, best {best_nr:.0f})")
    # Bootstrap on 100-session sets.
    B = 1000
    ok_order = 0
    ok_ratio = 0
    ok_min = 0
    ok_caut = 0
    for _ in range(B):
        idx = rng.integers(0, n, 100)
        s = {k: v[0][idx].sum() for k, v in res.items()}
        if all(s[a] < s[b] for a, b in ORDER):
            ok_order += 1
        if s["told"] >= 2 * max(max(s[k], 0) for k in NON_READING):
            ok_ratio += 1
        if all(s[a] < s[b] for a, b in ORDER_MIN):
            ok_min += 1
        if s["told"] >= 2 * max(max(s[k] for k in NODEC), 0):
            ok_caut += 1
    bestnd = max(NODEC, key=lambda k: res[k][0].sum())
    print(f"GATE0-REDESIGN told / best no-decision ({bestnd}) = {tot / max(res[bestnd][0].sum(), 1e-9):.2f}")
    for k in NODEC:
        print(f"  told/{k} = {tot / max(res[k][0].sum(), 1e-9):.2f}   told-nolean/{k} = {res['told-nolean'][0].sum() / max(res[k][0].sum(), 1e-9):.2f}")
    print(f"bootstrap(100): card order holds {ok_order / B:.3f}, minimal order holds {ok_min / B:.3f}, "
          f"gate0 ratio holds {ok_ratio / B:.3f}, told >= 2x best no-decision holds {ok_caut / B:.3f}")
    er, ei = weighted(res["reference"][0]), weighted(res["told"][0])
    print(f"v2 band: reference/informed weighted = {er / ei:.2f} (0.2-0.7), reference sharpe "
          f"{res['reference'][0].mean() / res['reference'][0].std():.2f} (>= 0.25)")
    for k in res:
        e = weighted(res[k][0])
        raw = 0.5 * e / er if e <= er else 0.5 + 0.5 * (e - er) / (ei - er)
        print(f"  v2 raw {k:16s} {raw:6.3f}")


if __name__ == "__main__":
    main()
