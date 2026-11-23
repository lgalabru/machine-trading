"""Contested Kamino LtvExceeded liquidations: winner + every other attempt on the same obligation within
winner slot +-2. Input: liq.jsonl.gz from klend_collect.py (one or more dirs). Output: validation/kamino_contests.json
usage: python3 -I kamino_contests.py DIR [DIR...]"""
import sys, os, json, gzip, collections
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import parse_liq, lib, cache, txfields as tf

MAIN = "7u3HeHxYDLhnCoErrtycNokbQYbWGzLs6JSDqGAv5PfF"
SCOPE = "HFn8GnPADiny6XqUoWE8uRPPxb29ikn4yTuPa9MF2fWJ"
MAXN = 40

def trim(tx):
    meta = tx["meta"]
    return {"slot": tx["slot"], "idx": tx.get("transactionIndex"), "blockTime": tx.get("blockTime"), "version": tx.get("version"),
            "sig": tx["transaction"]["signatures"][0], "keys": lib.all_keys(tx), "header": tx["transaction"]["message"]["header"],
            "ixs": tx["transaction"]["message"]["instructions"], "inner": meta.get("innerInstructions"), "logs": meta.get("logMessages"),
            "err": meta.get("err"), "fee": meta.get("fee"), "cu": meta.get("computeUnitsConsumed"), "pre": meta.get("preBalances"),
            "post": meta.get("postBalances"), "preTok": meta.get("preTokenBalances"), "postTok": meta.get("postTokenBalances")}

raw, recs = {}, []
for d in sys.argv[1:]:
    for l in gzip.open(os.path.join(d, "liq.jsonl.gz"), "rt"):
        t = json.loads(l); sig = t["transaction"]["signatures"][0]
        if sig in raw: continue
        raw[sig] = t
        for r in parse_liq.parse(trim(t)):
            recs.append(r)
print("txs", len(raw), "liq ix records", len(recs))

# learn repay mint -> borrow symbol from single-borrow obligations
mint_sym = collections.Counter()
for r in recs:
    b = r.get("borrows") or []
    if len(b) == 1: mint_sym[(r["repay_mint"], b[0][0])] += 1
M2S = {}
for (m, s), n in mint_sym.most_common():
    M2S.setdefault(m, s)

def repay_usd(r):
    if not r.get("log_pnl"): return None
    rep = int(r["log_pnl"][0]); b = r.get("borrows") or []
    sym = b[0][0] if len(b) == 1 else M2S.get(r["repay_mint"])
    for s, amt, val, *_ in b:
        if s == sym and amt > 0: return rep * val / amt
    return None

def failure_reason(r):
    if r["tx_ok"]: return None
    if r.get("log_anchor"): return r["log_anchor"][0]
    return json.dumps(r["tx_err"])

by_obl = collections.defaultdict(list)
for r in recs: by_obl[r["obligation"]].append(r)

cands = []
for r in recs:
    if not (r["tx_ok"] and r.get("log_reason") and r["log_reason"][0] == "LtvExceeded"): continue
    ob = by_obl[r["obligation"]]
    # winner = first success on this obligation in the +-2 window (skip if an earlier success exists within 2 slots)
    if any(o["tx_ok"] and o.get("log_pnl") and (o["slot"], o["idx"]) < (r["slot"], r["idx"]) and r["slot"] - o["slot"] <= 2 for o in ob):
        continue
    others = [o for o in ob if abs(o["slot"] - r["slot"]) <= 2 and o["sig"] != r["sig"]]
    fail_signers = {o["signer"] for o in others if not o["tx_ok"] and o["signer"] != r["signer"]}
    healthy_signers = {o["signer"] for o in others if failure_reason(o) == "ObligationHealthy" and o["signer"] != r["signer"]}
    cands.append(dict(r=r, others=others, usd=repay_usd(r) or 0, nfail=len(fail_signers), nhealthy=len(healthy_signers)))
print("winners", len(cands), "main", sum(c["r"]["market"] == MAIN for c in cands),
      "main >=1k & >=2 healthy-fail signers", sum(c["r"]["market"] == MAIN and c["usd"] >= 1000 and c["nhealthy"] >= 2 for c in cands))

def tier(c):
    m = c["r"]["market"] == MAIN
    if m and c["usd"] >= 1000 and c["nhealthy"] >= 2: return 0
    if m and c["usd"] >= 100 and c["nhealthy"] >= 2: return 1
    if c["usd"] >= 1000 and c["nhealthy"] >= 2: return 2
    return 9
pick = sorted([c for c in cands if tier(c) < 9], key=lambda c: (tier(c), -c["usd"]))[:MAXN]
print("picked by tier", collections.Counter(tier(c) for c in pick))

blocks = {}
def blk(s):
    if s not in blocks: blocks[s] = cache.block(s)
    return blocks[s]
tipcache = {}
def tipflags(s):
    if s not in tipcache: tipcache[s] = tf.block_tipflags(blk(s))
    return tipcache[s]

def scope_refreshes(t):
    logs = t["meta"].get("logMessages") or []
    ks = lib.all_keys(t)
    if SCOPE not in ks: return None
    tks = [m.group(1) for l in logs for m in [parse_liq.RE["scope"].search(l)] if m]
    called = any(l.startswith(f"Program {SCOPE} invoke") for l in logs)
    return dict(called=called, entries=tks)

out = []
for c in pick:
    r = c["r"]; s0 = r["slot"]
    atts = sorted([r] + c["others"], key=lambda o: (o["slot"], o["idx"]))
    seen = set(); conts = []
    for o in atts:
        if o["sig"] in seen: continue
        seen.add(o["sig"])
        t = raw[o["sig"]]; b = blk(o["slot"])
        fl, sg = tipflags(o["slot"])
        f = tf.fields(t, o["idx"])
        assert b["transactions"][o["idx"]]["transaction"]["signatures"][0] == o["sig"]
        sr = scope_refreshes(t)
        f.update(slot=o["slot"], slot_offset=o["slot"] - s0, block_ntx=len(b["transactions"]), is_winner=(o["sig"] == r["sig"]),
                 failure_reason=failure_reason(o), liq_reason=(o.get("log_reason") or [None])[0],
                 repay_usd=repay_usd(o), scope_refresh_in_tx=bool(sr and sr["entries"]), scope_entries_in_tx=(sr or {}).get("entries"),
                 bundle_guess=tf.bundle_guess(b["transactions"], o["idx"], fl, sg), outer_program=o.get("outer_program"))
        conts.append(f)
    # scope refreshes in winner's block before the winner
    b = blk(s0); prior = []
    for i in range(r["idx"] - 1, -1, -1):
        t = b["transactions"][i]
        sr = scope_refreshes(t)
        if sr and sr["entries"] and t["meta"]["err"] is None:
            prior.append(dict(index=i, signature=t["transaction"]["signatures"][0], signer=lib.all_keys(t)[0],
                              entries=sr["entries"]))
            if len(prior) >= 5: break
    w = next(x for x in conts if x["is_winner"])
    trig = dict(scope_refresh_in_winner_tx=w["scope_refresh_in_tx"], scope_entries_in_winner_tx=w["scope_entries_in_tx"],
                latest_scope_refreshes_earlier_in_block=prior,
                note="which Scope entry the obligation depends on is not resolved; 'latest_scope_refreshes_earlier_in_block' lists up to 5 successful refreshes before the winner, newest first")
    out.append(dict(id=f"kamino:{s0}:{r['obligation'][:8]}", source=f"klend getTransactionsForAddress, tier {tier(c)}",
                    slot=s0, winner_index=r["idx"], winner_signature=r["sig"], winner_signer=r["signer"],
                    market=r["market"], obligation=r["obligation"], repay_mint=r["repay_mint"],
                    repay_usd=c["usd"], bonus_bps=(r.get("log_reason") or [None, None, None])[2],
                    n_other_signers_failed=c["nfail"], n_other_signers_obligation_healthy=c["nhealthy"],
                    trigger=trig, contenders=conts))
    print(out[-1]["id"], round(c["usd"]), "idx", r["idx"], "nconts", len(conts), "healthy signers", c["nhealthy"], flush=True)
json.dump(out, open("/tmp/arb-research/validation/kamino_contests.json", "w"), indent=1)
print("cases", len(out))
