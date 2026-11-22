import json,os,sys,collections,re
d=sys.argv[1]; which=sys.argv[2]
recs=json.load(open(os.path.join(d,which)))
NAMES={"24Uqj9JCLxUeoC3hGfh5W3s9FM9uCHDS2SG3LYwBpyTi":"?24Uqj(Deposit)","jupeiUmn818Jg1ekPURTpr4mFo29p46vygyykFJ3wZC":"JupLend-liq-vault",
"SCoRcH8c2dpjvcJD6FiPbCSQyQgu3PcUAWj2Xxx3mqn":"Scorch(propAMM?)","ojh19ojaKduoJZuaJADhcVGp4xt1TcdAvZmpVsCorch":"Scorch-oracle",
"MNFSTqtC93rEfYHB6hF82sKdZpUDFWkViLByLd1k1Ms":"Manifest","goonuddtQRrWqqn5nFyczVKaie28f3kDkHWkHtURSLE":"GoonFiV2(propAMM)",
"ALPHAQmeA7bjrVuccPsYPiCvsi428SNwte66Srvs4pHA":"AlphaQ(propAMM)","HYEXCHtHkBagdStcJCp3xbbb9B7sdMdWXFNj6mdsG4hn":"Hylo(xSOL/hyUSD mint)",
"jupr81YtYssSyPt8jbnGuiWon5f6x9TcDEFxYe3Bdzi":"JupLend","jupnw4B6Eqs7ft6rxpzYLJZYSnrpRgPcr589n5Kv4oc":"JupLend-oracle"}
cats=collections.Counter(); catn=collections.Counter(); ex=collections.defaultdict(list)
allix=collections.Counter()
for r in recs:
    tx=json.load(open(os.path.join(d,'tx',r['sig']+'.json')))
    logs=tx['meta']['logMessages'] or []
    ixn=set()
    for i,l in enumerate(logs):
        m=re.match(r'Program (\w+) invoke \[(\d+)\]',l)
        if m and i+1<len(logs):
            n=re.match(r'Program log: Instruction: (\w+)',logs[i+1])
            if n: ixn.add((m.group(1)[:6],n.group(1)))
    names=" ".join(x[1] for x in ixn)
    for x in ixn: allix[x]+=1
    progs=set(re.findall(r'Program (\w+) invoke',"\n".join(logs)))
    v=set(r['venues'])
    if re.search(r'Liquidat',names): c='liquidation'
    elif re.search(r'FlashBorrow|FlashLoan|flash',names,re.I) or 'jupeiUmn818Jg1ekPURTpr4mFo29p46vygyykFJ3wZC' in progs: c='flashloan-funded arb'
    elif 'HYEXCHtHkBagdStcJCp3xbbb9B7sdMdWXFNj6mdsG4hn' in progs or re.search(r'DepositSol|WithdrawSol|DepositStake|Mint.*Lst|StakeWrappedSol|SwapViaStake',names): c='LST/synthetic mint-redeem arb'
    elif r['ntok']>=2: c='multi-token cycle (triangular+)'
    else: c='2-pool cycle SOL>X>SOL'
    props=[NAMES.get(p) for p in progs if NAMES.get(p) and 'propAMM' in NAMES.get(p)]
    if props or v & {'HumidiFi','SolFiV2','SolFi','ZeroFi','TesseraV','GoonFi','BisonFi','ObricV2','AlphaQ'}: r['propamm']=True
    cats[c]+=r['profit']; catn[c]+=1; ex[c].append(r)
    r['cat']=c; r['ixnames']=sorted(ixn)
tot=sum(cats.values())
print(which, "total",round(tot,3))
for c,v in cats.most_common(): print(f"  {c:38s} n={catn[c]:3d} profit={v:7.3f} SOL ({100*v/tot:.0f}%) median={sorted(x['profit'] for x in ex[c])[len(ex[c])//2]:.4f}")
print("  touches a prop AMM:", sum(1 for r in recs if r.get('propamm')), round(sum(r['profit'] for r in recs if r.get('propamm')),3))
print("  tipped (Jito):", sum(1 for r in recs if r['tip']>0), " high prio fee >0.01:", sum(1 for r in recs if r['fee']>0.01), " tx failed:", sum(1 for r in recs if r['err']))
json.dump(recs,open(os.path.join(d,which),'w'))
if len(sys.argv)>3:
  print("\ntop instruction names:"); [print("  ",k,n) for k,n in allix.most_common(40)]
