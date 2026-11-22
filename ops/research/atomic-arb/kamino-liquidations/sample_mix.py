import sys, json, re, collections
sys.path.insert(0, sys.argv[1])
import rpc
from concurrent.futures import ThreadPoolExecutor
r = rpc.call('getSignaturesForAddress', ['KLend2g3cP87fffoy8q1mQqGKjrxjC8boSyAYavgmjD', {'limit': 1000}])
sigs = [x['signature'] for x in r[:300]]
with ThreadPoolExecutor(8) as ex:
    txs = list(ex.map(rpc.get_tx, sigs))
c = collections.Counter(); cf = collections.Counter()
for t in txs:
    if not t or '__error' in t: continue
    logs = t['meta']['logMessages'] or []
    names = []
    on = False
    for i, l in enumerate(logs):
        if l.startswith('Program KLend2g3cP87fffoy8q1mQqGKjrxjC8boSyAYavgmjD invoke'):
            if i + 1 < len(logs) and logs[i+1].startswith('Program log: Instruction: '):
                names.append(logs[i+1].split(': ')[-1])
    key = tuple(sorted(set(n for n in names if not n.startswith('Refresh'))))
    (cf if t['meta']['err'] else c)[key] += 1
print('OK'); [print(v, k) for k, v in c.most_common(25)]
print('FAILED'); [print(v, k) for k, v in cf.most_common(25)]
