import json,sys,os,urllib.request
rpc=sys.argv[1]; d=sys.argv[2]; N=int(sys.argv[3])
recs=json.load(open(os.path.join(d,'top200.recs.json')))
recs=sorted(recs,key=lambda r:-r['profit'])[:N]
def call(m,p):
    req=urllib.request.Request(rpc,json.dumps({"jsonrpc":"2.0","id":1,"method":m,"params":p}).encode(),{"content-type":"application/json"})
    return json.load(urllib.request.urlopen(req,timeout=60))["result"]
SKIPW={"So11111111111111111111111111111111111111112"}
for r in recs:
    tx=json.load(open(os.path.join(d,'tx',r['sig']+'.json')))
    keys=tx['transaction']['message']['accountKeys']
    signer=keys[0]['pubkey']
    w=set(k['pubkey'] for k in keys if k['writable'] and not k['signer'])
    la=tx['meta'].get('loadedAddresses') or {}
    w|=set(la.get('writable',[]))
    blk=call("getBlock",[r['slot'],{"encoding":"json","transactionDetails":"accounts","maxSupportedTransactionVersion":1,"rewards":False}])
    txs=blk['transactions']
    idx=next(i for i,t in enumerate(txs) if t['transaction']['signatures'][0]==r['sig'])
    prev=[]
    for j in range(idx-1,-1,-1):
        t=txs[j]; ks=t['transaction']['accountKeys']
        tw=set(k['pubkey'] for k in ks if k['writable'])
        ov=(tw&w)
        if ov and ks[0]['pubkey']!=signer:
            sol_moves=[(b['owner'][:6],round((float(b['uiTokenAmount']['uiAmount'] or 0)),2)) for b in (t['meta'].get('postTokenBalances') or []) if b['mint']=='So11111111111111111111111111111111111111112'][:2]
            prev.append((idx-j, ks[0]['pubkey'][:8], len(ov), 'err' if t['meta'].get('err') else 'ok'))
            if len(prev)>=3: break
    # check previous slot too if nothing found
    print(f"{r['profit']:.3f} SOL {r['cat'][:22]:22s} {'>'.join(r['venues'])[:60]:60s} pos {idx}/{len(txs)} tip={r['tip']:.4f} fee={r['fee']:.4f} | same-pool writers before it in block (distance,signer,overlap): {prev}")
