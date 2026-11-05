#!/usr/bin/env python3
"""Freeze a scenario set only after all three smoke/no-op runs and exact replays pass."""
import argparse
import base64
import json
import os
from pathlib import Path
import shutil
import subprocess
from fixture_set import read, write, digest, verify


def frenzy_execution_check(fixture, report_path):
    market=read(fixture/'market.json');episodes=read(fixture/'provenance.json')['flow_generation']['episodes']
    counts=[{'buy':0,'sell':0,'quote_paid_on_buys':0,'quote_removed_on_sells':0} for _ in episodes]
    for line in (report_path.parent/'trace.jsonl').read_text().splitlines():
        row=json.loads(line)
        if row['participant'] is not None:continue
        receipt=row['receipt'];meta=receipt['meta']
        if meta['err'] is not None:continue
        keys=receipt['transaction']['message']['accountKeys']+meta['loadedAddresses']['writable']+meta['loadedAddresses']['readonly']
        before={keys[b['accountIndex']]:int(b['uiTokenAmount']['amount']) for b in meta['preTokenBalances']}
        after={keys[b['accountIndex']]:int(b['uiTokenAmount']['amount']) for b in meta['postTokenBalances']}
        # runner logical slots are one-based here; fixture schedule is zero-based.
        slot=row['slot']-1
        for i,e in enumerate(episodes):
            pool=market['pools'][e['pool']]
            if not e['start']<=slot<e['end'] or pool['address'] not in keys:continue
            quote_index=pool['mints'].index(0);q=pool['vaults'][quote_index];m=pool['vaults'][1-quote_index]
            dq=after[q]-before[q];dm=after[m]-before[m]
            if dq>0 and dm<0:
                counts[i]['buy']+=1;counts[i]['quote_paid_on_buys']+=dq
            elif dq<0 and dm>0:
                counts[i]['sell']+=1;counts[i]['quote_removed_on_sells']-=dq
            else:raise ValueError('swap did not produce the expected opposing reserve changes')
    if not all(c['buy']>0 and c['sell']>0 and c['quote_removed_on_sells']>0 for c in counts):
        raise ValueError('an episode lacks executed buys or quote-depleting sells')
    return counts


def freeze(args):
    manifest=verify(args.fixture_set,args.binary);package=read(args.package/'package.json')
    if package['fixture_set_hash']!=manifest['set_hash']:raise ValueError('wrong packaged fixture set')
    smoke,noop=read(args.smoke/'report.json'),read(args.noop/'report.json')
    for summary in [smoke,noop]:
        if summary['status']!='complete' or len(summary['scenarios'])!=3 or summary['aggregation'] is not None:
            raise ValueError('incomplete three-scenario calibration')
        if summary['fixture_set_hash']!=manifest['set_hash']:raise ValueError('wrong calibration set')
    hardware=None;proof=[]
    for index,scenario in enumerate(manifest['scenarios']):
        sid=scenario['id'];reports=[]
        for job in [args.smoke,args.noop]:
            entry=read(job/'report.json')['scenarios'][index]
            path=job/entry['report'];report=read(path);reports.append((path,report))
            if entry['id']!=sid or report['fixture_hash']!=scenario['fixture_hash'] or not report['valid']:
                raise ValueError('invalid scenario evidence')
            if report['completed_slots']!=2000 or len(report['timing']['slots'])!=2000 or report['timing']['coalesced_slots']!=0:
                raise ValueError('clock or flow incomplete')
            if report['runtime_binary_sha256']!=package['runtime_binary_sha256']:raise ValueError('wrong runtime')
            h=report['hardware']['fingerprint'];hardware=hardware or h
            if h!=hardware:raise ValueError('mixed hardware profiles')
        active,idle=reports[0][1],reports[1][1]
        if active['participants'][0]['score']['reward']<=0:raise ValueError('smoke bot must profit in every scenario')
        if idle['participants'][0]['score']['reward']!=0 or idle['participants'][0]['executions']!=0:raise ValueError('no-op is not exactly zero')
        replay_path=args.replays/(sid+'.json');replay=read(replay_path)
        expected=active['user_executions']+sum(p['executions'] for p in active['participants'])
        if not replay['verified'] or replay['fixture_hash']!=scenario['fixture_hash'] or replay['transactions']!=expected:
            raise ValueError('incomplete exact replay')
        proof.append({'id':sid,'smoke_net_sol':active['participants'][0]['score']['reward'],'noop_net_sol':0,
                      'smoke_report':str(reports[0][0].resolve()),'smoke_report_sha256':digest(reports[0][0]),
                      'noop_report':str(reports[1][0].resolve()),'noop_report_sha256':digest(reports[1][0]),
                      'replay_sha256':digest(replay_path),'replay_transactions':expected})
    checks=read(args.checks)
    for name in ['workspace','native','python','runtime_pin','harbor_lint','isolation','public_boundary']:
        check=checks[name];log=Path(check['log'])
        if check['exit_code']!=0 or not log.is_file() or digest(log)!=check['sha256']:
            raise ValueError('missing or failed check: '+name)
    episodes=frenzy_execution_check(args.fixture_set/'meme-frenzy',args.smoke/smoke['scenarios'][2]['report'])
    root=args.out.resolve();root.mkdir(parents=True,exist_ok=False,mode=0o700)
    try:
        shutil.copytree(args.package/'tasks',root/'tasks')
        shutil.copytree(args.fixture_set,root/'fixtures')
        (root/'evidence').mkdir()
        write(root/'evidence/acceptance.json',{'scenarios':proof,'frenzy_reserve_checks':episodes,'checks':checks})
        for sid in ['scenario-01','scenario-02','scenario-03']:shutil.copy2(args.replays/(sid+'.json'),root/'evidence'/(sid+'-replay.json'))
        repo=Path(__file__).resolve().parents[2]
        names=subprocess.check_output(['git','ls-files','--cached','--others','--exclude-standard'],cwd=repo,text=True).splitlines()
        source_files={name:digest(repo/name) for name in sorted(set(names)) if (repo/name).is_file()}
        package.update(source_files=source_files,repository='https://github.com/LimeChain/machine-trading',
                       calibration_attempts={'smoke':smoke.get('attempts',{}),'noop':noop.get('attempts',{})},
                       hardware_fingerprint=hardware,coordinator_sha256=digest(Path(__file__).with_name('evaluate_set.py')),
                       acceptance_sha256=digest(root/'evidence/acceptance.json'))
        write(root/'release.json',package)
        for path in sorted(root.rglob('*'),reverse=True):path.chmod(0o500 if path.is_dir() else 0o400)
        root.chmod(0o500)
    except BaseException:
        (root/'FREEZE_FAILED').touch();raise
    print(json.dumps({'release':str(root),'fixture_set_hash':manifest['set_hash'],'aggregation':None}))


def main():
    p=argparse.ArgumentParser(description=__doc__)
    for name in ['fixture-set','package','smoke','noop','replays','checks','out']:p.add_argument('--'+name,type=Path,required=True)
    p.add_argument('--binary',type=Path,default=Path('target/debug/mth-backrun'))
    args=p.parse_args();os.umask(0o077);freeze(args)

if __name__=='__main__':main()
