"""Run legs.py over every round1 tx; write data/round1_cycles.json (+csv)."""
import csv, glob, json, math, os, sys
sys.path.insert(0, os.path.dirname(__file__))
from legs import legs, WSOL

R1 = "/tmp/arb-research/round1"
OUT = "/tmp/arb-research/lst-triangular/data"
SYM = {WSOL: "SOL", "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v": "USDC", "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB": "USDT",
       "J1toso1uCk3RLmjorhTtrVwY9HJ7X8V9yYac6Y7kGCPn": "JitoSOL", "jupSoLaHXQiZZTSfEWMTRRgpnyFm8f6sZdosWBjx93v": "jupSOL",
       "mSoLzYCxHdYgdzU16g5QSh3i5K3z3KZK7ytfqcJm7So": "mSOL", "bSo13r4TkiE4KumL71LsHTPpL2euBYLFx6h9HP3piy1": "bSOL",
       "5oVNBeEEQvYi1cX3ir8Dx5n1P7pdxydbGF2X4TxVusJm": "INF", "he1iusmfkpAdwvxLNGV8Y1iSbj4rUy6yMhEA3fotn9A": "hSOL",
       "pSo1f9nQXWgXibFtKf7NWYxb5enAM4qfP6UJSiXRQfL": "pSOL", "jtojtomepa8beP8AuQc6eXt5FriJwfFMwQx2v2f9mCL": "JTO",
       "cbbtcf3aa214zXHbiAZQwf4122FBYbraNdFqgw4iMij": "cbBTC", "DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263": "BONK",
       "JUPyiwrYJFskUPiHa7hkeR8VUtAeFoSYbKedZNsDvCN": "JUP", "27G8MtK7VtTcCHkpASjSDdkWWYfoqT6ggEuKidVJidD4": "JLP",
       "WENWENvqqNya429ubCdR81ZmD69brwQaaBYY6p3LCpk": "WEN", "4sWNB8zGWHkh6UnmwiEtzNxL4XrN7uK9tosbESbJFfVs": "xSOL(Hylo)",
       "oreoU2P8bN6jkk3jbformuhfxzoEWdfwpQ8wvbcXTZ": "ORE", "7JA5eZdCzztSfQbJvS8aVVxMFfd81Rs9VvwnocV1mKHu": "?7JA5e",
       "LSTxxxnJzKDFSLr4dUkPcmCf5VyryEqzPLz5j4bpxFp": "LST(Sanctum)", "HYp9vNfVZgzwamFEmYHm8hz2djsmXMnebNq2tjwvq4Hd": "?HYp9v",
       "9n4nbM75f5Ui33ZbPYXn59EwSgE8CGsHtAeTH5YFeJ9E": "soBTC", "8nKJ4z9FSw6wrVZKASqBiS9DS1CiNsRnqwCCKVQjqdkB": "?8nKJ4"}
LST = {"JitoSOL", "jupSOL", "mSOL", "bSOL", "INF", "hSOL", "pSOL", "xSOL(Hylo)"}
def sym(m): return SYM.get(m, m[:5])

def main():
    recs = {}
    for f in ("top200.recs.json", "last200.recs.json"):
        for r in json.load(open(f"{R1}/{f}")): r["set"] = f.split(".")[0]; recs[r["sig"]] = r
    rows, allc = [], []
    for sig, r in recs.items():
        p = f"{R1}/tx/{sig}.json"
        if not os.path.exists(p): continue
        tx = json.load(open(p)); tx = tx.get("result", tx)
        try: ls, signer = legs(tx)
        except Exception as e: print("ERR", sig[:10], e); continue
        path = [sym(ls[0]["mint_in"])] + [sym(l["mint_out"]) for l in ls] if ls else []
        lst = sorted({s for s in path if s in LST})
        # edge = sum over legs of log(pre_px/ref) is unknown here; record exec and pre prices
        c = {"sig": sig, "set": r["set"], "slot": r["slot"], "epoch": r["slot"] // 432000, "slot_in_epoch": r["slot"] % 432000,
             "profit_sol": r["profit"], "signer": signer, "tip": r.get("tip"), "fee": r.get("fee"), "cu": r.get("cu"),
             "tx_index": tx.get("transactionIndex"), "cat": r["cat"], "path": path, "nlegs": len(ls),
             "lst": lst, "venues": [l["venue"] for l in ls], "pools": [l["pool"] for l in ls], "legs": ls}
        allc.append(c)
    json.dump(allc, open(f"{OUT}/round1_cycles.json", "w"), indent=1)
    with open(f"{OUT}/round1_cycles.csv", "w", newline="") as fh:
        w = csv.writer(fh)
        w.writerow(["set", "sig", "slot", "epoch", "slot_in_epoch", "profit_sol", "signer", "tip", "tx_index", "cat", "nlegs", "path", "venues", "lst", "size_in"])
        for c in sorted(allc, key=lambda c: (c["set"], -c["profit_sol"])):
            si = c["legs"][0]["amt_in"] / 10 ** (c["legs"][0]["dec_in"] or 0) if c["legs"] else None
            w.writerow([c["set"], c["sig"], c["slot"], c["epoch"], c["slot_in_epoch"], round(c["profit_sol"], 6), c["signer"], c["tip"], c["tx_index"], c["cat"], c["nlegs"], ">".join(c["path"]), "|".join(c["venues"]), "|".join(c["lst"]), si])
    print(len(allc), "txs;", sum(1 for c in allc if c["nlegs"] == 0), "with no legs")

if __name__ == "__main__":
    main()
