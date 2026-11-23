"""Rebuild the W<->R SOL/USDC backrun cases: trigger, contenders, fees/tips.
Inputs: /tmp/arb-research/explorer/wr_arbs.json (explorer), whale_cases.json (whale windows).
Output: /tmp/arb-research/validation/sol_usdc_cases.json"""
import sys, os, json, math
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import cache, lib, txfields as tf

W = "Czfq3xZZDmsdGdUyrNLtRhGc47cXcZtLG4crryfu44zE"; R = "3ucNos4NbumPLZNWztqGHNFFgkHeRMBQAVemeeomsUxv"
POOL = {W: "W", R: "R"}
Q64 = 2 ** 64
IMPACT_BPS = 3.0

def impacts(tx):
    """net price move in bps per pool (W, R) from swap events; None if no event."""
    if tx["meta"]["err"] is not None: return {}
    out = {}
    for e in lib.events_from_tx(tx):
        if e["pool"] not in POOL: continue
        if e["prog"] == "whirl":
            r = (e["post_sqrt"] / e["pre_sqrt"]) ** 2
        else:
            L = e["liquidity"]
            if not L: continue
            d = e["amt1"] * Q64 / L
            pre = e["post_sqrt"] + d if e["zero_for_one"] else e["post_sqrt"] - d * (1 - 0.0004)
            r = (e["post_sqrt"] / pre) ** 2
        k = POOL[e["pool"]]
        out[k] = out.get(k, 1.0) * r
    return {k: (v - 1) * 1e4 for k, v in out.items()}

def signer_gross(tx, tip):
    m = tx["meta"]; ks = lib.all_keys(tx); s = ks[0]
    sol = m["postBalances"][0] - m["preBalances"][0]
    usdc = 0
    for side, sg in (("preTokenBalances", -1), ("postTokenBalances", 1)):
        for b in m.get(side) or []:
            if b.get("owner") != s: continue
            a = int(b["uiTokenAmount"]["amount"])
            if b["mint"] == lib.SOL: sol += sg * a
            elif b["mint"] == lib.USDC: usdc += sg * a
    return dict(net_sol=sol / 1e9, gross_sol=(sol + m["fee"] + tip) / 1e9, usdc_delta=usdc / 1e6,
                note="signer-owned SOL + wSOL balance change; gross adds back fee and tip; misses funds held in PDAs")

def pool_flow(tx):
    """SOL and USDC the tx took out of W and R net (positive = profit), from swap events."""
    sol = usdc = 0
    for e in lib.events_from_tx(tx) if tx["meta"]["err"] is None else []:
        if e["pool"] not in POOL: continue
        if e["prog"] == "whirl":
            if e["a_to_b"]: sol -= e["amt_in"]; usdc += e["amt_out"]
            else: usdc -= e["amt_in"]; sol += e["amt_out"]
        else:
            if e["zero_for_one"]: sol -= e["amt0"]; usdc += e["amt1"]
            else: sol += e["amt0"]; usdc -= e["amt1"]
    return dict(sol=sol / 1e9, usdc=usdc / 1e6)

def blk_of(slot):
    b = cache.block(slot)
    return b

def next_block(slot):
    for s in range(slot + 1, slot + 6):
        b = blk_of(s)
        if b is not None: return s, b
    return None, None

def build(case):
    slot, sig = case["slot"], case["sig"]
    blk = blk_of(slot)
    txs = blk["transactions"]
    widx = next(i for i, t in enumerate(txs) if t["transaction"]["signatures"][0] == sig)
    wtx = txs[widx]; wsigner = lib.all_keys(wtx)[0]
    # sequence back: this block (before winner), then up to 2 parent blocks
    seq_blocks = [(slot, blk)]
    ps = blk["parentSlot"]
    for _ in range(2):
        pb = blk_of(ps); seq_blocks.append((ps, pb)); ps = pb["parentSlot"]
    wim = impacts(wtx)
    trig = literal = None
    for bi, (s, b) in enumerate(seq_blocks):
        rng = range(widx - 1, -1, -1) if bi == 0 else range(len(b["transactions"]) - 1, -1, -1)
        for i in rng:
            t = b["transactions"][i]
            if lib.all_keys(t)[0] == wsigner: continue
            im = impacts(t)
            if not im: continue
            if literal is None: literal = (s, i, t, im)
            (pk, pv), = im.items() if len(im) == 1 else [(None, 0)]
            if pk and abs(pv) >= IMPACT_BPS and pk in wim and pv * wim[pk] < 0:
                trig = (s, i, t, im); break
        if trig: break
    rule = f"latest earlier successful tx by another signer swapping exactly one of W/R with |impact| >= {IMPACT_BPS} bps, opposite in sign to the winner's move on that pool"
    if trig is None:
        trig = literal; rule = "fallback: latest earlier W/R swap by another signer"
    ts, ti, ttx, tim = trig
    # contender blocks: trigger slot after trigger, then each later block up to winner slot (<= winner idx)
    blocks = {s: b for s, b in seq_blocks}
    slots = []
    s = ts
    while True:
        b = blocks.get(s) or blk_of(s)
        if b is not None:
            blocks[s] = b; slots.append(s)
        if s >= slot: break
        s += 1
    contenders = []
    for s in slots:
        b = blocks[s]
        flags, signers = tf.block_tipflags(b)
        n = len(b["transactions"])
        for i, t in enumerate(b["transactions"]):
            if s == ts and i <= ti: continue
            if s == slot and s != ts and i > widx: break
            w = tf.writable(t)
            wr = sorted(POOL[k] for k in w & set(POOL))
            if not wr: continue
            f = tf.fields(t, i)
            f.update(slot=s, block_ntx=n, is_winner=(f["signature"] == sig), writes=wr,
                     wr_impact_bps={k: round(v, 2) for k, v in impacts(t).items()},
                     bundle_guess=tf.bundle_guess(b["transactions"], i, flags, signers))
            contenders.append(f)
    tflags, tsigners = tf.block_tipflags(blocks[ts])
    tfld = tf.fields(ttx, ti)
    trigger = dict(signature=tfld["signature"], slot=ts, index=ti, signer=tfld["signer"], fee=tfld["fee"],
                   tip_lamports=tfld["tip_lamports"], cu_price_micro=tfld["cu_price_micro"], cu_limit=tfld["cu_limit"],
                   pay_per_cu=tfld["pay_per_cu"], impact_bps={k: round(v, 2) for k, v in tim.items()},
                   bundle_guess=tf.bundle_guess(blocks[ts]["transactions"], ti, tflags, tsigners),
                   block_ntx=len(blocks[ts]["transactions"]), rule=rule)
    if literal and literal[2] is not ttx:
        lf = tf.fields(literal[2], literal[1])
        trigger["latest_other_wr_swap"] = dict(signature=lf["signature"], slot=literal[0], index=literal[1],
                                              signer=lf["signer"], impact_bps={k: round(v, 2) for k, v in literal[3].items()})
    wf = next(c for c in contenders if c["is_winner"])
    out = dict(id=f"{case['source']}:{slot}:{wsigner[:8]}", source=case["source"], slot=slot, winner_index=widx,
               winner_signer=wsigner, winner_signature=sig, block_ntx=len(txs),
               winner_slot_offset=slot - ts, winner_index_minus_trigger=(widx - ti) if slot == ts else None,
               explorer_profit_sol=case.get("profitAmount"),
               winner_gross=signer_gross(wtx, wf["tip_lamports"]), winner_pool_flow=pool_flow(wtx),
               trigger=trigger, contenders=contenders)
    nb_slot, nb = next_block(slot)
    out["next_block_slot"] = nb_slot
    return out

if __name__ == "__main__":
    cases = []
    for r in json.load(open("/tmp/arb-research/explorer/wr_arbs.json")):
        cases.append(dict(source="explorer_top200", slot=r["slot"], sig=r["txSignature"], profitAmount=r["profitAmount"]))
    seen = {c["sig"] for c in cases}
    for r in json.load(open("/tmp/arb-research/explorer/whale_cases.json")):
        if r["sig"] in seen: continue
        cases.append(dict(source=f"{r['source']}_first_WR_backrun", slot=r["slot"], sig=r["sig"]))
    res = []
    for c in sorted(cases, key=lambda c: c["slot"]):
        try:
            o = build(c); res.append(o)
            t = o["trigger"]
            print(o["id"], "idx", o["winner_index"], "trig", t["slot"], t["index"], t["signer"][:8], t["impact_bps"],
                  "ncont", len(o["contenders"]), flush=True)
        except Exception as e:
            print("FAIL", c, repr(e), flush=True)
    os.makedirs("/tmp/arb-research/validation", exist_ok=True)
    json.dump(res, open("/tmp/arb-research/validation/sol_usdc_cases.json", "w"), indent=1)
