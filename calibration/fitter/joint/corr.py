"""Fit the cross-asset correlation file (calibration/correlation/<set>.<date>.<hash>.toml).

The inputs are the real 1 m regular-hours mark returns of each asset over one
common window, read from the archived Phoenix candles. The fit is the method of
simulated moments: `draw_joint` (crates/grader/src/market/model.rs) divides each
asset's innovation by its own Student-t factor, gives each asset its own volatility
level and GARCH state, and smooths the mark, so the realized correlation of the
generated 1 m returns is a fraction of the matrix entry. `simulate_pair` mirrors
that draw for two assets from their calibration files, and the matrix entry is
the one whose simulated realized correlation equals the real one. The matrix is
then projected to the nearest positive-definite correlation matrix.

Standard library only; every random stream is seeded, so the same archives give
the same bytes.
"""

from __future__ import annotations

import hashlib
import json
import math
import random
import tomllib
from collections import defaultdict
from datetime import datetime, timezone
from pathlib import Path
from statistics import NormalDist, median
from zoneinfo import ZoneInfo

HERE = Path(__file__).resolve().parent
FITTER = HERE.parent
MARKET = FITTER.parent / "market"
NY = ZoneInfo("America/New_York")
OPEN_MIN = 9 * 60 + 31  # the first regular-hours return is the 09:31 bar's
SESSION_BARS = 390
SUB = 12  # 5 s steps in a 60 s block: the mark's update interval
MSM_SEED = 7


def r4(x: float) -> float:
    return round(x, 4)


def code_hash() -> str:
    """SHA-256 over joint/*.py, 12 hex."""
    h = hashlib.sha256()
    for p in sorted(HERE.glob("*.py")):
        h.update(p.read_bytes())
    return h.hexdigest()[:12]


# --- the real returns ------------------------------------------------------


def load_marks(raws: list[Path], symbol: str) -> tuple[dict[int, float], list[dict]]:
    """Mark closes by bar time (ms) from whichever archive holds the symbol's
    1 m candles, and the manifest rows of the files read."""
    for raw in raws:
        manifest = json.loads((raw / "manifest.json").read_text())
        names = sorted(n for n in manifest if n.startswith(f"phoenix/candles_{symbol}_1m_"))
        if not names:
            continue
        marks: dict[int, float] = {}
        rows = []
        for n in names:
            for c in json.loads((raw / n).read_text()):
                if c["markClose"]:
                    marks[c["time"]] = c["markClose"]
            rows.append(dict(name=n, **manifest[n]))
        return marks, rows
    raise SystemExit(f"correlation: no archive holds {symbol}'s 1 m candles")


def regular_returns(marks: dict[int, float], first: str, last: str, holidays: set[str]) -> dict[tuple[str, int], float]:
    """1 m log returns of regular hours (09:31 to 16:00 New York, weekdays that
    are not holidays) by (date, minute index), only where both bars exist: no
    return spans a missing bar or is forward-filled."""
    out = {}
    for t, c in marks.items():
        p = marks.get(t - 60_000)
        if p is None:
            continue
        dt = datetime.fromtimestamp(t / 1000, NY)
        d = dt.date().isoformat()
        idx = dt.hour * 60 + dt.minute - OPEN_MIN
        if dt.weekday() < 5 and 0 <= idx < SESSION_BARS and first <= d < last and d not in holidays:
            out[(d, idx)] = math.log(c / p)
    return out


def pearson(x: list[float], y: list[float]) -> float:
    n = len(x)
    mx, my = sum(x) / n, sum(y) / n
    sxy = sum((a - mx) * (b - my) for a, b in zip(x, y))
    sxx = sum((a - mx) ** 2 for a in x)
    syy = sum((b - my) ** 2 for b in y)
    return sxy / math.sqrt(sxx * syy) if sxx > 0 and syy > 0 else float("nan")


def windows(ra: dict, rb: dict, bars: int) -> list[tuple[list[float], list[float]]]:
    """Clock-aligned windows of `bars` consecutive 1 m returns present in both."""
    out = []
    days = sorted({d for d, _ in ra} & {d for d, _ in rb})
    for d in days:
        for k in range(SESSION_BARS // bars):
            idx = range(k * bars, (k + 1) * bars)
            if all((d, i) in ra and (d, i) in rb for i in idx):
                out.append(([ra[(d, i)] for i in idx], [rb[(d, i)] for i in idx]))
    return out


def pair_stats(ra: dict, rb: dict, bars: int) -> dict:
    keys = sorted(set(ra) & set(rb))
    pooled = pearson([ra[k] for k in keys], [rb[k] for k in keys])
    # 5 m: sums of 5 consecutive 1 m returns, clock-aligned
    sums = [(sum(a), sum(b)) for a, b in windows(ra, rb, 5)]
    five = pearson([a for a, _ in sums], [b for _, b in sums])
    wc = [c for c in (pearson(a, b) for a, b in windows(ra, rb, bars)) if c == c]
    return dict(pooled=pooled, five=five, n=len(keys), median=median(wc), windows=wc)


def bootstrap_median_sd(values: list[float], draws: int = 400, seed: int = MSM_SEED) -> float:
    """The sd of the window-median correlation over resamples of the real
    windows, as many as there are."""
    rng = random.Random(seed)
    n = len(values)
    meds = [median(values[rng.randrange(n)] for _ in range(n)) for _ in range(draws)]
    m = sum(meds) / draws
    return math.sqrt(sum((x - m) ** 2 for x in meds) / (draws - 1))


def jump_flags(r: dict) -> tuple[set, set]:
    """(eligible, flagged) minutes: eligible have a full trailing hour of
    returns; flagged beyond 5 robust sigmas of it (the fitter's jump rule)."""
    eligible, flagged = set(), set()
    for (d, i), v in r.items():
        if i < 60 or not all((d, j) in r for j in range(i - 60, i)):
            continue
        eligible.add((d, i))
        s = 1.4826 * median(abs(r[(d, j)]) for j in range(i - 60, i))
        if s > 0 and abs(v) > 5 * s:
            flagged.add((d, i))
    return eligible, flagged


def jump_pair(fa, fb, ra: dict, rb: dict) -> dict:
    common = fa[0] & fb[0]
    ja, jb = fa[1] & common, fb[1] & common
    both = ja & jb
    expect = len(ja) * len(jb) / len(common)
    same = sum(1 for k in both if (ra[k] > 0) == (rb[k] > 0))
    return dict(minutes=len(common), jumps_a=len(ja), jumps_b=len(jb), both=len(both), expected_if_independent=r4(expect),
                same_sign=same)


# --- the model's draw, for the simulated moments ----------------------------


def normal_cdf(z: float) -> float:
    """Abramowitz and Stegun's 7.1.26, as model.rs has it."""
    x = abs(z) / math.sqrt(2)
    t = 1.0 / (1.0 + 0.3275911 * x)
    poly = t * (0.254829592 + t * (-0.284496736 + t * (1.421413741 + t * (-1.453152027 + t * 1.061405429))))
    erf = 1.0 - poly * math.exp(-x * x)
    return 0.5 * (1.0 + erf) if z >= 0 else 0.5 * (1.0 - erf)


def median_level(rg: dict) -> float:
    parts = [(rg["share"][i], math.log(rg["vol_mult"][i]), rg["log_sd_within"][i]) for i in range(len(rg["names"]))]

    def cdf(x):
        return sum(w * ((1.0 if x >= mu else 0.0) if sd <= 0 else normal_cdf((x - mu) / sd)) for w, mu, sd in parts)

    lo, hi = -20.0, 20.0
    for _ in range(200):
        mid = 0.5 * (lo + hi)
        if cdf(mid) < 0.5:
            lo = mid
        else:
            hi = mid
    return math.exp(0.5 * (lo + hi))


class Asset:
    """One asset's walker (model.rs `Walker`) for a 'regular' session start, as
    the default spec draws it: no background jumps (they are opt-in) and no gap
    inside a 30-minute session."""

    def __init__(self, cal: dict, rng: random.Random):
        fv = cal["fair_value"]
        self.rng = rng
        self.rg = fv["regimes"]
        self.norm = median_level(self.rg)
        self.block_sd = fv["per_second_sd"] * math.sqrt(fv["block_seconds"])
        self.df = fv["tails"]["df"]
        self.alpha, self.beta = fv["clustering"]["alpha"], fv["clustering"]["beta"]
        mark = fv.get("mark")
        self.lam = 1.0 - 0.5 ** (mark["update_seconds"] / mark["half_life_seconds"]) if mark else None
        self.leave = [(1.0 - self.rg["transition_per_hour"][i][i]) * fv["block_seconds"] / 3600.0 for i in range(len(self.rg["names"]))]

    def start(self) -> None:
        rng, rg = self.rng, self.rg
        self.lv = [rg["vol_mult"][i] * math.exp(rg["log_sd_within"][i] * rng.gauss(0, 1)) / self.norm for i in range(len(rg["names"]))]
        u, acc, self.r = rng.random(), 0.0, len(rg["share"]) - 1
        for i, w in enumerate(rg["share"]):
            acc += w
            if u < acc:
                self.r = i
                break
        self.h, self.p, self.m = 1.0, 0.0, 0.0

    def block(self, z: float) -> float:
        """One 60 s block with the correlated normal `z`; returns the mark's move."""
        rng = self.rng
        sd = self.block_sd * self.lv[self.r]
        scale = math.sqrt((self.df - 2.0) / (2.0 * rng.gammavariate(self.df / 2.0, 1.0)))
        sq = math.sqrt(self.h)
        diffusive = sd * sq * z
        excess = diffusive * (scale - 1.0)
        at = rng.randrange(SUB)
        g = [rng.gauss(0, 1) for _ in range(SUB)]
        gm = sum(g) / SUB
        pc = sd * sq / math.sqrt(SUB)
        start = self.m
        p, m, lam = self.p, self.m, self.lam
        for j in range(SUB):
            step = diffusive / SUB + pc * (g[j] - gm) + (excess if j == at else 0.0)
            p += step
            m = p if lam is None else m + lam * (p - m)
        self.p, self.m = p, m
        e = sq * z * scale
        self.h = (1.0 - self.alpha - self.beta) + self.alpha * e * e + self.beta * self.h
        if len(self.leave) > 1:
            u = rng.random()
            if u >= 1.0 - self.leave[self.r]:
                self.r = 1 - self.r
        return m - start


def simulate_pair(ca: dict, cb: dict, rho: float, sessions: int, bars: int, seed: int = MSM_SEED) -> list[tuple[list[float], list[float]]]:
    """`sessions` joint draws of `bars` blocks for two assets, their innovations
    correlated by `rho`. The random streams do not depend on `rho`, so the
    simulated correlation is a smooth function of it."""
    a, b = Asset(ca, random.Random(seed * 3 + 1)), Asset(cb, random.Random(seed * 3 + 2))
    gz = random.Random(seed * 3 + 3)
    c = math.sqrt(max(0.0, 1.0 - rho * rho))
    out = []
    for _ in range(sessions):
        a.start()
        b.start()
        xa, xb = [], []
        for _ in range(bars):
            ga, gb = gz.gauss(0, 1), gz.gauss(0, 1)
            xa.append(a.block(ga))
            xb.append(b.block(rho * ga + c * gb))
        out.append((xa, xb))
    return out


def sim_stats(draws: list[tuple[list[float], list[float]]]) -> dict:
    pooled = pearson([v for x, _ in draws for v in x], [v for _, y in draws for v in y])
    wc = [c for c in (pearson(x, y) for x, y in draws) if c == c]
    five = [(sum(x[i:i + 5]), sum(y[i:i + 5])) for x, y in draws for i in range(0, len(x) - len(x) % 5, 5)]
    return dict(pooled=pooled, median=median(wc), five=pearson([a for a, _ in five], [b for _, b in five]))


def solve_entry(ca: dict, cb: dict, target: float, sessions: int, bars: int) -> tuple[float, dict]:
    """The matrix entry whose simulated median 30-minute-window 1 m correlation
    is `target` (secant steps on a smooth, increasing function)."""
    def f(rho):
        return sim_stats(simulate_pair(ca, cb, rho, sessions, bars))["median"]

    x0, y0 = 0.5, f(0.5)
    x1 = min(0.97, max(0.02, target / y0 * x0))
    y1 = f(x1)
    for _ in range(4):
        if abs(y1 - target) < 5e-4 or y1 == y0:
            break
        x2 = x1 + (target - y1) * (x1 - x0) / (y1 - y0)
        x2 = min(0.99, max(0.0, x2))
        x0, y0, x1, y1 = x1, y1, x2, f(x2)
    return x1, sim_stats(simulate_pair(ca, cb, x1, sessions, bars))


# --- the matrix --------------------------------------------------------------


def jacobi_eigh(a: list[list[float]]) -> tuple[list[float], list[list[float]]]:
    """Eigenvalues and vectors (as columns) of a small symmetric matrix."""
    n = len(a)
    a = [row[:] for row in a]
    v = [[1.0 if i == j else 0.0 for j in range(n)] for i in range(n)]
    for _ in range(100):
        off = sum(a[i][j] ** 2 for i in range(n) for j in range(i + 1, n))
        if off < 1e-24:
            break
        for p in range(n):
            for q in range(p + 1, n):
                if abs(a[p][q]) < 1e-300:
                    continue
                theta = (a[q][q] - a[p][p]) / (2 * a[p][q])
                t = (1.0 if theta >= 0 else -1.0) / (abs(theta) + math.sqrt(theta * theta + 1))
                c = 1 / math.sqrt(t * t + 1)
                s = t * c
                for k in range(n):
                    akp, akq = a[k][p], a[k][q]
                    a[k][p], a[k][q] = c * akp - s * akq, s * akp + c * akq
                for k in range(n):
                    apk, aqk = a[p][k], a[q][k]
                    a[p][k], a[q][k] = c * apk - s * aqk, s * apk + c * aqk
                for k in range(n):
                    vkp, vkq = v[k][p], v[k][q]
                    v[k][p], v[k][q] = c * vkp - s * vkq, s * vkp + c * vkq
    return [a[i][i] for i in range(n)], v


def min_eig(m: list[list[float]]) -> float:
    return min(jacobi_eigh(m)[0])


def nearest_correlation(m: list[list[float]], floor: float = 0.02) -> list[list[float]]:
    """Higham's alternating projections onto the matrices with eigenvalues at
    least `floor` and the unit diagonal. A matrix already above the floor is
    returned as it is."""
    n = len(m)
    if min_eig(m) >= floor:
        return m
    y = [row[:] for row in m]
    ds = [[0.0] * n for _ in range(n)]
    for _ in range(500):
        r = [[y[i][j] - ds[i][j] for j in range(n)] for i in range(n)]
        w, v = jacobi_eigh(r)
        x = [[sum(v[i][k] * max(w[k], floor) * v[j][k] for k in range(n)) for j in range(n)] for i in range(n)]
        ds = [[x[i][j] - r[i][j] for j in range(n)] for i in range(n)]
        new = [[1.0 if i == j else x[i][j] for j in range(n)] for i in range(n)]
        delta = max(abs(new[i][j] - y[i][j]) for i in range(n) for j in range(n))
        y = new
        if delta < 1e-12:
            break
    return y


def cholesky_ok(m: list[list[float]]) -> bool:
    n = len(m)
    low = [[0.0] * n for _ in range(n)]
    for i in range(n):
        for j in range(i + 1):
            s = sum(low[i][k] * low[j][k] for k in range(j))
            if i == j:
                d = m[i][i] - s
                if d <= 0:
                    return False
                low[i][j] = math.sqrt(d)
            else:
                low[i][j] = (m[i][j] - s) / low[j][j]
    return True


def symmetric(m: list[list[float]]) -> list[list[float]]:
    n = len(m)
    return [[1.0 if i == j else r4(m[min(i, j)][max(i, j)]) for j in range(n)] for i in range(n)]


# --- the fit -----------------------------------------------------------------


def fit(cfg: dict, raws: list[Path], fitted_at: str, fit_date: str, log=print) -> tuple[dict, list[dict]]:
    from calib import export_v1 as E

    assets = cfg["assets"]
    n = len(assets)
    holidays = set(E.HOLIDAYS)
    cals, cal_files = {}, {}
    for a, cid in zip(assets, cfg["calibrations"]):
        path = MARKET / f"{cid}.toml"
        cals[a] = tomllib.loads(path.read_text())
        cal_files[a] = hashlib.sha256(path.read_bytes()).hexdigest()
        if cals[a]["meta"]["calibration_id"] != cid or cals[a]["meta"]["asset"] != a:
            raise SystemExit(f"correlation: {path.name} is not {a}'s {cid}")

    inputs, rets = [], {}
    for a in assets:
        marks, rows = load_marks(raws, cals[a]["meta"]["symbol"])
        first, last = cfg["window_from"], cfg["window_to"]
        # only the files that hold the window's days are inputs of this fit
        lo = datetime.fromisoformat(first).replace(tzinfo=timezone.utc).timestamp() - 86400
        hi = datetime.fromisoformat(last).replace(tzinfo=timezone.utc).timestamp() + 86400
        for r in rows:
            start, end = (int(x) for x in r["name"].rsplit(".", 1)[0].split("_")[-2:])
            if end >= lo and start <= hi:
                inputs.append(r)
        rets[a] = regular_returns(marks, first, last, holidays)
        log(f"{a}: {len(rets[a])} regular-hours 1 m returns", flush=True)
    inputs.sort(key=lambda r: r["name"])

    bars = cfg["window_minutes"]
    stats, pairs = {}, [(i, j) for i in range(n) for j in range(i + 1, n)]
    for i, j in pairs:
        stats[(i, j)] = pair_stats(rets[assets[i]], rets[assets[j]], bars)
    sample = [[1.0 if i == j else stats[(min(i, j), max(i, j))]["median"] for j in range(n)] for i in range(n)]
    off = [sample[i][j] for i, j in pairs]
    avg = sum(off) / len(off)
    s = cfg["shrinkage"]
    target = [[1.0 if i == j else (1 - s) * sample[i][j] + s * avg for j in range(n)] for i in range(n)]
    log("sample (window median 1 m):", [[r4(x) for x in row] for row in sample], flush=True)

    entries, sims = {}, {}
    for i, j in pairs:
        rho, sim = solve_entry(cals[assets[i]], cals[assets[j]], target[i][j], cfg["msm_sessions"], bars)
        entries[(i, j)], sims[(i, j)] = rho, sim
        log(f"{assets[i]}-{assets[j]}: target {target[i][j]:.4f} -> entry {rho:.4f} (simulated median {sim['median']:.4f}, pooled {sim['pooled']:.4f})",
            flush=True)
    raw_matrix = [[1.0 if i == j else entries[(min(i, j), max(i, j))] for j in range(n)] for i in range(n)]
    matrix = symmetric(nearest_correlation(raw_matrix))
    projected = matrix != symmetric(raw_matrix)
    if not cholesky_ok(matrix):
        raise SystemExit("correlation: the matrix is not positive definite")
    if projected:
        for i, j in pairs:
            sims[(i, j)] = sim_stats(simulate_pair(cals[assets[i]], cals[assets[j]], matrix[i][j], cfg["msm_sessions"], bars))

    # what the realism check judges: each pair's median window correlation
    nsig = len(pairs)
    sigmas = NormalDist().inv_cdf(1 - (1 - 0.99 ** (1.0 / nsig)) / 2)
    realism_pairs = []
    for i, j in pairs:
        st = stats[(i, j)]
        realism_pairs.append(dict(a=assets[i], b=assets[j], corr_median=r4(st["median"]), corr_spread_sd=r4(bootstrap_median_sd(st["windows"])),
                                  window_minutes=bars, n_windows=len(st["windows"])))
    flags = {a: jump_flags(rets[a]) for a in assets}
    cooc = [dict(a=assets[i], b=assets[j], **jump_pair(flags[assets[i]], flags[assets[j]], rets[assets[i]], rets[assets[j]])) for i, j in pairs]

    digest = hashlib.sha256("".join(r["sha256"] for r in inputs).encode()).hexdigest()[:16]
    code = code_hash()
    params = json.dumps({k: cfg[k] for k in ("window_from", "window_to", "window_minutes", "shrinkage", "msm_sessions", "assets", "calibrations")}, sort_keys=True)
    version = hashlib.sha256((digest + code + "".join(cal_files[a] for a in assets) + params).encode()).hexdigest()[:8]
    cid = f"{cfg['set']}.{fit_date}.{version}"
    common_days = len(set.intersection(*({d for d, _ in rets[a]} for a in assets)))
    common_minutes = len(set.intersection(*(set(rets[a]) for a in assets)))
    first_day = min(d for a in assets for d, _ in rets[a])
    last_day = max(d for a in assets for d, _ in rets[a])
    matrix_table = [[r4(x) for x in row] for row in matrix]
    target_table = [[1.0 if i == j else r4(target[i][j]) for j in range(n)] for i in range(n)]

    def table(f):
        return [[1.0 if i == j else r4(f(min(i, j), max(i, j))) for j in range(n)] for i in range(n)]

    sample_table = table(lambda i, j: sample[i][j])
    pooled_table = table(lambda i, j: stats[(i, j)]["pooled"])
    five_table = table(lambda i, j: stats[(i, j)]["five"])
    gen_table = table(lambda i, j: sims[(i, j)]["median"])
    gen_pooled_table = table(lambda i, j: sims[(i, j)]["pooled"])
    cal = dict(
        meta=dict(
            schema_version="1.0", correlation_id=cid, assets=assets, calibrations=cfg["calibrations"], fitted_at=fitted_at,
            fitter="calibration/fitter (joint/corr.py)", fitter_code_sha256_12=code, inputs_file=f"{cid}.inputs.json", inputs_digest=digest,
            inputs_count=len(inputs),
            sources=["Phoenix perp API /candles (1 m mark closes) of " + ", ".join(cals[a]["meta"]["symbol"] for a in assets),
                     "the pinned calibration files (the draw the matrix is fitted through)"],
            limits=[
                f"Fitted on one common window, {cfg['window_from']} to {cfg['window_to']} (New York dates, end exclusive): {common_days} sessions, "
                f"{common_minutes} minutes in which all {n} assets have a regular-hours return. The window is the shortest history's (SPY and QQQ's first full "
                "week) to the end of the SPCX pull; correlations of equities move with the market's regime, and this window is one period.",
                "Phoenix's mark is the oracle plus the perp's premium, not the underlying's tape. Its 1 m returns carry quote noise the underlying's do not, "
                "so the real correlations are below the underlyings' (the Epps effect and stale marks): the pooled 5 m correlations in real_pooled_5m are "
                "higher for every pair. The model targets 1 m, the scale sessions trade at.",
                f"The matrix entry of each pair is the one at which the simulated draw (method of simulated moments: {cfg['msm_sessions']} sessions of {bars} "
                f"blocks, seeded) reproduces the real median {bars}-minute-window correlation of 1 m returns, the statistic the joint check judges. The draw's "
                "volatility levels, GARCH states and Student-t factors are independent across assets, so the entries are above the correlations they give, and the "
                "draw cannot reproduce the real pooled 1 m correlation (real_pooled_1m against simulated_pooled_1m): pooled returns weight the heavy-tailed "
                "windows, where real assets move together and the draw's do not. The draw has no background jumps by default (they are opt-in), and "
                "when on they are independent across assets: the data's jump co-occurrence is recorded under [jump_cooccurrence] and not modelled, so "
                "the entries absorb it.",
                f"The targets are the real medians shrunk {s} of the way toward their average off-diagonal ({avg:.4f}).",
                ("The matrix was projected to the nearest positive-definite correlation matrix (eigenvalue floor 0.02)." if projected
                 else "The fitted matrix was positive definite as it came; no projection was applied."),
            ],
            windows=dict(fit=[first_day, last_day]),
        ),
        correlation=dict(
            block_seconds=60, matrix=matrix_table, realized_target=target_table, realized_sample=sample_table, simulated_realized=gen_table,
            real_pooled_1m=pooled_table, real_pooled_5m=five_table, simulated_pooled_1m=gen_pooled_table, shrinkage=s,
            source=(f"{len(inputs)} Phoenix 1 m candle files; the real median {bars}-minute-window correlation of regular-hours 1 m mark returns over "
                    f"{common_days} common sessions, shrunk {s} toward the average; entries by simulated moments through draw_joint's model "
                    f"({cfg['msm_sessions']} sessions)"),
        ),
        realism=dict(
            source=(f"real {bars}-minute windows of consecutive regular-hours 1 m returns present in both assets, clock-aligned, "
                    f"{cfg['window_from']} to {cfg['window_to']}"),
            statistic="median over windows of the Pearson correlation of the windows' 1 m returns",
            sigmas=r4(sigmas), sigmas_source=f"Sidak: each of the {nsig} pairs at 1% family-wise false fails",
            draws=400, pairs=realism_pairs,
        ),
        jump_cooccurrence=dict(
            rule="1 m return beyond 5 robust sigmas of the trailing hour, as in the calibration files; reported, not modelled and not gated",
            pairs=cooc,
        ),
    )
    return cal, inputs


def write(cal: dict, inputs: list[dict], out: Path) -> Path:
    from calib import toml_out

    cid = cal["meta"]["correlation_id"]
    out.mkdir(parents=True, exist_ok=True)
    path = out / f"{cid}.toml"
    header = (f"# Correlation {cid} (schema 1.0). Written by calibration/fitter (joint/corr.py)\n"
              f"# from sets/<set>.toml; never edited by hand. A refit is a new file with a new id.\n"
              f"# Every input (url, sha256, bytes, fetch time) is in {cid}.inputs.json.\n\n")
    path.write_text(header + toml_out.dumps(cal) + "\n")
    tomllib.loads(path.read_text())
    (out / f"{cid}.inputs.json").write_text(json.dumps(inputs, indent=1) + "\n")
    return path
