"""Extract swap legs (pool, mint in/out, amounts, pre/post marginal price) from a jsonParsed transaction.

A leg is one invocation of a program that directly performs both a transfer from the trader's
token accounts and a transfer to them, with different mints. Programs in TRANSPARENT (Meteora
vault under DAMM v1) are climbed through. Per-venue events give the pre-trade price where cheap:
Whirlpool Traded (pre/post sqrt price), Raydium/Pancake/Byreal CLMM SwapEvent (post sqrt price),
Meteora DLMM Swap (start/end bin), Raydium AMM v4 ray_log (reserves before).
"""
import base64, hashlib, json, struct, sys

TOKEN = {"TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA", "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb"}
SKIP = TOKEN | {"11111111111111111111111111111111", "ComputeBudget111111111111111111111111111111",
                "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL", "Memo1UhkJRfHyvLMcVucJwxXeuD728EqVDDwQDxFMNo",
                "MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr"}
TRANSPARENT = {"24Uqj9JCLxUeoC3hGfh5W3s9FM9uCHDS2SG3LYwBpyTi"}  # Meteora dynamic vault (DAMM v1 internals)
WSOL = "So11111111111111111111111111111111111111112"

VENUE = {
    "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc": "OrcaWhirlpool",
    "CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK": "RaydiumCLMM",
    "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8": "RaydiumAMMv4",
    "CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C": "RaydiumCPMM",
    "LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo": "MeteoraDLMM",
    "Eo7WjKq67rjJQSZxS6z3YkapzY3eMj6Xy8X5EQVn5UaB": "MeteoraDAMMv1",
    "cpamdpZCGKUy5JxQXB4dcpGPiikHawvSWAd6mEn1sGG": "MeteoraDAMMv2",
    "HpNfyc2Saw7RKkQd8nEL4khUcuPhQ7WwY1B2qjx8jxFq": "PancakeCLMM",
    "REALQqNEomY6cQGZJUGwywTBD2UmDT32rZcNnfxQ5N2": "ByrealCLMM",
    "9W959DqEETiGZocYWCQPaJ6sBmUzgfxXfqGeTEdp3aQP": "OrcaV2",
    "SSwpkEEcbUqx4vtoEByFjSkhKdCT862DNVb52nZg1UZ": "Saber",
    "pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA": "PumpAMM",
    "BiSoNHVpsVZW2F7rx2eQ59yQwKxzU5NvBcmKshCSUypi": "BisonFi",
    "SV2EYYJyRz2YhfXwXnhNAevDEui5Q6yrfyo13WtupPF": "SolFiV2",
    "SoLFiHG9TfgtdUXUjWAxi3LtvYuFyDLVhBWxdMZxyCe": "SolFi",
    "ZERor4xhbUycZ6gb9ntrhqscUcZmAbQDjEAtCf4hbZY": "ZeroFi",
    "goonuddtQRrWqqn5nFyczVKaie28f3kDkHWkHtURSLE": "GoonFi",
    "9H6tua7jkLhdm3w8BvgpTn5LZNU7g4ZynDmCiNN3q6Rp": "HumidiFi",
    "TessVdML9pBGgG9yGks7o4HewRaXVAMuoVj4x83GLQH": "Tessera",
    "obriQD1zbpyLz95G5n7nJe6a4DPjpFwa5XYPoNm113y": "Obric",
    "DEXYosS6oEGvk8uCDayvwEZz4qEyDJRf9nFgYCaqPMTm": "1DEX",
    "PERPHjGBqRHArX4DySjwM6UJHiR3sWAatqfdBS2qQJu": "JupPerps",
    "5ocnV1qiCgaQR8Jb8xWnVbApfaygJ8tNoZfgPwsgx9kx": "SanctumInfinity",
    "SPoo1Ku8WFXoNDMHPsrGSTSG1Y47rzgn41SLUNakuHy": "SplStakePool",
    "SP12tWFxD9oJsVWNavTTBZvMbA6gkAmxtVgxdqvyvhY": "SanctumSplStakePool",
    "SPMBzsVUuoHA4Jm6KunbsotaahvVikZs1JyTW6iJvbn": "SanctumSplMultiStakePool",
    "MarBmsSgKXdrN1egZf5sqe1TMai9K1rChYNDJgjq7aD": "Marinade",
    "HYEXCHtHkBagdStcJCp3xbbb9B7sdMdWXFNj6mdsG4hn": "HyloExchange",
    "stkitrT1Uoy18Dk1fTrgPw8W6MVzoCfYoAFT4MLsmhq": "SanctumRouter",
}
# index of the pool account in the swap instruction, per program (first matching ix layout)
POOL_IX = {"OrcaWhirlpool": {8: 2, 17: 4}, "RaydiumCLMM": 2, "PancakeCLMM": 2, "ByrealCLMM": 2,
           "RaydiumAMMv4": 1, "RaydiumCPMM": 3, "MeteoraDLMM": 0, "MeteoraDAMMv1": 0,
           "MeteoraDAMMv2": 1, "OrcaV2": 0, "Saber": 0, "PumpAMM": 0}

def disc(name):
    return hashlib.sha256(f"event:{name}".encode()).digest()[:8]
D_TRADED = disc("Traded")
D_CLMM_SWAP = disc("SwapEvent")
D_DLMM_SWAP = disc("Swap")
EVENT_IX_TAG = bytes.fromhex("e445a52e51cb9a1d")

B58 = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz"
def b58d(s):
    n = 0
    for ch in s: n = n * 58 + B58.index(ch)
    b = n.to_bytes((n.bit_length() + 7) // 8, "big")
    return b"\0" * (len(s) - len(s.lstrip("1"))) + b
def b58e(b):
    n = int.from_bytes(b, "big"); s = ""
    while n: n, r = divmod(n, 58); s = B58[r] + s
    return "1" * (len(b) - len(b.lstrip(b"\0"))) + s

def flat_instructions(tx):
    """Instructions in execution order: (stackHeight, ix)."""
    top = tx["transaction"]["message"]["instructions"]
    inner = {g["index"]: g["instructions"] for g in tx["meta"].get("innerInstructions") or []}
    out = []
    for i, ix in enumerate(top):
        out.append((1, ix))
        for j in inner.get(i, []):
            out.append((j.get("stackHeight") or 2, j))
    return out

def log_invocations(logs):
    """Per invocation in log order: (program, [Program data payloads])."""
    inv, stack = [], []
    for l in logs or []:
        if l.startswith("Program ") and " invoke [" in l:
            inv.append([l.split()[1], []]); stack.append(len(inv) - 1)
        elif l.startswith("Program data: ") and stack:
            inv[stack[-1]][1].append(l[len("Program data: "):])
        elif l.startswith("Program log: ray_log: ") and stack:
            inv[stack[-1]][1].append("RAY:" + l[len("Program log: ray_log: "):])
        elif l.startswith("Program ") and (l.endswith(" success") or " failed" in l) and stack:
            stack.pop()
    return inv

def keys_of(tx):
    ks = [k["pubkey"] if isinstance(k, dict) else k for k in tx["transaction"]["message"]["accountKeys"]]
    la = tx["meta"].get("loadedAddresses")
    if la: ks += la.get("writable", []) + la.get("readonly", [])
    return ks

def token_info(tx):
    ks = keys_of(tx); info = {}
    for side in ("preTokenBalances", "postTokenBalances"):
        for b in tx["meta"].get(side) or []:
            k = ks[b["accountIndex"]]
            d = info.setdefault(k, {"mint": b["mint"], "owner": b.get("owner"), "dec": b["uiTokenAmount"]["decimals"]})
            d[side[:3]] = int(b["uiTokenAmount"]["amount"])
    return info

def sqrt_to_price(sq, dec_a, dec_b):
    return (sq / 2**64) ** 2 * 10 ** (dec_a - dec_b)

def legs(tx):
    meta = tx["meta"]; ks = keys_of(tx); signer = ks[0]
    ti = token_info(tx)
    flat = flat_instructions(tx)
    inv = log_invocations(meta.get("logMessages"))
    # attach log index to each flat ix (they are in the same order)
    for n, (h, ix) in enumerate(flat):
        ix["_logidx"] = n if n < len(inv) and inv[n][0] == ix["programId"] else None
    transfers = []  # (flat idx, parent flat idx, src, dst, amount, mint, authority)
    parents = []
    for n, (h, ix) in enumerate(flat):
        while parents and parents[-1][0] >= h: parents.pop()
        if ix["programId"] in TOKEN and isinstance(ix.get("parsed"), dict) and ix["parsed"].get("type") in ("transfer", "transferChecked"):
            inf = ix["parsed"]["info"]
            amt = int(inf.get("amount") or inf["tokenAmount"]["amount"])
            mint = inf.get("mint") or (ti.get(inf["source"]) or ti.get(inf["destination"]) or {}).get("mint")
            p = None
            for ph, pn in reversed(parents):
                if flat[pn][1]["programId"] not in TRANSPARENT: p = pn; break
            transfers.append((n, p, inf["source"], inf["destination"], amt, mint, inf.get("authority") or inf.get("multisigAuthority")))
        parents.append((h, n))
    # trader-owned accounts
    U = {signer}
    for t in transfers:
        if t[6] == signer and t[2] in ti: U.add(ti[t[2]]["owner"])
    def owner(acct): return (ti.get(acct) or {}).get("owner")
    # bots using a PDA: an owner whose accounts take part in two or more distinct invocations
    seen = {}
    for t in transfers:
        if t[1] is None: continue
        for o in (owner(t[2]) or t[6], owner(t[3])):
            if o: seen.setdefault(o, set()).add(t[1])
    U |= {o for o, ps in seen.items() if len(ps) >= 2}
    groups = {}
    for t in transfers: groups.setdefault(t[1], []).append(t)
    out = []
    for p, ts in groups.items():
        if p is None: continue
        ins = [t for t in ts if owner(t[2]) in U or t[6] in U]
        outs = [t for t in ts if owner(t[3]) in U and not (owner(t[2]) in U)]
        if not ins or not outs:
            # fallback for single swaps by a PDA-owned account: first transfer in, first other-mint transfer out
            if len(ts) >= 2 and ts[0][5] != ts[-1][5]: ins, outs = [ts[0]], [t for t in ts[1:] if t[5] != ts[0][5]][:1]
            else: continue
        ti_, to_ = ins[0], outs[-1]
        if ti_[5] == to_[5]: continue
        ix = flat[p][1]; prog = ix["programId"]; venue = VENUE.get(prog, prog[:8])
        accts = ix.get("accounts") or []
        pool = None
        m = POOL_IX.get(venue)
        if isinstance(m, dict):  # whirlpool swap (pool at 2) vs swapV2 (memo program at 2, pool at 4)
            m = 4 if len(accts) > 2 and accts[2].startswith("Memo") else 2
        if m is not None and m < len(accts): pool = accts[m]
        if pool is None: pool = to_[6]  # outflow authority (pool or shared authority)
        din = (ti.get(ti_[2]) or ti.get(ti_[3]) or {}).get("dec"); dout = (ti.get(to_[3]) or ti.get(to_[2]) or {}).get("dec")
        leg = {"i": p, "venue": venue, "program": prog, "pool": pool, "mint_in": ti_[5], "mint_out": to_[5],
               "amt_in": ti_[4], "amt_out": to_[4], "dec_in": din, "dec_out": dout,
               "vault_in": ti_[3], "vault_out": to_[2]}
        if din is not None and dout is not None:
            leg["exec_px"] = (to_[4] / 10**dout) / (ti_[4] / 10**din)  # out per in (UI units)
        # events
        datas = []
        li = ix.get("_logidx")
        if li is not None: datas = inv[li][1]
        # anchor event-cpi children (DLMM): inner ix to same program with EVENT_IX_TAG
        evcpi = []
        for q in range(p + 1, len(flat)):
            if flat[q][0] <= flat[p][0]: break
            if flat[q][1]["programId"] == prog and "data" in flat[q][1]:
                try:
                    raw = b58d(flat[q][1]["data"])
                    if raw[:8] == EVENT_IX_TAG: evcpi.append(raw[8:])
                except Exception: pass
        decode_events(leg, venue, datas, evcpi)
        # vault balances before the tx (spot for constant-product pools if pool not reused)
        for side, acct in (("in", leg["vault_in"]), ("out", leg["vault_out"])):
            if acct in ti and "pre" in ti[acct]: leg[f"vault_{side}_pre"] = ti[acct]["pre"]
        out.append(leg)
    out.sort(key=lambda l: l["i"])
    return out, signer

def decode_events(leg, venue, datas, evcpi):
    din, dout = leg.get("dec_in"), leg.get("dec_out")
    for d in datas:
        if d.startswith("RAY:"):
            raw = base64.b64decode(d[4:])
            if raw[0] == 3 and len(raw) >= 57:  # SwapBaseIn
                amount_in, min_out, direction, user_src, pool_coin, pool_pc, out_amt = struct.unpack_from("<7Q", raw, 1)
                leg["ray"] = {"dir": direction, "pool_coin": pool_coin, "pool_pc": pool_pc}
            continue
        try: raw = base64.b64decode(d)
        except Exception: continue
        if raw[:8] == D_TRADED and venue == "OrcaWhirlpool" and len(raw) >= 8 + 32 + 1 + 32:
            a_to_b = raw[40]
            pre = int.from_bytes(raw[41:57], "little"); post = int.from_bytes(raw[57:73], "little")
            inp, outp, inf, outf, lpf, pf = struct.unpack_from("<6Q", raw, 73)
            # price of token A in B; in-token is A when a_to_b
            da, db = (din, dout) if a_to_b else (dout, din)
            pa, pb = sqrt_to_price(pre, da, db), sqrt_to_price(post, da, db)
            # express as out per in (same orientation as exec_px)
            leg["pre_px"] = pa if a_to_b else 1 / pa
            leg["post_px"] = pb if a_to_b else 1 / pb
            leg["fee_paid_in"] = lpf + pf
        elif raw[:8] == D_CLMM_SWAP and venue in ("RaydiumCLMM", "PancakeCLMM", "ByrealCLMM") and len(raw) >= 8 + 128 + 32 + 1 + 16:
            o = 8 + 128
            a0, f0, a1, f1 = struct.unpack_from("<4Q", raw, o); o += 32
            z41 = raw[o]; o += 1
            sq = int.from_bytes(raw[o:o + 16], "little")
            liq = int.from_bytes(raw[o + 16:o + 32], "little")
            leg["clmm"] = {"liquidity": liq, "sqrt_post": sq, "zero_for_one": z41}
            d0, d1 = (din, dout) if z41 else (dout, din)
            p = sqrt_to_price(sq, d0, d1)
            leg["post_px"] = p if z41 else 1 / p
    for raw in evcpi:
        if raw[:8] == D_DLMM_SWAP and venue == "MeteoraDLMM" and len(raw) >= 8 + 64 + 8:
            sb, eb = struct.unpack_from("<2i", raw, 72)
            amt_in, amt_out = struct.unpack_from("<2Q", raw, 80)
            swap_for_y = raw[96]
            fee, pfee = struct.unpack_from("<2Q", raw, 97)
            leg["dlmm"] = {"start_bin": sb, "end_bin": eb, "swap_for_y": swap_for_y, "fee": fee}

if __name__ == "__main__":
    tx = json.load(open(sys.argv[1])); tx = tx.get("result", tx)
    ls, s = legs(tx)
    print(s); [print(json.dumps(l)) for l in ls]
