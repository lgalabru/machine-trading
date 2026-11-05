#!/usr/bin/env python3
"""Freeze a maintainer release only after independently recorded acceptance checks.

Evidence directory: smoke-report.json, noop-report.json, replay.json,
isolation.json, checks.json. checks.json maps check names to {exit_code, log}.
Log paths resolve relative to that directory. Retain the original Harbor jobs.
"""
import argparse
import hashlib
import json
import os
import pathlib
import shutil
import subprocess

p = argparse.ArgumentParser(description=__doc__)
p.add_argument('--fixture', type=pathlib.Path, required=True)
p.add_argument('--evidence', type=pathlib.Path, required=True)
p.add_argument('--out', type=pathlib.Path, required=True)
p.add_argument('--binary', type=pathlib.Path, default=pathlib.Path('target/debug/mth-backrun'))
a = p.parse_args()
# Retained for provenance of legacy releases. A current task promises three
# sessions, so freezing it with a single fixture would misstate the contract.
import tomllib
if tomllib.loads(pathlib.Path('bench/tasks/atomic-arb/task.toml').read_text())['task']['version'] != '0.4.0':
    raise SystemExit('Use freeze_set.py for the current three-scenario task; legacy releases remain unchanged.')

def read(name):
    return json.loads((a.evidence / name).read_text())
def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()

verified = json.loads(subprocess.check_output([str(a.binary.resolve()), 'fixture', 'verify', '--fixture', str(a.fixture)], text=True))
fixture_hash = verified['fixture_hash']
smoke, noop, replay, isolation, checks = [read(n) for n in ['smoke-report.json', 'noop-report.json', 'replay.json', 'isolation.json', 'checks.json']]
for report in [smoke, noop]:
    assert report['fixture_hash'] == fixture_hash and report['valid'], 'invalid or different fixture'
    assert len(report['participants']) == 4 and all(p['score']['valid'] for p in report['participants'])
    assert report['protocol'] == 'continuous-arrival-v1' and report['grading_phase'] == 'post-run'
    assert report['completed_slots'] == 2000 and len(report['timing']['slots']) == 2000 and report['timing']['coalesced_slots'] == 0, 'incomplete user flow'
assert smoke['participants'][0]['score']['reward'] > 0, 'smoke bot must profit after fees'
assert noop['participants'][0]['score']['reward'] == 0 and noop['participants'][0]['executions'] == 0
assert smoke['runtime_binary_sha256'] == noop['runtime_binary_sha256'], 'different runtime binaries'
assert smoke['hardware']['fingerprint'] == noop['hardware']['fingerprint'], 'different hardware profiles'
assert smoke['coverage']['successful_mixed_multihop_routes'] > 0 and smoke['coverage']['maximum_successful_swaps'] >= 3
assert replay['verified'] and replay['fixture_hash'] == fixture_hash
expected_transactions = smoke['user_executions'] + sum(p['executions'] for p in smoke['participants'])
assert replay['transactions'] == expected_transactions, 'replay must cover the full smoke run'
assert isolation['isolation'] == 'passed'
assert isolation['searcher_cpus'] == [0, 1, 2, 3] and isolation['grader_cpus'] == list(range(4, 12))
for name in ['workspace', 'real_programs', 'continuous', 'all_pools', 'harbor_lint']:
    check = checks[name]
    assert check['exit_code'] == 0, f'{name} failed'
    log = a.evidence / check['log']
    assert log.is_file(), f'missing {name} log'
    if name != 'harbor_lint':
        assert 'test result: ok.' in log.read_text() and 'test result: FAILED' not in log.read_text()
    assert digest(log) == check['sha256'], f'{name} evidence changed'

# Never overwrite an earlier frozen release or silently regenerate its flow.
a.out.mkdir(parents=True, exist_ok=False, mode=0o700)
try:
    shutil.copytree(a.fixture, a.out / 'fixture')
    shutil.copytree(a.evidence, a.out / 'evidence')
    images = {}
    for name in ['runtime', 'candidate', 'verifier', 'development']:
        image = f'mth-backrun-{name}:local'
        images[image] = json.loads(subprocess.check_output(['docker', 'image', 'inspect', image], text=True))[0]['Id']
    sources = {}
    for directory in ['crates/backrun', 'programs/backrun-executor', 'bench/backrun', 'bench/tasks/atomic-arb']:
        for path in pathlib.Path(directory).rglob('*'):
            if path.is_file() and not any(part in ['target','artifact','__pycache__'] for part in path.parts):
                sources[str(path)] = digest(path)
    for name in ['Cargo.toml', 'Cargo.lock', 'crates/cli/Cargo.toml', 'crates/cli/src/main.rs', 'crates/cli/src/evaluate.rs']:
        sources[name] = digest(pathlib.Path(name))
    # A runnable release task references immutable local image IDs, not tags.
    shutil.copytree('bench/tasks/atomic-arb', a.out / 'task', ignore=shutil.ignore_patterns('artifact', '__pycache__'))
    compose = a.out / 'task/tests/docker-compose.yaml'
    text = compose.read_text()
    for name, identifier in images.items():
        text = text.replace(name, identifier)
    compose.write_text(text)
    task = a.out / 'task/task.toml'
    task.write_text(task.read_text().replace('mth-backrun-verifier:local', images['mth-backrun-verifier:local']).replace('[environment]\n', '[environment]\ndocker_image = '+json.dumps(images['mth-backrun-development:local'])+'\n'))
    dockerfile = a.out / 'task/environment/Dockerfile'
    dockerfile.write_text(dockerfile.read_text().replace('mth-backrun-development:local', images['mth-backrun-development:local']))
    flow_manifest=json.loads((a.fixture/'flow-manifest.json').read_text())
    release = {'version': 3, 'user_flow_engine': 'swap-injector-v1', 'protocol': 'continuous-arrival-v1', 'market_sha256': flow_manifest['market_sha256'], 'flow_sha256': flow_manifest['flow_sha256'], 'previous_fixture_hash': flow_manifest['previous_fixture_hash'], 'fixture_hash': fixture_hash, 'hardware_fingerprint': smoke['hardware']['fingerprint'], 'runtime_binary_sha256': smoke['runtime_binary_sha256'], 'images': images, 'sources': sources, 'evidence': {str(p.relative_to(a.evidence)): digest(p) for p in a.evidence.rglob('*') if p.is_file()}, 'policy': 'one scored run per artifact; diagnostics never select a best result'}
    (a.out / 'release.json').write_text(json.dumps(release, indent=2) + '\n')
    for root, dirs, files in os.walk(a.out):
        for name in files:
            os.chmod(pathlib.Path(root) / name, 0o400)
        os.chmod(root, 0o500)
except BaseException:
    # Keep failed staging evidence for diagnosis, without a successful release marker.
    (a.out / 'FREEZE_FAILED').touch()
    raise
print(json.dumps({'frozen': str(a.out.resolve()), 'fixture_hash': fixture_hash}))
