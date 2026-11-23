"""Build per-liquidation tables, competition and episode stats from parsed records.
usage: python3 -I analyze.py <records.jsonl> <reserves.json> <sol_hourly.json> <out_dir>
Writes liquidations_ltv.csv/json (successful LtvExceeded), liquidations_all_reasons.csv, attempts_summary.json,
episodes.json and prints summary stats.
"""
import sys, json, csv, collections, statistics as st, os, time, bisect

recs = [json.loads(l) for l in open(sys.argv[1])]
RES = json.load(open(sys.argv[2]))
solh = json.load(open(sys.argv[3]))["history"]
OUT = sys.argv[4]
SOL = "So11111111111111111111111111111111111111112"
STABLE = {"EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v", "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB",
          "2u1tszSeqZ3qBWF3uNGPFc8TzMk2tdiwknnRMWGWjGWH", "2b1kV6DkPAnxd5ixfnxCpjxmKwqjjaYmCZfHsFu24GXo",
          "USDSwr9ApdHk5bvJKMjzff41FfuX8bSxdKcR81vTwcA", "USD1ttGY1N17NEEHLmELoaybftRBUSErhqYiQzvEmuB"}
sol_ts = [time.mktime(time.strptime(x["timestamp"][:19], "%Y-%m-%dT%H:%M:%S")) - time.timezone for x in solh]
sol_px = [float(x["price"]) for x in solh]

def sol_price(t):
    i = max(0, min(len(sol_ts) - 1, bisect.bisect_right(sol_ts, t) - 1))
    return sol_px[i]

def reason(r): return (r.get("log_reason") or [None])[0]

def sym(res): return RES.get(res, {}).get("name", res[:6])

def atom_prices(r):
    """symbol -> USD per atom from the refresh_obligation logs of this tx."""
    p = {}
    for s, amt, val, *_ in (r.get("deposits") or []) + (r.get("borrows") or []):
        if amt > 0: p[s] = val / amt
    return p

def usd_of_mint(r, mint, atoms, ap):
    for res in (r["repay_reserve"], r["withdraw_reserve"]):
        R = RES.get(res)
        if R and R["mint"] == mint and R["name"] in ap:
            return atoms * ap[R["name"]]
    if mint == SOL: return atoms / 1e9 * sol_price(r["t"])
    if mint in STABLE: return atoms / 1e6
    for R in RES.values():  # current price fallback
        if R["mint"] == mint: return atoms / 10 ** R["decimals"] * R["price"]
    return None

rows = []
for r in recs:
    if not r["tx_ok"] or not r.get("log_pnl"): continue
    ap = atom_prices(r)
    rep, wd, pfee = map(int, r["log_pnl"])
    rs, ws = sym(r["repay_reserve"]), sym(r["withdraw_reserve"])
    rep_usd = rep * ap[rs] if rs in ap else None
    wd_usd = wd * ap[ws] if ws in ap else None
    pfee_usd = pfee * ap[ws] if ws in ap else None
    dep_tot = sum(d[2] for d in r.get("deposits") or [])
    bor_tot = sum(b[2] for b in r.get("borrows") or [])
    bf_tot = sum(b[3] for b in r.get("borrows") or [])
    el = r.get("log_elig")
    overshoot = (float(el[0]) / float(el[1]) - 1) if el and float(el[1]) > 0 else None
    solp = sol_price(r["t"])
    prio = r["fee"] - 5000 * r["nsig"]
    cost_lam = r["fee"] + r["jito_tip"] + sum(x[1] for x in r["other_transfers"])
    # profit: signer token deltas + sol delta (fee/tip already inside sol_delta)
    prof, unk = 0.0, []
    for mint, (d, dec) in r["tok_delta"].items():
        u = usd_of_mint(r, mint, d, ap)
        if u is None: unk.append(mint)
        else: prof += u
    prof += (r["sol_delta"] or 0) / 1e9 * solp
    # scope updates in the liquidation tx itself touching the collateral/debt price chains
    cchain = [e for e in RES.get(r["withdraw_reserve"], {}).get("price_chain", []) if e != 65535]
    dchain = [e for e in RES.get(r["repay_reserve"], {}).get("price_chain", []) if e != 65535]
    self_push = []
    for tk, kind, old, new, prev_s, new_s, crt in r["scope"]:
        if tk in cchain[:1] or tk in dchain[:1]:
            self_push.append({"tk": tk, "side": "coll" if tk in cchain[:1] else "debt", "move": (new / old - 1) if old else None,
                              "src_age_slots": crt - new_s, "prev_age_slots": crt - prev_s, "changed": old != new})
    rows.append({
        "sig": r["sig"], "slot": r["slot"], "idx": r["idx"], "time": time.strftime("%Y-%m-%d %H:%M:%S", time.gmtime(r["t"])),
        "t": r["t"], "reason": reason(r), "signer": r["signer"], "outer_program": r["outer_program"],
        "via_cpi": r["inner"] is not None, "market": r["market"], "obligation": r["obligation"],
        "repay_reserve": r["repay_reserve"], "repay_sym": rs, "repay_mint": r["repay_mint"],
        "withdraw_reserve": r["withdraw_reserve"], "withdraw_sym": ws, "withdraw_mint": r["withdraw_mint"],
        "repaid_atoms": rep, "repaid_usd": rep_usd, "withdrew_atoms": wd, "withdrew_usd": wd_usd,
        "protocol_fee_atoms": pfee, "protocol_fee_usd": pfee_usd,
        "bonus_bps": int(r["log_reason"][2]) if r.get("log_reason") else None,
        "realized_bonus_bps": ((wd_usd / rep_usd - 1) * 1e4) if rep_usd and wd_usd else None,
        "ltv_pct_log": int(el[2]) if el else None, "liq_ltv_pct_log": int(el[3]) if el else None,
        "health_overshoot": overshoot, "bf_debt_usd": bf_tot, "debt_usd": bor_tot, "deposits_usd": dep_tot,
        "n_deposits": len(r.get("deposits") or []), "n_borrows": len(r.get("borrows") or []),
        "share_of_debt_repaid": (rep_usd / bor_tot) if rep_usd and bor_tot else None,
        "arg_amount": r["arg_amount"], "arg_is_max": r["arg_amount"] >= 2 ** 63,
        "close_factor_pct": int(r["log_cf"][0]) if r.get("log_cf") else None,
        "flash_source": "kamino" if r["flash_borrows"] else ("jupiter-lend" if "jupgfSgfuAXv4B6R2Uxu85Z1qdzgju79s6MfZekN6XS" in r["programs"] else "none"),
        "flash": bool(r["flash_borrows"]), "flash_reserve": r["flash_borrows"][0]["reserve"] if r["flash_borrows"] else None,
        "flash_same_as_repay": bool(r["flash_borrows"]) and r["flash_borrows"][0]["reserve"] == r["repay_reserve"],
        "dex": "|".join(d for d in r["dex"] if d not in ("JupiterLendFlash", "JupiterLendBorrow", "VaultStakeRedeem")), "programs": "|".join(r["programs"]), "n_liq_in_tx": r["n_liq_in_tx"],
        "cu": r["cu"], "cu_limit": r["cu_limit"], "cu_price_microlamports": r["cu_price"],
        "priority_fee_lamports": prio, "jito_tip_lamports": r["jito_tip"],
        "other_transfer_lamports": sum(x[1] for x in r["other_transfers"]),
        "cost_usd": cost_lam / 1e9 * solp, "sol_usd": solp,
        "profit_usd_signer": prof if not unk else None, "profit_unpriced_mints": "|".join(unk),
        "self_scope_push": self_push,
        "deposits": r.get("deposits"), "borrows": r.get("borrows"), "refresh_reserves": r.get("refresh_reserves"),
        "scope_in_tx": r["scope"],
        "self_push_moved_coll": any(x["changed"] and x["side"] == "coll" for x in self_push),
        "self_push_moved_debt": any(x["changed"] and x["side"] == "debt" for x in self_push),
    })

rows.sort(key=lambda x: (x["slot"], x["idx"]))
os.makedirs(OUT, exist_ok=True)
ltv = [x for x in rows if x["reason"] == "LtvExceeded"]
json.dump(ltv, open(os.path.join(OUT, "liquidations_ltv.json"), "w"), indent=0)
cols = [k for k in ltv[0] if k not in ("self_scope_push", "deposits", "borrows", "refresh_reserves", "scope_in_tx")]
for name, data in (("liquidations_ltv.csv", ltv), ("liquidations_all_reasons.csv", rows)):
    with open(os.path.join(OUT, name), "w", newline="") as f:
        w = csv.DictWriter(f, fieldnames=cols, extrasaction="ignore"); w.writeheader(); w.writerows(data)

# ---------- attempts / competition ----------
by_obl = collections.defaultdict(list)
for r in recs:
    by_obl[r["obligation"]].append(r)
def err_kind(r):
    if r["tx_ok"]: return "ok"
    if r.get("log_healthy"): return "ObligationHealthy"
    if r.get("log_anchor"): return r["log_anchor"][0]
    if not r["executed"]: return "failed_before_liq_ix:" + json.dumps(r["tx_err"])[:60]
    return "other:" + json.dumps(r["tx_err"])[:60]

comp = []
for x in ltv:
    att = [a for a in by_obl[x["obligation"]] if abs(a["slot"] - x["slot"]) <= 20 and a["sig"] != x["sig"]]
    signers = {a["signer"] for a in att} - {x["signer"]}
    same_slot = [a for a in att if a["slot"] == x["slot"]]
    comp.append({"sig": x["sig"], "slot": x["slot"], "obligation": x["obligation"], "winner": x["signer"],
                 "attempts_pm20": len(att), "other_signers_pm20": len(signers),
                 "failed_pm20": sum(1 for a in att if not a["tx_ok"]), "ok_pm20": sum(1 for a in att if a["tx_ok"]),
                 "same_slot_attempts": len(same_slot),
                 "fail_kinds": dict(collections.Counter(err_kind(a) for a in att if not a["tx_ok"])),
                 "fail_before": sum(1 for a in att if not a["tx_ok"] and (a["slot"], a["idx"] or 0) < (x["slot"], x["idx"] or 0)),
                 "fail_after": sum(1 for a in att if not a["tx_ok"] and (a["slot"], a["idx"] or 0) > (x["slot"], x["idx"] or 0))})
json.dump(comp, open(os.path.join(OUT, "competition.json"), "w"), indent=0)

# all attempts by error kind and reason/market
kinds = collections.Counter((reason(r) or "-", err_kind(r)) for r in recs)
json.dump([[list(k), v] for k, v in kinds.most_common()], open(os.path.join(OUT, "attempts_summary.json"), "w"), indent=0)

# ---------- episodes: consecutive LtvExceeded liquidations of one obligation within 900 slots (~3 min) ----------
eps = []
byo = collections.defaultdict(list)
for x in ltv: byo[x["obligation"]].append(x)
for obl, xs in byo.items():
    xs.sort(key=lambda x: (x["slot"], x["idx"]))
    cur = [xs[0]]
    for x in xs[1:]:
        if x["slot"] - cur[-1]["slot"] <= 900: cur.append(x)
        else: eps.append(cur); cur = [x]
    eps.append(cur)
ep_out = [{"obligation": e[0]["obligation"], "market": e[0]["market"], "first_sig": e[0]["sig"], "first_slot": e[0]["slot"],
           "last_slot": e[-1]["slot"], "n": len(e), "n_signers": len({x["signer"] for x in e}),
           "pair": f'{e[0]["repay_sym"]}<-{e[0]["withdraw_sym"]}',
           "repaid_usd": sum(x["repaid_usd"] or 0 for x in e), "debt_usd_before": e[0]["debt_usd"],
           "deposits_usd_before": e[0]["deposits_usd"], "first_overshoot": e[0]["health_overshoot"]} for e in eps]
json.dump(ep_out, open(os.path.join(OUT, "episodes.json"), "w"), indent=0)

# ---------- summary ----------
def q(v, ps=(0.1, 0.25, 0.5, 0.75, 0.9)):
    v = sorted(a for a in v if a is not None)
    if not v: return None
    return {"n": len(v), **{f"p{int(p*100)}": round(v[min(len(v) - 1, int(p * len(v)))], 6) for p in ps}, "max": round(v[-1], 6), "sum": round(sum(v), 2)}

S = {}
S["n_records"] = len(recs)
S["n_success_by_reason"] = collections.Counter(x["reason"] for x in rows)
S["ltv_by_market"] = collections.Counter(x["market"] for x in ltv)
S["ltv_by_pair_top"] = collections.Counter(f'{x["repay_sym"]}<-{x["withdraw_sym"]}' for x in ltv).most_common(25)
S["ltv_by_day"] = collections.Counter(x["time"][:10] for x in ltv)
S["repaid_usd"] = q([x["repaid_usd"] for x in ltv])
S["debt_usd"] = q([x["debt_usd"] for x in ltv])
S["bonus_bps"] = collections.Counter(x["bonus_bps"] for x in ltv).most_common(12)
S["realized_bonus_bps"] = q([x["realized_bonus_bps"] for x in ltv])
S["health_overshoot"] = q([x["health_overshoot"] for x in ltv])
S["share_of_debt_repaid"] = q([x["share_of_debt_repaid"] for x in ltv])
S["flash_share"] = sum(x["flash"] for x in ltv) / len(ltv)
S["flash_source"] = collections.Counter(x["flash_source"] for x in ltv)
S["flash_same_reserve_share"] = sum(x["flash_same_as_repay"] for x in ltv) / len(ltv)
S["via_cpi_share"] = sum(x["via_cpi"] for x in ltv) / len(ltv)
S["dex_counts"] = collections.Counter(d for x in ltv for d in (x["dex"].split("|") if x["dex"] else ["none"])).most_common(20)
S["no_dex_share"] = sum(1 for x in ltv if not x["dex"]) / len(ltv)
S["winners_top"] = collections.Counter(x["signer"] for x in ltv).most_common(15)
S["n_winners"] = len({x["signer"] for x in ltv})
S["jito_tip_share_nonzero"] = sum(1 for x in ltv if x["jito_tip_lamports"] > 0) / len(ltv)
S["jito_tip_lamports"] = q([x["jito_tip_lamports"] for x in ltv if x["jito_tip_lamports"] > 0])
S["priority_fee_lamports"] = q([x["priority_fee_lamports"] for x in ltv])
S["cost_usd"] = q([x["cost_usd"] for x in ltv])
S["cu"] = q([x["cu"] for x in ltv])
S["profit_usd_signer"] = q([x["profit_usd_signer"] for x in ltv])
S["self_push_moved_coll_share"] = sum(x["self_push_moved_coll"] for x in ltv) / len(ltv)
S["self_push_any_share"] = sum(1 for x in ltv if x["self_scope_push"]) / len(ltv)
S["block_idx"] = q([x["idx"] for x in ltv])
S["competition_other_signers_pm20"] = collections.Counter(c["other_signers_pm20"] for c in comp).most_common()
S["competition_failed_pm20"] = q([c["failed_pm20"] for c in comp])
S["episodes"] = len(ep_out)
S["episode_len"] = collections.Counter(e["n"] for e in ep_out).most_common()
S["episode_repaid_usd"] = q([e["repaid_usd"] for e in ep_out])
S["episode_debt_usd_before"] = q([e["debt_usd_before"] for e in ep_out])
S["arg_is_max_share"] = sum(x["arg_is_max"] for x in ltv) / len(ltv)
json.dump(S, open(os.path.join(OUT, "summary.json"), "w"), indent=1, default=str)
print(json.dumps(S, indent=1, default=str)[:12000])
