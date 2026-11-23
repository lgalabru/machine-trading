"""Swap-by-swap price path of LST/SOL pools around the last 3 epoch boundaries.
For every tx touching a pool: the leg on that pool (pre/post marginal price where the venue's event
gives it, else the execution price), signer, tx index, and whether the tx is a cycle (arb).
Writes data/price_paths.json."""
import json, os, sys
sys.path.insert(0, os.path.dirname(__file__))
from sigs import sigs_until
from gettx import get_many
from legs import legs, WSOL

POOLS = {  # pool: (label, lst mint, venue)
    "Hp53XEtt4S8SvPCXarsLSdGfZBuUr5mMmZmX2DRNXQKp": ("JitoSOL/SOL Orca WP", "J1toso1uCk3RLmjorhTtrVwY9HJ7X8V9yYac6Y7kGCPn"),
    "2uoKbPEidR7KAMYtY4x7xdkHXWqYib5k4CutJauSL3Mc": ("JitoSOL/SOL Raydium CLMM", "J1toso1uCk3RLmjorhTtrVwY9HJ7X8V9yYac6Y7kGCPn"),
    "BoeMUkCLHchTD31HdXsbDExuZZfcUppSLpYtV3LZTH6U": ("JitoSOL/SOL Meteora DLMM", "J1toso1uCk3RLmjorhTtrVwY9HJ7X8V9yYac6Y7kGCPn"),
    "ERgpKaq59Nnfm9YRVAAhnq16cZhHxGcDoDWCzXbhiaNw": ("JitoSOL/SOL Meteora DAMMv1", "J1toso1uCk3RLmjorhTtrVwY9HJ7X8V9yYac6Y7kGCPn"),
    "DtYKbQELgMZ3ihFUrCcCs9gy4djcUuhwgR7UpxVpP2Tg": ("jupSOL/SOL Orca WP", "jupSoLaHXQiZZTSfEWMTRRgpnyFm8f6sZdosWBjx93v"),
    "8EzbUfvcRT1Q6RL462ekGkgqbxsPmwC5FMLQZhSPMjJ3": ("mSOL/SOL Raydium CLMM", "mSoLzYCxHdYgdzU16g5QSh3i5K3z3KZK7ytfqcJm7So"),
}
BOUNDS = [471312000, 471744000, 472176000]
HALF = int(os.environ.get("HALF", "15000"))

def main():
    out = {}
    for pool, (label, lst) in POOLS.items():
        s = sigs_until(pool, BOUNDS[0] - HALF)
        w = sorted([x for x in s if any(abs(x["slot"] - B) <= HALF for B in BOUNDS) and not x["err"]], key=lambda x: x["slot"])
        print(label, len(w), "txs", flush=True)
        txs = get_many([x["sig"] for x in w])
        pts = []
        for x, tx in zip(w, txs):
            if tx is None: continue
            try: ls, signer = legs(tx)
            except Exception as e: continue
            mine = [l for l in ls if l["pool"] == pool or pool in (l["vault_in"], l["vault_out"])]
            if not mine:
                ks = tx["transaction"]["message"]["accountKeys"]
                # liquidity ops etc.
                continue
            l = mine[0]
            # orient: SOL per LST
            def orient(px):
                if px is None: return None
                if l["mint_in"] == lst and l["mint_out"] == WSOL: return px
                if l["mint_in"] == WSOL and l["mint_out"] == lst: return 1 / px
                return None
            path = [ls[0]["mint_in"]] + [q["mint_out"] for q in ls]
            pts.append({"slot": x["slot"], "idx": tx.get("transactionIndex"), "sig": x["sig"], "signer": signer,
                        "side": "sell_lst" if l["mint_in"] == lst else "buy_lst",
                        "lst_amt": (l["amt_in"] if l["mint_in"] == lst else l["amt_out"]) / 1e9,
                        "pre": orient(l.get("pre_px")), "post": orient(l.get("post_px")), "exec": orient(l.get("exec_px")),
                        "dlmm": l.get("dlmm"), "cycle": len(ls) >= 2 and path[0] == path[-1], "nlegs": len(ls),
                        "venues": [q["venue"] for q in ls]})
        out[pool] = {"label": label, "lst": lst, "points": pts}
    json.dump(out, open(os.path.join(os.path.dirname(__file__), "../data/price_paths.json"), "w"))

if __name__ == "__main__":
    main()
