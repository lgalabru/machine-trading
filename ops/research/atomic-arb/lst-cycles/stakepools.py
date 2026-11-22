"""Locate and decode SPL stake pool accounts for LST mints; Marinade state for mSOL. Writes data/stakepools_now.json."""
import base64, json, os, struct, sys
sys.path.insert(0, os.path.dirname(__file__))
import rpc
from legs import b58e, b58d

PROGRAMS = ["SPoo1Ku8WFXoNDMHPsrGSTSG1Y47rzgn41SLUNakuHy", "SP12tWFxD9oJsVWNavTTBZvMbA6gkAmxtVgxdqvyvhY", "SPMBzsVUuoHA4Jm6KunbsotaahvVikZs1JyTW6iJvbn"]
MINTS = {"JitoSOL": "J1toso1uCk3RLmjorhTtrVwY9HJ7X8V9yYac6Y7kGCPn", "jupSOL": "jupSoLaHXQiZZTSfEWMTRRgpnyFm8f6sZdosWBjx93v",
         "bSOL": "bSo13r4TkiE4KumL71LsHTPpL2euBYLFx6h9HP3piy1", "hSOL": "he1iusmfkpAdwvxLNGV8Y1iSbj4rUy6yMhEA3fotn9A",
         "pSOL": "pSo1f9nQXWgXibFtKf7NWYxb5enAM4qfP6UJSiXRQfL", "dSOL": "Dso1bDeDjCQxTrWHqUUi63oBvV7Mdm6WaobLbQ7gnPQ",
         "bonkSOL": "BonK1YhkXEGLZzwtcvRTip3gAL9nCeQD7ppZBLXhtTs", "vSOL": "vSoLxydx6akxyMD9XEcPvGYNYbVvMxbDuY2tXfp6oL"}
MARINADE_STATE = "8szGkuLTAux9XMgZ2vtY39jVSowEcpBfFfD8hXSEqdGC"

def decode_pool(data):
    pk = lambda o: b58e(data[o:o + 32])
    o = 1; manager = pk(o); o += 32; staker = pk(o); o += 32; dep = pk(o); o += 32; o += 1
    vlist = pk(o); o += 32; reserve = pk(o); o += 32; mint = pk(o); o += 32; mfee = pk(o); o += 32; tprog = pk(o); o += 32
    total, supply, last_epoch = struct.unpack_from("<3Q", data, o); o += 24
    o += 8 + 8 + 32  # lockup
    fden, fnum = struct.unpack_from("<2Q", data, o); o += 16
    def fee():
        nonlocal o
        d, n = struct.unpack_from("<2Q", data, o); o += 16
        return [n, d]
    def future_fee():
        nonlocal o
        tag = data[o]; o += 1
        return fee() if tag else None
    def opt_pk():
        nonlocal o
        tag = data[o]; o += 1
        if tag: v = pk(o); o += 32; return v
        return None
    nxt_epoch_fee = future_fee(); pref_dep = opt_pk(); pref_wd = opt_pk()
    stake_dep_fee = fee(); stake_wd_fee = fee(); nxt_stake_wd = future_fee(); stake_ref = data[o]; o += 1
    sol_dep_auth = opt_pk(); sol_dep_fee = fee(); sol_ref = data[o]; o += 1; sol_wd_auth = opt_pk()
    sol_wd_fee = fee(); nxt_sol_wd = future_fee()
    le_supply, le_total = struct.unpack_from("<2Q", data, o)
    extra = {"stake_deposit_fee": stake_dep_fee, "stake_withdrawal_fee": stake_wd_fee, "sol_deposit_fee": sol_dep_fee,
             "sol_withdrawal_fee": sol_wd_fee, "sol_deposit_authority": sol_dep_auth, "sol_withdraw_authority": sol_wd_auth,
             "next_epoch_fee": nxt_epoch_fee, "next_sol_withdrawal_fee": nxt_sol_wd,
             "last_epoch_pool_token_supply": le_supply, "last_epoch_total_lamports": le_total,
             "last_epoch_rate": le_total / le_supply if le_supply else None}
    return {**extra, "validator_list": vlist, "reserve_stake": reserve, "pool_mint": mint, "manager_fee_account": mfee,
            "total_lamports": total, "pool_token_supply": supply, "last_update_epoch": last_epoch,
            "epoch_fee": [fnum, fden], "rate": total / supply if supply else None}

def main():
    out = {}
    for name, mint in MINTS.items():
        for prog in PROGRAMS:
            res = rpc.call("getProgramAccounts", [prog, {"encoding": "base64", "filters": [{"memcmp": {"offset": 162, "bytes": mint}}]}])
            for a in res:
                d = base64.b64decode(a["account"]["data"][0])
                if d[0] != 1: continue
                p = decode_pool(d); p["address"] = a["pubkey"]; p["program"] = prog
                out[name] = p
            if name in out: break
    # Marinade: msol_price u64 (x 2^32) lives in State; read via known offset search is brittle, use total/supply proxy
    acc = rpc.call("getAccountInfo", [MARINADE_STATE, {"encoding": "base64"}])
    d = base64.b64decode(acc["value"]["data"][0])
    out["mSOL_state_len"] = len(d)
    out["_epoch"] = rpc.call("getEpochInfo", [])
    json.dump(out, open(os.path.join(os.path.dirname(__file__), "../data/stakepools_now.json"), "w"), indent=1)
    for k, v in out.items():
        if isinstance(v, dict) and "rate" in v: print(k, v["address"], v["program"][:6], v["last_update_epoch"], round(v["rate"], 9), "step_bps", v["last_epoch_rate"] and round((v["rate"]/v["last_epoch_rate"]-1)*1e4, 3), "epoch_fee", v["epoch_fee"], "sol_dep", v["sol_deposit_fee"], "sol_wd", v["sol_withdrawal_fee"], "stake_wd", v["stake_withdrawal_fee"], "dep_auth", v["sol_deposit_authority"], "TVL_SOL", round(v["total_lamports"]/1e9))

if __name__ == "__main__":
    main()
