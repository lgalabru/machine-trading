"""Stake pool exchange rate around epoch boundaries, measured from DepositSol/WithdrawSol txs, and the
slot of the first UpdateStakePoolBalance of each epoch. Writes data/rate_steps.json."""
import json, os, sys
sys.path.insert(0, os.path.dirname(__file__))
from sigs import sigs_until
from gettx import get_many

POOLS = json.load(open(os.path.join(os.path.dirname(__file__), "../data/stakepools_now.json")))
BOUNDS = [471312000, 471744000, 472176000]
WIN = (-6000, 6000)

def flat(tx):
    out = []
    for ix in tx["transaction"]["message"]["instructions"]: out.append(ix)
    for g in tx["meta"].get("innerInstructions") or []: out += g["instructions"]
    return out

def classify(tx, pool):
    logs = tx["meta"]["logMessages"] or []
    names = [l.split("Instruction: ")[1] for l in logs if "Program log: Instruction: " in l]
    mint, reserve = pool["pool_mint"], pool["reserve_stake"]
    mints = burns = lam_in = lam_out = 0
    for ix in flat(tx):
        p = ix.get("parsed")
        if not isinstance(p, dict): continue
        t, inf = p.get("type"), p.get("info", {})
        if t in ("mintTo", "mintToChecked") and inf.get("mint") == mint:
            mints += int(inf.get("amount") or inf["tokenAmount"]["amount"])
        elif t in ("burn", "burnChecked") and inf.get("mint") == mint:
            burns += int(inf.get("amount") or inf["tokenAmount"]["amount"])
        elif t == "transfer" and ix.get("program") == "system" and inf.get("destination") == reserve:
            lam_in += int(inf["lamports"])
        elif t == "withdraw" and ix.get("program") == "stake" and inf.get("stakeAccount") == reserve:
            lam_out += int(inf["lamports"])
    return names, mints, burns, lam_in, lam_out

def main():
    out = {}
    for name in ("JitoSOL", "jupSOL", "bSOL", "hSOL", "dSOL", "bonkSOL", "pSOL"):
        pool = POOLS[name]; addr = pool["address"]
        s = sigs_until(addr, BOUNDS[0] + WIN[0])
        res = {"address": addr, "epoch_fee": pool["epoch_fee"], "boundaries": []}
        for B in BOUNDS:
            w = sorted([x for x in s if B + WIN[0] <= x["slot"] < B + WIN[1] and not x["err"]], key=lambda x: x["slot"])
            if len(w) > 1500:  # thin out deposits far from the boundary
                near = [x for x in w if abs(x["slot"] - B) < 1500]; far = [x for x in w if abs(x["slot"] - B) >= 1500][::10]
                w = sorted(near + far, key=lambda x: x["slot"])
            txs = get_many([x["sig"] for x in w])
            pts, upd = [], []
            for x, tx in zip(w, txs):
                if tx is None: continue
                names, mints, burns, li, lo = classify(tx, pool)
                if "UpdateStakePoolBalance" in names: upd.append({"slot": x["slot"], "sig": x["sig"], "fee_minted": mints, "signer": tx["transaction"]["message"]["accountKeys"][0]["pubkey"]})
                elif li and mints and "DepositSol" in names: pts.append({"slot": x["slot"], "kind": "DepositSol", "rate": li / mints})
                elif lo and burns and "WithdrawSol" in names: pts.append({"slot": x["slot"], "kind": "WithdrawSol", "rate": lo / burns})
            first = upd[0] if upd else None
            before = [p["rate"] for p in pts if p["slot"] < B]
            after = [p["rate"] for p in pts if first and p["slot"] > first["slot"]]
            between = [p for p in pts if B <= p["slot"] and (not first or p["slot"] <= first["slot"])]
            r = {"boundary": B, "epoch": B // 432000, "first_update": first, "n_updates": len(upd),
                 "n_points": len(pts), "rate_before": (sorted(before)[len(before) // 2] if before else None),
                 "rate_after": (sorted(after)[len(after) // 2] if after else None),
                 "between_update": [(p["slot"] - B, p["kind"], p["rate"]) for p in between][:20], "points": pts}
            if r["rate_before"] and r["rate_after"]: r["step_bps"] = (r["rate_after"] / r["rate_before"] - 1) * 1e4
            res["boundaries"].append(r)
            print(name, B, "update@", first and first["slot"] - B, "pts", len(pts), "before", len(before), "after", len(after), "step_bps", r.get("step_bps"))
        out[name] = res
    json.dump(out, open(os.path.join(os.path.dirname(__file__), "../data/rate_steps.json"), "w"), indent=1)

if __name__ == "__main__":
    main()
