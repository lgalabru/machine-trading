"""Sample txs touching a stake pool account; find cycles that use DepositSol/WithdrawSol as a leg
(stake-pool mint/redeem arbs) and txs that only read the pool (oracle use). Writes data/stakepool_arbs_<name>.json"""
import collections, json, os, sys
sys.path.insert(0, os.path.dirname(__file__))
from gettx import get_many
from legs import legs, keys_of, WSOL
from rate_series import classify, POOLS

def sol_delta(tx, who):
    ks = keys_of(tx); m = tx["meta"]
    d = 0
    if who in ks:
        i = ks.index(who); d += m["postBalances"][i] - m["preBalances"][i]
    for side, sgn in (("postTokenBalances", 1), ("preTokenBalances", -1)):
        for b in m.get(side) or []:
            if b["mint"] == WSOL and b.get("owner") == who: d += sgn * int(b["uiTokenAmount"]["amount"])
    return d

def main(name, step):
    p = POOLS[name]
    s = [x for x in json.load(open(os.path.join(os.path.dirname(__file__), f"../data/sigs/{p['address']}.json"))) if not x["err"]]
    samp = s[::step]
    txs = get_many([x["sig"] for x in samp], workers=10)
    rows = []; readers = collections.Counter()
    for x, t in zip(samp, txs):
        if t is None: continue
        names, mints, burns, li, lo = classify(t, p)
        sp = [n for n in names if n in ("DepositSol", "WithdrawSol", "DepositStake", "WithdrawStake")]
        try: ls, signer = legs(t)
        except Exception: ls, signer = [], keys_of(t)[0]
        top = [ix["programId"] for ix in t["transaction"]["message"]["instructions"]]
        inner = [ix["programId"] for g in t["meta"].get("innerInstructions") or [] for ix in g["instructions"]]
        progs = set(top + inner)
        if ls and not sp:
            for q in ("5ocnV1qiCgaQR8Jb8xWnVbApfaygJ8tNoZfgPwsgx9kx", "stkitrT1Uoy18Dk1fTrgPw8W6MVzoCfYoAFT4MLsmhq", "ALPHAQmeA7bjrVuccPsYPiCvsi428SNwte66Srvs4pHA",
                      "MNFSTqtC93rEfYHB6hF82sKdZpUDFWkViLByLd1k1Ms"):
                if q in progs: readers[q[:8]] += 1
            readers["swap_tx_total"] += 1
        if sp and ls:
            rows.append({"slot": x["slot"], "idx": t.get("transactionIndex"), "sig": x["sig"], "signer": signer, "stake_ix": sp,
                         "lamports_in": li, "lamports_out": lo, "pool_minted": mints, "pool_burned": burns,
                         "rate": (li / mints if li and mints else (lo / burns if lo and burns else None)),
                         "venues": [l["venue"] for l in ls], "pools": [l["pool"] for l in ls],
                         "path": [l["mint_in"][:5] + ">" + l["mint_out"][:5] for l in ls],
                         "signer_sol_delta": sol_delta(t, signer) / 1e9, "fee": t["meta"]["fee"] / 1e9})
    out = {"name": name, "sampled": len(samp), "of": len(s), "step": step, "readers": readers, "arbs": rows}
    json.dump(out, open(os.path.join(os.path.dirname(__file__), f"../data/stakepool_arbs_{name}.json"), "w"), indent=1)
    print(name, len(samp), "sampled;", len(rows), "stake-pool-leg cycles;", dict(readers))

if __name__ == "__main__":
    main(sys.argv[1], int(sys.argv[2]))
