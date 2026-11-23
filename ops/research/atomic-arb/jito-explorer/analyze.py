import json, sys, os, collections
d = sys.argv[1]; which = sys.argv[2]
rows = json.load(open(os.path.join(d, which)))
WSOL="So11111111111111111111111111111111111111112"
DEX = {
"675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8":"RaydiumAMMv4","CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK":"RaydiumCLMM",
"CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C":"RaydiumCPMM","LanMV9sAd7wArD4vJFi2qDdfnVhFxYSUg6eADduJ3uj":"RaydiumLaunchpad",
"whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc":"OrcaWhirlpool","LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo":"MeteoraDLMM",
"Eo7WjKq67rjJQSZxS6z3YkapzY3eMj6Xy8X5EQVn5UaB":"MeteoraDAMMv1","cpamdpZCGKUy5JxQXB4dcpGPiikHawvSWAd6mEn1sGG":"MeteoraDAMMv2",
"dbcij3LWUppWqq96dh6gJWwBifmcGfLSB5D4DuSMaqN":"MeteoraDBC","pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA":"PumpAMM",
"6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P":"PumpFun","PhoeNiXZ8ByJGLkxNfZRnkUfjvmuYqLR89jjFHGqdXY":"PhoenixV1",
"opnb2LAfJYbRMAHHvqjCwQxanZn7ReEHp1k81EohpZb":"OpenBookV2","2wT8Yq49kHgDzXuPxZSaeLaH1qbmGXtEyPy64bL7aD3c":"LifinityV2",
"SoLFiHG9TfgtdUXUjWAxi3LtvYuFyDLVhBWxdMZxyCe":"SolFi","SV2EYYJyRz2YhfXwXnhNAevDEui5Q6yrfyo13WtupPF":"SolFiV2","obriQD1zbpyLz95G5n7nJe6a4DPjpFwa5XYPoNm113y":"ObricV2",
"ZERor4xhbUycZ6gb9ntrhqscUcZmAbQDjEAtCf4hbZY":"ZeroFi","9H6tua7jkLhdm3w8BvgpTn5LZNU7g4ZynDmCiNN3q6Rp":"HumidiFi",
"TessVdML9pBGgG9yGks7o4HewRaXVAMuoVj4x83GLQH":"TesseraV","goonERTdGsjnkZqWuVjs73BZ3Pb9qoCUdBUL17BnS5j":"GoonFi",
"swapNyd8XiQwJ6ianp9snpu4brUqFxadzvHebnAXjJZ":"StabbleStable","swapFpHZwjELNnjvThjajtiVmkz3yPQEHjLtka2fwHW":"StabbleWeighted",
"JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4":"JupiterV6","MNFSTqtC93rEfYHB6hF82sKdZpUDFWkViLDLJBhmUDV":"Manifest",
"SSwpkEEcbUqx4vtoEByFjSkhKdCT862DNVb52nZg1UZ":"Saber","5ocnV1qiCgaQR8Jb8xWnVbApfaygJ8tNoZfgPwsgx9kx":"SanctumInfinity",
"REALQqNEomY6cQGZJUGwywTBD2UmDT32rZcNnfxQ5N2":"ByrealCLMM","AQU1FRd7papthgdrwPTTq5JacJh8YtwEXaBfKU3bTz45":"Aquifer",
"9W959DqEETiGZocYWCQPaJ6sBmUzgfxXfqGeTEdp3aQP":"OrcaV2","HyaB3W9q6XdA5xwpU4XnSZV94htfmbmqJXZcEbRaJutt":"Invariant",
"BiSoNHVpsVZW2F7rx2eQ59yQwKxzU5NvBcmKshCSUypi":"BisonFi","Gswppe6ERWKpUTXvRPfXdzHhiCyJvLadVvXGfdpBqcE1":"GuacSwap",
"DEXYosS6oEGvk8uCDayvwEZz4qEyDJRf9nFgYCaqPMTm":"1DEX","FLUXubRmkEi2q6K3Y9kBPg9248ggaZVsoSFhtJHSrm1X":"FluxBeam",
"PERPHjGBqRHArX4DySjwM6UJHiR3sWAatqfdBS2qQJu":"JupPerps","dRiftyHA39MWEi3m9aunc5MzRF1JYuBsbn6VPcn33UH":"Drift",
"stkitrT1Uoy18Dk1fTrgPw8W6MVzoCfYoAFT4MLsmhq":"SanctumRouter","SPoo1Ku8WFXoNDMHPsrGSTSG1Y47rzgn41SLUNakuHy":"SPLStakePool",
"CURVGoZn8zycx6FXwwevgBTB2gVvdbGTEpvMJDbgs2t4":"Aldrin?","EewxydAPCCVuNEyrVN68PuSYdQ7wKn27V9Gjeoi8dy3S":"LifinityV1",
"HpNfyc2Saw7RKkQd8nEL4khUcuPhQ7WwY1B2qjx8jxFq":"PancakeCLMM","FUTARELBfJfQ8RDGhg1wdhddq1odMAJUePHFuBYfUxKq":"Futarchy",
"NUMERUNsFCP3kuNmWZuXtm1AaQCPj9uw6Guv2Ekoi5P":"Numeraire","DecZY86MU5Gj7kppfUCEmd4LbXXuyZH1yHaP2NTqdiZB":"Heaven",
"vo1tWgqZMjG61Z2T9qUaMYKqZ75CYzMuaZ2LZP1n7HV":"Vertigo","MoonCVVNZFSYkqNXP6bxHLPL6QQJiMagDL3qcqUQTrG":"Moonshot",
"boop8hVGQGqehUK2iVEMEnMrL5RbjywRzHKBmBE7ry4":"Boop","5U3EU2ubXtK84QcRjWVmYt9RaDyA8gKxdUrPFXmZyaki":"Virtuals",
"WooFif76YGRNjk1pA8wCsN67aQsD9f9iLsz4NcJ1AVb":"Woofi","GAMMA7meSFWaBXF25oSUgmGRwaW6sCMFLmBNiMSdbHVT":"GooseFX",
"KLend2g3cP87fffoy8q1mQqGKjrxjC8boSyAYavgmjD":"KaminoLend(flash?)","MFv2hWf31Z9kbCa1snEPYctwafyhdvnV7FZnsebVacA":"MarginFi(flash?)",
"So1endDq2YkqhipRh3WViPa8hdiSpxWy6z3Z6tMCpAo":"Solend","jupr81YtYssSyPt8jbnGuiWon5f6x9TcDEFxdWUpKhaw":"JupLend?",
}
SKIP = {"11111111111111111111111111111111","ComputeBudget111111111111111111111111111111","TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA",
"TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb","ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL","MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr",
"Memo1UhkJRfHyvLMcVucJwxXeuD728EqVDDwQDxFMNo","AddressLookupTab1e1111111111111111111111111","Sysvar1nstructions1111111111111111111111111"}
TIPS = {"96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5","HFqU5x63VTqvQss8hp11i4wVV8bD44PvwucfZ2bU7gRe","Cw8CFyM9FkoMi7K7Crf6HNQqf4uEMzpKw6QNghXLvLkY",
"ADaUMid9yfUytqMBgopwjb2DTLSokTSzL1zt6iGPaS49","DfXygSm4jCyNCybVYYK6DwvWqjKee8pbDmJGcLWNDXjh","ADuUkR4vqLUMWXxW9gh6D6L8pMSawimctcNZ5pGwDcEt",
"DttWaMuVvTiduZRnguLF7jNxTgiMBZ1hyAumKUiL2KRL","3AVi9Tg9Uo68tJfuvoKvqKNWKkC5wPdSSdeBnizKZ6jT"}
def ixs(tx):
    m = tx["transaction"]["message"]
    out = [(0, ix) for ix in m["instructions"]]
    for g in (tx["meta"].get("innerInstructions") or []):
        for ix in g["instructions"]: out.append((ix.get("stackHeight") or 2, ix))
    return out
recs=[]
unknown=collections.Counter()
for r in rows:
    p=os.path.join(d,"tx",r["txSignature"]+".json")
    if not os.path.exists(p): continue
    tx=json.load(open(p))
    if tx is None: continue
    meta=tx["meta"]; msg=tx["transaction"]["message"]
    keys=[k["pubkey"] for k in msg["accountKeys"]]
    signer=keys[0]
    top=[ix["programId"] for ix in msg["instructions"]]
    venues=[]; tip=0; flash=False
    for h,ix in ixs(tx):
        pid=ix["programId"]
        if pid in DEX: venues.append(DEX[pid])
        elif pid not in SKIP: unknown[pid]+=1
        pr=ix.get("parsed")
        if isinstance(pr,dict) and pr.get("type")=="transfer" and pid=="11111111111111111111111111111111":
            if pr["info"].get("destination") in TIPS: tip+=pr["info"]["lamports"]
    # bot program = top-level non-skip program
    botprog=[x for x in top if x not in SKIP and x not in DEX]
    # mints touched
    mints=set(b["mint"] for b in (meta.get("preTokenBalances") or [])+(meta.get("postTokenBalances") or []))
    other=sorted(m for m in mints if m!=WSOL)
    # notional: max abs WSOL change on non-signer-owned token accounts
    pre={b["accountIndex"]:(b["mint"],b.get("owner"),float(b["uiTokenAmount"]["uiAmount"] or 0)) for b in meta.get("preTokenBalances") or []}
    post={b["accountIndex"]:(b["mint"],b.get("owner"),float(b["uiTokenAmount"]["uiAmount"] or 0)) for b in meta.get("postTokenBalances") or []}
    notional=0
    for i in set(pre)|set(post):
        a=pre.get(i) or post.get(i); b=post.get(i) or pre.get(i)
        if a[0]==WSOL and a[1]!=signer:
            notional=max(notional,abs((post.get(i,(0,0,0))[2])-(pre.get(i,(0,0,0))[2])))
    # dedup consecutive venue hits (some DEX self-CPI for events)
    hops=[]
    for v in venues:
        if not hops or hops[-1]!=v: hops.append(v)
    cu=meta.get("computeUnitsConsumed"); fee=meta["fee"]
    recs.append(dict(sig=r["txSignature"],profit=r["profitAmount"],signer=signer,ts=r["timestamp"],slot=r["slot"],
        venues=hops,nvenue=len(hops),tip=tip/1e9,fee=fee/1e9,cu=cu,botprog=botprog[:2],ntok=len(other),tokens=other,notional=notional,
        nix=len(msg["instructions"]),err=meta.get("err"),ver=tx.get("version")))
json.dump(recs,open(os.path.join(d,which.replace(".json",".recs.json")),"w"))
C=collections.Counter
print("N",len(recs),"total profit",round(sum(x["profit"] for x in recs),3))
tot=sum(x["profit"] for x in recs)
def share(key):
    c=C(); n=C()
    for x in recs:
        for k in (key(x) if isinstance(key(x),(list,set,tuple)) else [key(x)]): c[k]+=x["profit"]; n[k]+=1
    return [(k,n[k],round(v,3),f"{100*v/tot:.0f}%") for k,v in c.most_common(25)]
print("\n-- signers"); [print(*z) for z in share(lambda x:x["signer"])[:15]]
print("\n-- venue set (any leg)"); [print(*z) for z in share(lambda x:sorted(set(x["venues"])))]
print("\n-- route venue sequence"); [print(*z) for z in share(lambda x:" > ".join(x["venues"]))[:25]]
print("\n-- #non-SOL tokens in route"); [print(*z) for z in share(lambda x:x["ntok"])]
print("\n-- #venue hops"); [print(*z) for z in share(lambda x:x["nvenue"])]
print("\n-- tokens"); [print(*z) for z in share(lambda x:x["tokens"])[:20]]
print("\n-- bot program"); [print(*z) for z in share(lambda x:x["botprog"][0] if x["botprog"] else "-")[:12]]
print("\n-- tx version"); [print(*z) for z in share(lambda x:str(x["ver"]))]
import statistics as s
def q(v): v=sorted(v); return [round(v[int(len(v)*p)],6) for p in (0.1,0.5,0.9)] if v else None
print("\nprofit p10/50/90",q([x["profit"] for x in recs]))
print("tip p10/50/90",q([x["tip"] for x in recs]),"tipped frac",sum(1 for x in recs if x["tip"]>0)/len(recs))
print("tip/profit p10/50/90",q([x["tip"]/x["profit"] for x in recs if x["profit"]>0]))
print("fee p10/50/90",q([x["fee"] for x in recs]))
print("cu p10/50/90",q([x["cu"] or 0 for x in recs]))
print("notional SOL p10/50/90",q([x["notional"] for x in recs]))
print("profit/notional bps p10/50/90",q([1e4*x["profit"]/x["notional"] for x in recs if x["notional"]>0]))
print("\nunknown programs",unknown.most_common(20))
