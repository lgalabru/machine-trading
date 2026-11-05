#!/usr/bin/env python3
"""Maintainer-only generation of fixed private flows. Never export to the agent SDK."""
import argparse
import base64
import collections
import hashlib
import json
import math
import os
from pathlib import Path
import random
import shutil
import subprocess

PROTOCOL = 'continuous-arrival-v1'
SEALED_FILES = ('market.json', 'flow.json', 'snapshot.json', 'provenance.json',
                'users.json', 'competition.json', 'flow-manifest.json')
REGIMES = ('liquidity-driven', 'incentivized', 'meme-frenzy')


def read(path):
    return json.loads(Path(path).read_text())


def write(path, value):
    Path(path).write_text(json.dumps(value, indent=2, sort_keys=True) + '\n')


def digest(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def canonical_hash(value):
    return hashlib.sha256(json.dumps(value, sort_keys=True, separators=(',', ':')).encode()).hexdigest()


def seal_fixture(root):
    files = {name: digest(root / name) for name in SEALED_FILES}
    seal = {'version': 1, 'files': files, 'fixture_hash': canonical_hash(files)}
    write(root / 'seal.json', seal)
    return seal['fixture_hash']


def native_verify(binary, root):
    result = json.loads(subprocess.check_output(
        [str(binary), 'fixture', 'verify', '--fixture', str(root)], text=True))
    if result['fixture_hash'] != read(root / 'seal.json')['fixture_hash']:
        raise ValueError('native fixture verification disagrees')
    return result['fixture_hash']


def pool_states(market, snapshot):
    """Local effective reserves, not TVL; Whirlpool estimates stop at tick crossings."""
    result = []
    for pool in market['pools']:
        if pool['kind'] == 'constant_product':
            reserves = [int.from_bytes(base64.b64decode(snapshot[v]['data'])[64:72], 'little')
                        for v in pool['vaults']]
        else:
            data = base64.b64decode(snapshot[pool['address']]['data'])
            liquidity = int.from_bytes(data[49:65], 'little')
            sqrt = int.from_bytes(data[65:81], 'little') / 2**64
            reserves = [liquidity / sqrt, liquidity * sqrt]
        if min(reserves) <= 0:
            raise ValueError('non-positive initial liquidity')
        result.append(reserves)
    return result


def quote(pool, reserves, direction, amount):
    i = 0 if direction else 1
    effective = amount * (1 - pool['fee_millionths'] / 1_000_000)
    return reserves[1-i] * effective / (reserves[i] + effective)


def order(rng, slot, pool, direction, amount, minimum=1):
    return {'slot': slot, 'route': [{'pool': pool, 'a_to_b': direction}],
            'amount_in': int(amount), 'min_out': max(1, int(minimum)),
            'wallet': rng.randrange(8)}


def ordinary_size(rng):
    u = rng.random()
    if u < .4:
        return rng.randint(1_000, 100_000)
    if u < .9:
        return rng.randint(100_000_000, 3_000_000_000)
    return rng.randint(3_000_000_000, 15_000_000_000)


def liquidity_flow(market, states, rng):
    pairs = collections.defaultdict(list)
    for i, pool in enumerate(market['pools']):
        pairs[tuple(pool['mints'])].append(i)
    pair_list = list(pairs.values())
    # Uneven token-pair attention, independent of venue. Never uniform by mandate.
    attention = [rng.lognormvariate(0, 1.1) for _ in pair_list]
    orders = []
    for slot in range(2000):
        for _ in range(rng.choice([0, 0, 1, 1, 3])):
            candidates = rng.choices(pair_list, weights=attention)[0]
            direction, amount = bool(rng.getrandbits(1)), ordinary_size(rng)
            i = 0 if direction else 1
            floor = min(states[p][i] for p in candidates)
            # Favor depth AND net execution quality, while retaining 15% venue
            # exploration. These are generation-time initial-state estimates;
            # the frozen order is never re-routed at evaluation time.
            weights = [math.sqrt(states[p][i] / floor) *
                       (quote(market['pools'][p], states[p], direction, amount) / amount)**4
                       for p in candidates]
            pool = rng.choice(candidates) if rng.random() < .15 else rng.choices(candidates, weights=weights)[0]
            minimum = quote(market['pools'][pool], states[pool], direction, amount) * .95
            orders.append(order(rng, slot, pool, direction, amount, minimum))
    return orders, {'selection': 'uneven pair demand; local depth and fee/impact-adjusted initial quotes',
                    'venue_exploration_probability': .15,
                    'depth_is_not_tvl': True, 'quotes_are_local_not_tick_exact': True}


def frenzy_flow(market, states, rng):
    # WSOL-only quote avoids inventing an unbacked synthetic stablecoin peg.
    eligible = [i for i,p in enumerate(market['pools'])
                if p['kind'] == 'constant_product' and 0 in p['mints']]
    if len(eligible) < 2:
        raise ValueError('frenzy requires at least two WSOL pairs with alternative paths')
    eligible.sort(key=lambda i: states[i][market['pools'][i]['mints'].index(0)])
    eligible = eligible[:min(3, len(eligible))]
    count = rng.choice([4, 5])
    durations = [rng.randint(190, 290) for _ in range(count)]
    remaining = 2000 - sum(durations)
    raw_gaps = [rng.randint(40, 140) for _ in range(count + 1)]
    gaps = [int(remaining * x / sum(raw_gaps)) for x in raw_gaps]
    gaps[-1] += remaining - sum(gaps)
    episodes, cursor = [], gaps[0]
    for n, duration in enumerate(durations):
        pool = eligible[n % len(eligible)]
        turn = cursor + rng.randint(int(.3 * duration), int(.72 * duration))
        episodes.append({'start': cursor, 'turn': turn, 'end': cursor+duration,
                         'pool': pool, 'intensity': rng.uniform(.65, 1.0),
                         'size_fraction': rng.uniform(.002, .009)})
        cursor += duration + gaps[n+1]
    orders = []
    for slot in range(2000):
        # Background demand includes both directions and all venues throughout.
        if rng.random() < .18:
            orders.append(order(rng, slot, rng.randrange(128), bool(rng.getrandbits(1)), ordinary_size(rng)))
        for episode in episodes:
            if not episode['start'] <= slot < episode['end']:
                continue
            if rng.random() > episode['intensity']:
                continue
            pool = episode['pool']; p = market['pools'][pool]
            buying = slot < episode['turn']
            for _ in range(rng.choice([1, 1, 1, 2, 3])):
                buy = buying if rng.random() < .88 else not buying
                direction = (p['mints'][0] == 0) if buy else (p['mints'][0] != 0)
                side = 0 if direction else 1
                amount = max(100_000, int(states[pool][side] * episode['size_fraction'] * rng.uniform(.4, 1.8)))
                minimum = quote(p, states[pool], direction, amount) * .2
                orders.append(order(rng, slot, pool, direction, amount, minimum))
    orders.sort(key=lambda o: o['slot'])
    return orders, {'episodes': episodes, 'quote_token': 'WSOL', 'stablecoin_assumption': None,
                    'meme_tokens': sorted({next(t for t in market['pools'][i]['mints'] if t != 0) for i in eligible}),
                    'counter_direction_probability': .12,
                    'sell_effect': 'meme input rises; WSOL quote reserve falls on successful sell'}


def validate_flow(flow, market):
    if flow['version'] != 1 or flow['duration_slots'] != 2000 or not flow['orders']:
        raise ValueError('invalid scenario duration or empty flow')
    previous = -1
    for o in flow['orders']:
        if not previous <= o['slot'] < 2000 or len(o['route']) != 1:
            raise ValueError('unordered or invalid route')
        previous = o['slot']
        if not 0 <= o['wallet'] < 8 or min(o['amount_in'],o['min_out']) <= 0:
            raise ValueError('invalid order amount or wallet')
        if not 0 <= o['route'][0]['pool'] < len(market['pools']):
            raise ValueError('invalid pool')


def build(source, out, binary, seed=None):
    source, out, binary = Path(source).resolve(), Path(out).resolve(), Path(binary).resolve()
    if out.exists():
        raise FileExistsError('immutable set output already exists')
    seed = seed if seed is not None else os.urandom(32)
    if len(seed) != 32:
        raise ValueError('expected 32 private seed bytes')
    out.mkdir(parents=True, mode=0o700)
    try:
        base = out / 'incentivized'
        # Adopts the newer runtime without regenerating any original market,
        # user flow, snapshot, users or setup provenance bytes.
        subprocess.run([str(binary), 'fixture', 'migrate-execution', '--source', str(source), '--out', str(base)], check=True, capture_output=True)
        market, snapshot = read(base/'market.json'), read(base/'snapshot.json')
        states = pool_states(market, snapshot)
        old_hash = read(source/'seal.json')['fixture_hash']
        entries = []
        for index, regime in enumerate(REGIMES):
            folder = out / regime
            if regime != 'incentivized':
                shutil.copytree(base, folder)
                # copytree preserves immutable source modes; outputs are new private files.
                for path in folder.iterdir(): path.chmod(0o600)
                rng = random.Random(hashlib.sha256(seed + regime.encode()).digest())
                orders, provenance = (liquidity_flow if index == 0 else frenzy_flow)(market, states, rng)
                flow = {'version':1, 'duration_slots':2000, 'orders':orders}
                validate_flow(flow, market)
                write(folder/'flow.json', flow)
                original = read(folder/'provenance.json')
                write(folder/'provenance.json', {'original_setup':original, 'flow_generation':provenance,
                                               'seed_sha256': hashlib.sha256(seed).hexdigest(),
                                               'generator_sha256': digest(__file__)})
                manifest = read(folder/'flow-manifest.json')
                manifest.update(flow_sha256=digest(folder/'flow.json'), previous_fixture_hash=old_hash)
                write(folder/'flow-manifest.json', manifest)
                seal_fixture(folder)
            fixture_hash = native_verify(binary, folder)
            # Every scenario deliberately has identical observable initial state.
            for name in ['market.json','snapshot.json','users.json']:
                if digest(folder/name) != digest(source/name): raise ValueError('initial state changed')
            if regime == 'incentivized':
                for name in ['flow.json','provenance.json']:
                    if digest(folder/name) != digest(source/name): raise ValueError('legacy fixture assets changed')
            entries.append({'id':f'scenario-{index+1:02}', 'regime':regime, 'path':regime,
                            'fixture_hash':fixture_hash, 'flow_sha256':digest(folder/'flow.json')})
        write(out/'generation.json', {'version':1, 'seed_hex':seed.hex(), 'generator_sha256':digest(__file__),
                                     'python_version':__import__('sys').version, 'source_fixture_hash':old_hash})
        files = {str(p.relative_to(out)):digest(p) for p in sorted(out.rglob('*')) if p.is_file()}
        manifest = {'version':1, 'protocol':PROTOCOL, 'slot_ms':200, 'scenarios':entries,
                    'source_fixture_hash':old_hash, 'market_sha256':digest(base/'market.json'),
                    'aggregation':None, 'files':files}
        manifest['set_hash'] = canonical_hash(manifest)
        write(out/'set.json', manifest)
        verify(out, binary)
        return manifest
    except BaseException:
        (out/'BUILD_FAILED').touch()
        raise


def verify(root, binary):
    root = Path(root).resolve(); manifest = read(root/'set.json')
    if (root/'BUILD_FAILED').exists(): raise ValueError('failed fixture build')
    expected = manifest.copy(); claimed = expected.pop('set_hash')
    if canonical_hash(expected) != claimed: raise ValueError('set manifest hash mismatch')
    if manifest['aggregation'] is not None or manifest['slot_ms'] != 200 or [s['regime'] for s in manifest['scenarios']] != list(REGIMES):
        raise ValueError('unexpected evaluation contract')
    for name, hash_ in manifest['files'].items():
        p = Path(name)
        if p.is_absolute() or '..' in p.parts or any((root/Path(*p.parts[:i])).is_symlink() for i in range(1,len(p.parts)+1)):
            raise ValueError('unsafe manifest path')
        if digest(root/p) != hash_: raise ValueError('set file changed: '+name)
    for s in manifest['scenarios']:
        if s['path'] != s['regime'] or native_verify(Path(binary).resolve(), root/s['path']) != s['fixture_hash']:
            raise ValueError('scenario hash mismatch')
    return manifest


def main():
    p=argparse.ArgumentParser(description=__doc__); sub=p.add_subparsers(dest='command',required=True)
    b=sub.add_parser('build'); b.add_argument('--source',type=Path,required=True);b.add_argument('--out',type=Path,required=True)
    b.add_argument('--seed-file',type=Path,help='Private 32-byte generation seed; never needed during grading')
    v=sub.add_parser('verify');v.add_argument('--fixture-set',type=Path,required=True)
    for parser in [b,v]:parser.add_argument('--binary',type=Path,default=Path('target/debug/mth-backrun'))
    a=p.parse_args();os.umask(0o077)
    m=build(a.source,a.out,a.binary,a.seed_file.read_bytes() if a.seed_file else None) if a.command=='build' else verify(a.fixture_set,a.binary)
    print(json.dumps({'set_hash':m['set_hash'],'scenarios':[{k:s[k] for k in ['id','fixture_hash']} for s in m['scenarios']]}))

if __name__=='__main__':main()
