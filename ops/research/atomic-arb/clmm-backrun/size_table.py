"""Trigger size on the thin pool -> gap and two-pool optimum (constant-L), for generator calibration."""
import sys, os, math, json
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import arbmath
P = 70.0
rows = []
for LW, LR, tag in ((4.06e14, 8.9e13, "Nov21 L"), (3.8e14, 7.0e13, "Nov20 L"), (1.5e14, 1.9e13, "Nov14 L (thin)")):
    for size in (50, 100, 150, 200, 300, 500, 750, 1000, 1500):
        s = math.sqrt(P / 1e3)
        _, sR = arbmath.sell_sol(s, LR, 4e-4, size * 1e9)
        o = arbmath.optimal((s, LW, 4e-4), (sR, LR, 4e-4))
        rows.append(dict(L=tag, trigger_sol_into_R=size, gap_bps=round(o["gap_bps"], 1), opt_in_sol=round(o["x_sol"], 1), opt_gross_sol=round(o["profit_sol"], 4)))
for r in rows: print(r)
json.dump(rows, open(os.path.join(os.path.dirname(__file__), "..", "data", "size_table.json"), "w"), indent=1)
