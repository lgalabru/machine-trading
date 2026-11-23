"""Per-transaction fee / CU / tip fields from a json-encoded tx (getBlock full or getTransaction)."""
import math, sys, os
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import lib

CB = "ComputeBudget111111111111111111111111111111"
SYS = "11111111111111111111111111111111"
JITO_TIP = {"96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5", "HFqU5x63VTqvQss8hp11i4wVV8bD44PvwucfZ2bU7gRe",
    "Cw8CFyM9FkoMi7K7Crf6HNQqf4uEMzpKw6QNghXLvLkY", "ADaUMid9yfUytqMBgopwjb2DTLSokTSzL1zt6iGPaS49",
    "DfXygSm4jCyNCybVYYK6DwvWqjKee8pbDmJGcLWNDXjh", "ADuUkR4vqLUMWXxW9gh6D6L8pMSawimctcNZ5pGwDcEt",
    "DttWaMuVvTiduZRnguLF7jNxTgiMBZ1hyAumKUiL2KRL", "3AVi9Tg9Uo68tJfuvoKvqKNWKkC5wPdSSdeBnizKZ6jT"}

def keys(tx): return lib.all_keys(tx)

def writable(tx):
    m = tx["transaction"]["message"]; h = m["header"]; st = m["accountKeys"]
    n = len(st); ns = h["numRequiredSignatures"]
    w = set()
    for i, k in enumerate(st):
        if i < ns - h["numReadonlySignedAccounts"] or (ns <= i < n - h["numReadonlyUnsignedAccounts"]):
            w.add(k)
    la = tx["meta"].get("loadedAddresses") or {}
    w |= set(la.get("writable", []))
    return w

def _sys_transfer(data_b58):
    d = lib.b58d(data_b58) if data_b58 else b""
    if len(d) >= 12 and int.from_bytes(d[:4], "little") == 2:
        return int.from_bytes(d[4:12], "little")
    return None

def fields(tx, idx=None):
    m = tx["transaction"]["message"]; meta = tx["meta"]; ks = keys(tx)
    nsig = len(tx["transaction"]["signatures"])
    limit = price = None; nonbudget = 0
    tip_top = 0
    for ix in m["instructions"]:
        pid = ks[ix["programIdIndex"]]
        if pid == CB:
            d = lib.b58d(ix["data"])
            if d and d[0] == 2 and len(d) >= 5: limit = int.from_bytes(d[1:5], "little")
            elif d and d[0] == 3 and len(d) >= 9: price = int.from_bytes(d[1:9], "little")
        else:
            nonbudget += 1
        if pid == SYS:
            lam = _sys_transfer(ix["data"])
            if lam is not None and len(ix["accounts"]) >= 2 and ks[ix["accounts"][1]] in JITO_TIP:
                tip_top += lam
    tip_inner = 0
    for g in meta.get("innerInstructions") or []:
        for ix in g["instructions"]:
            if ks[ix["programIdIndex"]] == SYS:
                lam = _sys_transfer(ix["data"])
                if lam is not None and len(ix["accounts"]) >= 2 and ks[ix["accounts"][1]] in JITO_TIP:
                    tip_inner += lam
    cfg = m.get("transactionConfig")
    if cfg is not None:  # v1 transaction: budget lives in the message config, priority fee in lamports
        limit_src = "v1_config" if cfg.get("computeUnitLimit") is not None else "default"
        limit = cfg.get("computeUnitLimit")
        if limit is None: limit = min(200_000 * nonbudget, 1_400_000)
        prio = int(cfg.get("priorityFee") or 0)
        price = prio * 1e6 / limit if limit else 0
    else:
        limit_src = "ix" if limit is not None else "default"
        if limit is None: limit = min(200_000 * nonbudget, 1_400_000)
        price = price or 0
        prio = math.ceil(price * limit / 1e6)
    tip = tip_top + tip_inner
    # tip accounts referenced at all (e.g. a CPI tip in a failed tx that never ran)
    tip_acct_ref = any(k in JITO_TIP for k in ks)
    base = 5000 * nsig
    err = meta.get("err")
    return dict(
        signature=tx["transaction"]["signatures"][0], signer=ks[0], index=idx, tx_version=tx.get("version"),
        succeeded=err is None, error=err,
        cu_limit=limit, cu_limit_source=limit_src, cu_price_micro=price, cu_consumed=meta.get("computeUnitsConsumed"),
        fee=meta.get("fee"), n_signatures=nsig, base_fee=base, priority_lamports=prio,
        fee_check=(meta.get("fee") == base + prio),
        tip_lamports=tip, tip_top_level=tip_top, tip_cpi=tip_inner, refs_tip_account=tip_acct_ref,
        pay_per_cu=(prio + tip) * 1e6 / limit if limit else None,
        pay_per_cu_incl_base=(prio + tip + base) * 1e6 / limit if limit else None,
    )

def bundle_guess(block_txs, i, tipflags, signers):
    """tips itself -> 'tips'; a tx within 4 positions with the same signer tips -> 'same_signer_neighbor_tips';
    the tx immediately before or after tips -> 'adjacent_to_tipper'; else ''"""
    if tipflags[i]: return "tips"
    n = len(block_txs)
    for d in range(1, 5):
        for j in (i - d, i + d):
            if 0 <= j < n and tipflags[j] and signers[j] == signers[i]: return "same_signer_neighbor_tips"
    for j in (i - 1, i + 1):
        if 0 <= j < n and tipflags[j]: return "adjacent_to_tipper"
    return ""

def block_tipflags(blk):
    fl = []; sg = []
    for t in blk["transactions"]:
        ks = keys(t); sg.append(ks[0])
        tipped = False
        if any(k in JITO_TIP for k in ks):
            f = fields(t); tipped = f["tip_lamports"] > 0
            if not tipped and t["meta"]["err"] is None:
                pre, post = t["meta"]["preBalances"], t["meta"]["postBalances"]
                tipped = any(ks[j] in JITO_TIP and post[j] > pre[j] for j in range(len(post)))
        fl.append(tipped)
    return fl, sg
