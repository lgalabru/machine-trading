#!/usr/bin/env python3
"""Check the actual public image, including the absence of private flow tooling."""
import argparse,json,subprocess
from pathlib import Path
p=argparse.ArgumentParser(description=__doc__);p.add_argument('--package',type=Path,required=True);a=p.parse_args()
meta=json.loads((a.package/'package.json').read_text())
script=r'''
import json,pathlib,subprocess
root=pathlib.Path('/src/crates/backrun/src')
for name in ['build.rs','reference.rs','runner.rs','verifier.rs','cli.rs','fixture_set.py','generation.json','flow.json']:
    assert not (root/name).exists(), name
assert not pathlib.Path('/private').exists()
public=pathlib.Path('/opt/backrun/public')
assert {p.name for p in public.iterdir()}=={'market.json','playground.json','fixture.json'}
assert 'fixture_set_hash' in json.loads((public/'fixture.json').read_text())
market=json.loads((public/'market.json').read_text());assert len(market['pools'])==128
for directory in [pathlib.Path('/src'),public]:
    for path in directory.rglob('*'):
        if not path.is_file() or path.stat().st_size>30_000_000:continue
        if path.suffix not in ['.rs','.json','.py','.md','.toml']:continue
        text=path.read_text(errors='replace')
        for secret in ['liquidity-driven','incentivized','meme-frenzy','seed_hex']:
            assert secret not in text, str(path)+': '+secret
assert subprocess.run(['mth-backrun','playground','--help'],capture_output=True).returncode==0
for command in ['grade','reference','fixture']:
    assert subprocess.run(['mth-backrun',command,'--help'],capture_output=True).returncode!=0
print(json.dumps({'public_boundary':'passed','private_generators':False,'future_flow':False,'pool_count':128}))
'''
subprocess.run(['docker','run','--rm',meta['images']['development'],'python3','-c',script],check=True)
