#!/usr/bin/env python3
"""Rank one scored candidate result per artifact; reject incompatible/repeated runs."""
import argparse, json, pathlib
p = argparse.ArgumentParser(description=__doc__)
p.add_argument('reports', nargs='+', type=pathlib.Path)
a = p.parse_args()
profile = None
seen = set()
valid = []
excluded = []
for path in a.reports:
    report = json.loads(path.read_text())
    if report.get('status') == 'infrastructure_error':
        p.error(f'{path}: infrastructure failures have no trading score')
    candidate = report['participants'][0]
    if not report['valid'] or not candidate['score']['valid']:
        excluded.append({'report': str(path), 'reason': candidate['score']['disqualification']})
        continue
    identity = (report['fixture_hash'], report['hardware']['fingerprint'], report['runtime_binary_sha256'])
    if profile is not None and identity != profile:
        p.error('cannot combine different fixtures, hardware profiles or runtimes')
    profile = identity
    artifact = report['artifact_hash']
    if artifact in seen:
        p.error('multiple results for one artifact: diagnostic repeats are not ranking inputs')
    seen.add(artifact)
    valid.append({'artifact_hash': artifact, 'net_profit_lamports': candidate['score']['net_profit_lamports'], 'net_profit_sol': candidate['score']['reward'], 'report': str(path)})
valid.sort(key=lambda r: (-r['net_profit_lamports'], r['artifact_hash']))
print(json.dumps({'profile': profile, 'ranking': valid, 'disqualified': excluded}, indent=2))
