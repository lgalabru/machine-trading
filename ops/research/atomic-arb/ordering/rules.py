"""Agreement of block-ordering rules with mainnet's winners (2026-11-22).

The pipeline: `gtfa.py` and `klend_collect.py` fetch the windows,
`whale_cases.py`, `fetch_cands.py`, `sol_usdc_cases.py` and
`kamino_contests.py` rebuild the contested slots into
/tmp/arb-research/validation/{sol_usdc_cases,kamino_contests}.json
(`txfields.py` reads a transaction's units, fees and tips, version 1
included), `stats.py` summarises them, and this script scores the rules
docs/tx-layer.md ("Ranking") reports. `lib.py`, `rpc.py`, `b58.py` and
`parse_liq.py` are the studies' own, as the clmm-backrun and
kamino-liquidations folders keep them (`parse_liq.py` reads version 1
transactions too). Run with `python3 -I`.

For each case, the contenders that landed in the winner's slot (the arb's,
or the liquidation's). A rule ranks them; it agrees when it puts the
winner first. Rules:
  old:      tipped first (by pay per CU), then untipped by priority per CU
  pay:      one scale, pay per CU (priority + tip over requested CU), all simultaneous
  window K: pay decides among contenders landing within K positions of each
            other's arrival (index as arrival proxy): the winner is first unless a
            contender within K positions after it (or before it) pays more.
"""
import json, sys

def load(path, kamino):
    cases = json.load(open(path))
    out = []
    for c in cases:
        slot = c.get('winner_slot') or c.get('slot')
        cons = c['contenders']
        w = [x for x in cons if x.get('is_winner')]
        if not w:
            continue
        w = w[0]
        same = [x for x in cons if x['slot'] == w['slot']]
        others = [x for x in same if not x.get('is_winner')]
        if kamino:
            pass
        if not others:
            continue
        out.append((c['id'], w, others))
    return out

def pay(x, base):
    return x['pay_per_cu_incl_base' if base else 'pay_per_cu']

def tipped(x):
    return (x.get('tip_lamports') or 0) > 0

def agree_old(w, others, base):
    key = lambda x: (1 if tipped(x) else 0, pay(x, base))
    return all(key(w) >= key(o) for o in others)

def agree_pay(w, others, base):
    return all(pay(w, base) >= pay(o, base) for o in others)

def agree_window(w, others, base, k):
    near = [o for o in others if abs(o['index'] - w['index']) <= k]
    return all(pay(w, base) >= pay(o, base) for o in near)

for name, path, kamino in [('SOL/USDC', 'sol_usdc_cases.json', False), ('Kamino', 'kamino_contests.json', True)]:
    cases = load('/tmp/arb-research/validation/' + path, kamino)
    n = len(cases)
    for base in (False, True):
        tag = 'incl. base' if base else 'no base'
        old = sum(agree_old(w, o, base) for _, w, o in cases)
        one = sum(agree_pay(w, o, base) for _, w, o in cases)
        line = f'{name} ({tag}) n={n}: old {old}, pay {one}'
        for k in (5, 10, 20, 50, 100):
            line += f', window{k} {sum(agree_window(w, o, base, k) for _, w, o in cases)}'
        print(line)
