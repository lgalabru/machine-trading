"""Two-pool CLMM cycle math with a constant-liquidity (single range) approximation.
Units: sqrt price s = sqrt(USDC_raw / SOL_raw); L in raw liquidity units; fee as a fraction.
Token A = SOL (9 dp), token B = USDC (6 dp)."""
import math

def sell_sol(s, L, fee, dx_raw):
    """Sell dx SOL into a pool. Returns (usdc_out_raw, new_s)."""
    dx = dx_raw * (1 - fee)
    s2 = 1.0 / (1.0 / s + dx / L)
    return L * (s - s2), s2

def buy_sol(s, L, fee, dy_raw):
    """Spend dy USDC in a pool. Returns (sol_out_raw, new_s)."""
    dy = dy_raw * (1 - fee)
    s2 = s + dy / L
    return L * (1.0 / s - 1.0 / s2), s2

def price(s):  # USDC per SOL
    return s * s * 1e3

def gap_bps(s_a, s_b):
    pa, pb = price(s_a), price(s_b)
    return (max(pa, pb) / min(pa, pb) - 1) * 1e4

def cycle(hi, lo, x_sol_raw):
    """Sell x SOL in the high-price pool, buy SOL back in the low-price pool. hi/lo: (s, L, fee)."""
    y, s_hi2 = sell_sol(hi[0], hi[1], hi[2], x_sol_raw)
    x2, s_lo2 = buy_sol(lo[0], lo[1], lo[2], y)
    return x2 - x_sol_raw, s_hi2, s_lo2, y

def optimal(pool1, pool2, tip_sol=0.0, tx_fee_sol=0.0):
    """pool = (s, L, fee). Returns dict with direction, optimal input (SOL), gross/net profit (SOL), post prices."""
    hi, lo, d = (pool1, pool2, "sell_in_1") if price(pool1[0]) > price(pool2[0]) else (pool2, pool1, "sell_in_2")
    g = gap_bps(pool1[0], pool2[0])
    if (1 - hi[2]) * (1 - lo[2]) * price(hi[0]) / price(lo[0]) <= 1:
        return dict(direction=d, gap_bps=g, x_sol=0.0, profit_sol=0.0, net_sol=-tx_fee_sol, profitable=False)
    # closed form for two constant-product (virtual reserve) pools
    # hi: SOL reserve Xh = L/s, USDC reserve Yh = L*s ; lo: Xl, Yl. route SOL->hi->USDC->lo->SOL
    Xh, Yh = hi[1] / hi[0], hi[1] * hi[0]
    Xl, Yl = lo[1] / lo[0], lo[1] * lo[0]
    gh, gl = 1 - hi[2], 1 - lo[2]
    # composite CPMM: x_out = a*x/(b + c*x)
    a = gh * gl * Yh * Xl
    b = Xh * Yl
    c = gh * Yl + gl * gh * Yh  # derived from chaining
    # chain: y = gh*Yh*x/(Xh+gh*x); x2 = gl*Xl*y/(Yl+gl*y) -> x2 = gh*gl*Yh*Xl*x / (Xh*Yl + gh*x*(Yl + gl*Yh))
    c = gh * (Yl + gl * Yh)
    x_opt = (math.sqrt(a * b) - b) / c
    p, s_hi2, s_lo2, y = cycle(hi, lo, x_opt)
    return dict(direction=d, gap_bps=g, x_sol=x_opt / 1e9, usdc_leg=y / 1e6, profit_sol=p / 1e9,
                net_sol=p / 1e9 - tip_sol - tx_fee_sol, profitable=p / 1e9 > tip_sol + tx_fee_sol,
                post_gap_bps=gap_bps(s_hi2, s_lo2))
