"""Parse trimmed klend liquidation txs (collect.py output) into one record per liquidate ix.
usage: python3 -I parse_liq.py <scripts_dir> <out.jsonl> <liq_txs.jsonl.gz>...
"""
import sys, json, gzip, re, struct
sys.path.insert(0, sys.argv[1])
from b58 import b58decode

KLEND = "KLend2g3cP87fffoy8q1mQqGKjrxjC8boSyAYavgmjD"
SCOPE = "HFn8GnPADiny6XqUoWE8uRPPxb29ikn4yTuPa9MF2fWJ"
D_LIQ = {bytes.fromhex("a2a1238f1ebbb967"): "v2", bytes.fromhex("b1479abce2854a37"): "v1"}
D_FB = bytes.fromhex("87e734a70734d4c1")
D_FR = bytes.fromhex("b97500cb60f5b4ba")
D_ROB = bytes.fromhex("218493e497c04859")
CB = "ComputeBudget111111111111111111111111111111"
SYS = "11111111111111111111111111111111"
JITO_TIP = {
    "96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5", "HFqU5x63VTqvQss8hp11i4wVV8bD44PvwucfZ2bU7gRe",
    "Cw8CFyM9FkoMi7K7Crf6HNQqf4uEMzpKw6QNghXLvLkY", "ADaUMid9yfUytqMBgopwjb2DTLSokTSzL1zt6iGPaS49",
    "DfXygSm4jCyNCybVYYK6DwvWqjKee8pbDmJGcLWNDXjh", "ADuUkR4vqLUMWXxW9gh6D6L8pMSawimctcNZ5pGwDcEt",
    "DttWaMuVvTiduZRnguLF7jNxTgiMBZ1hyAumKUiL2KRL", "3AVi9Tg9Uo68tJfuvoKvqKNWKkC5wPdSSdeBnizKZ6jT",
}
INFRA = {KLEND, SCOPE, CB, SYS, "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA",
         "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb", "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL",
         "FarmsPZpWu9i7Kky8tPN37rs2TpmMrAZrC7S7vJa91Hr", "MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr",
         "Memo1UhkJRfHyvLMcVucJwxXeuD728EqVDDwQDxFMNo", "Sysvar1nstructions1111111111111111111111111",
         "L2TExMFKdjpN9kozasaurPirfHy9P8sbXoAN1qA3S95", "AddressLookupTab1e1111111111111111111111111"}
DEX = {
    "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc": "OrcaWhirlpool",
    "CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK": "RaydiumCLMM",
    "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8": "RaydiumAMMv4",
    "CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C": "RaydiumCPMM",
    "LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo": "MeteoraDLMM",
    "cpamdpZCGKUy5JxQXB4dcpGPiikHawvSWAd6mEn1sGG": "MeteoraDAMMv2",
    "Eo7WjKq67rjJQSZxS6z3YkapzY3eMj6Xy8X5EQVn5UaB": "MeteoraDAMMv1",
    "JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4": "JupiterV6",
    "SoLFiHG9TfgtdUXUjWAxi3LtvYuFyDLVhBWxdMZxyCe": "SolFi",
    "SV2EYYJyRz2YhfXwXnhNAevDEui5Q6yrfyo13WtupPF": "SolFiV2",
    "pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA": "PumpSwap",
    "obriQD1zbpyLz95G5n7nJe6a4DPjpFwa5XYPoNm113y": "Obric",
    "ZERor4xhbUycZ6gb9ntrhqscUcZmAbQDjEAtCf4hbZY": "ZeroFi",
    "9H6tua7jkLhdm3w8BvgpTn5LZNU7g4ZynDmCiNN3q6Rp": "HumidiFi",
    "TessVdML9pBGgG9yGks7o4HewRaXVAMuoVj4x83GLQH": "TesseraV",
    "goonERTdGsjnkZqWuVjs73BZ3Pb9qoCUdBUL17BnS5j": "GoonFi",
    "2wT8Yq49kHgDzXuPxZSaeLaH1qbmGXtEyPy64bL7aD3c": "LifinityV2",
    "stab1io8dHvK26KoHmTwwHyYmHRbUWbyEJx6CdrGabC": "StabbleStable",
    "swapNyd8XiQwJ6ianp9snpu4brUqFxadzvHebnAXjJZ": "StabbleWeighted",
    "BiSoNHVpsVZW2F7rx2eQ59yQwKxzU5NvBcmKshCSUypi": "BisonFi",
    "PhoeNiXZ8ByJGLkxNfZRnkUfjvmuYqLR89jjFHGqdXY": "Phoenix",
    "opnb2LAfJYbRMAHHvqjCwQxanZn7ReEHp1k81EohpZb": "OpenBookV2",
    "SSwpkEEcbUqx4vtoEByFjSkhKdCT862DNVb52nZg1UZ": "Saber",
    "DjVE6JNiYqPL2QXyCUUh8rNjHrbz9hXHNYt99MQ59qw1": "OrcaV1",
    "9W959DqEETiGZocYWCQPaJ6sBmUzgfxXfqGeTEdp3aQP": "OrcaV2",
    "5uJgCDrQHfA58fPqLsuU14Srg9quxXNHz91cZ54cq4pK": "VaultStakeRedeem",
    "jupr81YtYssSyPt8jbnGuiWon5f6x9TcDEFxYe3Bdzi": "JupiterLendBorrow",
    "jupgfSgfuAXv4B6R2Uxu85Z1qdzgju79s6MfZekN6XS": "JupiterLendFlash",
    "MNFSTqtC93rEfYHB6hF82sKdZpUDFWkViLByLd1k1Ms": "Manifest",
    "DEXYosS6oEGvk8uCDayvwEZz4qEyDJRf9nFgYCaqPMTm": "1DEX",
    "HpNfyc2Saw7RKkQd8nEL4khUcuPhQ7WwY1B2qjx8jxFq": "PancakeSwapV3",
    "dbcij3LWUppWqq96dh6gJWwBifmcGfLSB5D4DuSMaqN": "MeteoraDBC",
    "AlphaQ?": "AlphaQ",
}

def u64(b, o): return struct.unpack_from("<Q", b, o)[0]

def flat_ixs(t):
    """(top_index, inner_pos or None, stack_height, ix) in execution order."""
    inner = {g["index"]: g["instructions"] for g in (t.get("inner") or [])}
    for i, ix in enumerate(t["ixs"]):
        yield i, None, 1, ix
        for j, iix in enumerate(inner.get(i, [])):
            yield i, j, iix.get("stackHeight") or 2, iix

def klend_segments(logs):
    """Split logs into klend invocations: list of (name, depth, [lines]) in order."""
    segs, stack = [], []
    for l in logs:
        m = re.match(r"Program (\w+) invoke \[(\d+)\]", l)
        if m:
            stack.append((m.group(1), int(m.group(2)), None))
            if m.group(1) == KLEND:
                segs.append({"name": None, "depth": int(m.group(2)), "lines": [], "ok": None})
                stack[-1] = (KLEND, int(m.group(2)), len(segs) - 1)
            continue
        m = re.match(r"Program (\w+) (success|failed)", l)
        if m and stack:
            prog, d, si = stack.pop()
            if si is not None:
                segs[si]["ok"] = m.group(2) == "success"
                if m.group(2) == "failed":
                    segs[si]["fail"] = l
            continue
        # attribute line to innermost klend segment if innermost program is klend
        if stack and stack[-1][2] is not None:
            seg = segs[stack[-1][2]]
            if l.startswith("Program log: Instruction: ") and seg["name"] is None:
                seg["name"] = l[len("Program log: Instruction: "):]
            seg["lines"].append(l)
    return segs

RE = {
    "args": re.compile(r"LiquidateObligationAndRedeemReserveCollateral amount (\d+) max_allowed_ltv_override_percent (\d+)"),
    "cf": re.compile(r"liquidation_close_factor_pct: (\d+), liquidation_max_value: (\d+)"),
    "reason": re.compile(r"eligible for liquidation because of (\w+)(?:\(([^)]*)\))? with liquidation bonus: (\d+)\s*bps"),
    "elig": re.compile(r"borrowed value \(scaled\): ([\d.]+), unhealthy borrow value \(scaled\): ([\d.]+), LTV: (\d+)%/(\d+)%"),
    "liqd": re.compile(r"liquidated with liquidation bonus: (\d+) bps, liquidation amount \(rounded\): ([\d.]+)"),
    "pnl": re.compile(r"pnl: Liquidator repaid (\d+) and withdrew (\d+) collateral with fees (\d+)"),
    "healthy": re.compile(r"healthy and cannot be liquidated, LTV: ([\d.]+)"),
    "anchor": re.compile(r"Error Code: (\w+)"),
    "borrow": re.compile(r"Borrow: (.+?) amount: ([\d.]+) value: ([\d.]+) value_bf: ([\d.]+)"),
    "deposit": re.compile(r"Deposit: (.+?) amount: ([\d.]+) value: ([\d.]+)"),
    "price": re.compile(r"Token: (.+?) Price: ([\d.]+)"),
    "scope": re.compile(r"tk (\d+), (\w+): (\d+) to (\d+) \| prev_slot: (\d+), new_slot: (\d+), crt_slot: (\d+)"),
}

def parse(t):
    keys = t["keys"]; logs = t["logs"] or []
    nsig = t["header"]["numRequiredSignatures"]
    signer = keys[0]
    # compute budget, tips
    cu_limit = cu_price = None
    tips = {"jito": 0, "other": []}
    progs = set()
    liqs, fbs, frs, robs = [], [], [], []
    for ti, ij, h, ix in flat_ixs(t):
        p = keys[ix["programIdIndex"]]
        progs.add(p)
        data = b58decode(ix["data"]) if ix.get("data") else b""
        acc = [keys[a] for a in ix["accounts"]]
        if p == CB and ij is None and data:
            if data[0] == 2: cu_limit = struct.unpack_from("<I", data, 1)[0]
            if data[0] == 3: cu_price = u64(data, 1)
        if p == SYS and len(data) >= 12 and data[0] == 2 and len(acc) >= 2:
            lam = u64(data, 4)
            if acc[1] in JITO_TIP: tips["jito"] += lam
            elif acc[0] == signer and ij is None: tips["other"].append([acc[1], lam])
        if p == KLEND and len(data) >= 8:
            d = data[:8]
            pos = {"top": ti, "inner": ij, "h": h}
            if d in D_LIQ:
                liqs.append(dict(pos, ver=D_LIQ[d], obligation=acc[1], market=acc[2], repay_reserve=acc[4],
                                 repay_mint=acc[5], withdraw_reserve=acc[7], withdraw_mint=acc[8],
                                 withdraw_cmint=acc[9], fee_receiver=acc[12], user_src=acc[13],
                                 user_dst_liq=acc[15],
                                 arg_amount=u64(data, 8), arg_min_out=u64(data, 16),
                                 arg_ltv_override=u64(data, 24) if len(data) >= 32 else None))
            elif d == D_FB:
                fbs.append(dict(pos, reserve=acc[3], mint=acc[4], amount=u64(data, 8)))
            elif d == D_FR:
                frs.append(dict(pos, reserve=acc[3], mint=acc[4], amount=u64(data, 8)))
            elif d == D_ROB:
                robs.append(dict(pos, obligation=acc[1], reserves=acc[2:]))
    segs = klend_segments(logs)
    liq_segs = [s for s in segs if s["name"] and s["name"].startswith("LiquidateObligation")]
    # refresh-obligation and refresh-reserve logs, scope logs (whole tx)
    prices, positions = {}, {}
    rob_segs = [s for s in segs if s["name"] == "RefreshObligation"]
    for s in segs:
        for l in s["lines"]:
            m = RE["price"].search(l)
            if m: prices[m.group(1)] = float(m.group(2))
    scope = [list(map(lambda x: int(x) if x.isdigit() else x, m.groups())) for l in logs for m in [RE["scope"].search(l)] if m]
    out = []
    for k, L in enumerate(liqs):
        r = {"sig": t["sig"], "slot": t["slot"], "idx": t["idx"], "t": t["blockTime"], "tx_ok": t["err"] is None,
             "tx_err": t["err"], "signer": signer, "fee": t["fee"], "cu": t["cu"], "nsig": nsig,
             "cu_limit": cu_limit, "cu_price": cu_price, "jito_tip": tips["jito"], "other_transfers": tips["other"],
             "n_liq_in_tx": len(liqs), "k": k}
        r.update(L)
        # the refresh_obligation for this obligation, last one before this liquidation
        rob_i = [i for i, ro in enumerate(robs) if ro["obligation"] == L["obligation"] and (ro["top"], ro["inner"] or -1) < (L["top"], L["inner"] or -1)]
        seg = liq_segs[k] if k < len(liq_segs) else None
        r["executed"] = seg is not None
        if rob_i:
            ro = robs[rob_i[-1]]
            r["refresh_reserves"] = ro["reserves"]
            # matching log segment: index of this RefreshObligation among all RefreshObligation ixs
            if rob_i[-1] < len(rob_segs):
                lines = rob_segs[rob_i[-1]]["lines"]
                r["borrows"] = [[m.group(1), float(m.group(2)), float(m.group(3)), float(m.group(4))] for l in lines for m in [RE["borrow"].search(l)] if m]
                r["deposits"] = [[m.group(1), float(m.group(2)), float(m.group(3))] for l in lines for m in [RE["deposit"].search(l)] if m]
        if seg:
            r["seg_ok"] = seg["ok"]
            for l in seg["lines"]:
                for name, rx in RE.items():
                    if name in ("borrow", "deposit", "price", "scope"): continue
                    m = rx.search(l)
                    if m: r.setdefault("log_" + name, list(m.groups()))
            if not seg["ok"]:
                r["fail_line"] = seg.get("fail")
        else:
            m = None
            for l in logs:
                m2 = RE["anchor"].search(l)
                if m2: m = m2
            if m: r["log_anchor"] = list(m.groups())
        r["flash_borrows"] = fbs; r["flash_repays"] = frs
        r["prices"] = prices; r["scope"] = scope
        r["programs"] = sorted(progs - INFRA)
        r["dex"] = sorted({DEX[p] for p in progs if p in DEX})
        r["outer_program"] = keys[t["ixs"][L["top"]]["programIdIndex"]]
        # signer balance deltas
        try:
            si = keys.index(signer)
            r["sol_delta"] = t["post"][si] - t["pre"][si]
        except Exception:
            r["sol_delta"] = None
        deltas = {}
        for side, arr in (("pre", t["preTok"] or []), ("post", t["postTok"] or [])):
            for b in arr:
                if b.get("owner") != signer: continue
                amt = int(b["uiTokenAmount"]["amount"])
                deltas.setdefault(b["mint"], [0, 0, b["uiTokenAmount"]["decimals"]])
                deltas[b["mint"]][0 if side == "pre" else 1] += amt
        r["tok_delta"] = {m: [v[1] - v[0], v[2]] for m, v in deltas.items() if v[1] != v[0]}
        out.append(r)
    return out

if __name__ == "__main__":
    n = 0
    seen = set()
    with open(sys.argv[2], "w") as fo:
        for path in sys.argv[3:]:
            f = gzip.open(path, "rt")
            try:
                for line in f:
                    try:
                        t = json.loads(line)
                    except json.JSONDecodeError:
                        break
                    if t["sig"] in seen: continue
                    seen.add(t["sig"])
                    for r in parse(t):
                        fo.write(json.dumps(r) + "\n"); n += 1
            except EOFError:
                pass
    print("records", n)
