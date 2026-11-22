import json, sys, time, urllib.request, os
rpc = sys.argv[1]; src = sys.argv[2]; outdir = sys.argv[3]
os.makedirs(outdir, exist_ok=True)
rows = json.load(open(src))
def call(method, params):
    body = json.dumps({"jsonrpc":"2.0","id":1,"method":method,"params":params}).encode()
    for attempt in range(4):
        try:
            req = urllib.request.Request(rpc, body, {"content-type":"application/json"})
            r = json.load(urllib.request.urlopen(req, timeout=30))
            if "error" in r: raise Exception(r["error"])
            return r["result"]
        except Exception as e:
            time.sleep(1.5*(attempt+1)); err=e
    raise err
for i,row in enumerate(rows):
    p = os.path.join(outdir, row["txSignature"]+".json")
    if os.path.exists(p): continue
    try:
        tx = call("getTransaction", [row["txSignature"], {"encoding":"jsonParsed","maxSupportedTransactionVersion":1}])
        json.dump(tx, open(p,"w"))
    except Exception as e:
        print("fail", row["txSignature"], e, file=sys.stderr)
    time.sleep(0.05)
print("done", src)
