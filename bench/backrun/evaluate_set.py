#!/usr/bin/env python3
"""One development attempt, then three fresh isolated Harbor evaluations.

Later Harbor 'oracle' stages only copy the original compiled artifact; they do
not run a model, build code, or execute the bot on the host/trusted grader.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess


def read(path): return json.loads(Path(path).read_text())
def digest(path): return hashlib.sha256(Path(path).read_bytes()).hexdigest()
def write(path, data): Path(path).write_text(json.dumps(data, indent=2) + '\n')


def artifact_files(root):
    files = {}
    for path in sorted(root.rglob('*')):
        if path.is_symlink(): raise ValueError('symlink in submitted artifact')
        if path.is_file(): files[str(path.relative_to(root))] = digest(path)
    if not all(name in files for name in ['run.sh','manifest.json']):
        raise ValueError('incomplete submitted artifact')
    return files


def verify_release(root, calibration=False):
    marker = 'package.json' if calibration else 'release.json'
    if (root/'FREEZE_FAILED').exists(): raise ValueError('release failed acceptance')
    meta = read(root/marker)
    if meta['kind'] != 'private-scenario-set' or meta['protocol'] != 'continuous-arrival-v1' or meta['aggregation'] is not None:
        raise ValueError('unsupported scenario-set contract')
    if not calibration and meta.get('coordinator_sha256') != digest(__file__):
        raise ValueError('evaluation coordinator differs from frozen release')
    if len(meta['scenarios']) != 3 or len({s['id'] for s in meta['scenarios']}) != 3:
        raise ValueError('expected exactly three distinct scenarios')
    for name, sha in meta['task_files'].items():
        path=Path(name)
        if path.is_absolute() or '..' in path.parts or any((root/Path(*path.parts[:i])).is_symlink() for i in range(1,len(path.parts)+1)):
            raise ValueError('unsafe release task path')
        if digest(root/path) != sha: raise ValueError('frozen task changed: '+name)
    for scenario in meta['scenarios']:
        if scenario['task'] != 'tasks/'+scenario['id'] or scenario['id'] not in ['scenario-01','scenario-02','scenario-03']:
            raise ValueError('unsafe scenario task')
    for image in set(meta['images'].values()):
        if not image.startswith('sha256:'): raise ValueError('mutable image reference')
        subprocess.run(['docker','image','inspect',image],check=True,stdout=subprocess.DEVNULL)
    return meta


def harbor_command(task, jobs, name, agent, model=None, effort='high'):
    cmd=['harbor','run','--path',str(task),'--agent',agent,'--n-concurrent','1',
         '--n-attempts','1','--max-retries','0','--jobs-dir',str(jobs),'--job-name',name]
    if model:cmd += ['--model',model]
    if agent != 'oracle':cmd += ['--effort',effort]
    if agent == 'codex':cmd += ['--agent-kwarg','version=0.166.0','--agent-kwarg','config={}']
    return cmd


def evaluate(args):
    release=args.release.resolve();meta=verify_release(release,args.calibration)
    if args.calibration and args.artifact is None: raise ValueError('calibration requires a prebuilt artifact, never a model')
    if args.agent=='oracle' and args.artifact is None: raise ValueError('copy-only evaluation requires --artifact')
    root=(args.jobs_dir/args.job_name).resolve()
    resume=getattr(args,'resume',False)
    retry=getattr(args,'retry_infrastructure',False)
    if retry and not resume:raise ValueError('--retry-infrastructure requires --resume')
    previous=None
    if root.exists():
        if not resume:raise FileExistsError('job exists; preserve results and use a fresh job name')
        previous=read(root/'report.json')
        if previous['status']=='disqualified':raise ValueError('a disqualified artifact cannot be retried')
        if previous['fixture_set_hash']!=meta['fixture_set_hash']:raise ValueError('cannot resume with a different fixture set')
        if previous.get('model')!=(args.model if args.artifact is None else None):raise ValueError('cannot change model when resuming')
    elif resume:raise FileNotFoundError('no job to resume')
    if args.dry_run:
        print(json.dumps({'release':str(release),'job':str(root),'development_attempts':0 if args.artifact else 1,
                          'scored_sessions':3,'aggregation':None,'model':args.model,'fresh_containers_per_session':True},indent=2));return
    root.mkdir(parents=True,mode=0o700,exist_ok=resume)
    env=os.environ.copy()
    if args.agent=='codex' and args.artifact is None:
        auth=Path(env.get('CODEX_AUTH_JSON_PATH',Path.home()/'.codex/auth.json'))
        if not auth.is_file(): raise ValueError('Codex authentication missing')
        env['CODEX_AUTH_JSON_PATH']=str(auth.resolve())
    report={'version':1,'fixture_set_hash':meta['fixture_set_hash'],'protocol':meta['protocol'],
            'status':'running','aggregation':None,'reward_by_scenario':{},'scenarios':[],
            'development_attempts':0 if args.artifact else 1,'model':args.model if args.artifact is None else None,'attempts':{}}
    artifact=None; hashes=None; artifact_hash=None; hardware=None
    if (root/'submitted-artifact').exists():
        artifact=root/'submitted-artifact';hashes=artifact_files(artifact)
        if previous and previous.get('artifact_files') and hashes!=previous['artifact_files']:
            raise ValueError('submitted artifact changed since previous run')
        if args.artifact and hashes!=artifact_files(args.artifact):raise ValueError('cannot replace artifact while resuming')
    elif args.artifact:
        hashes=artifact_files(args.artifact);artifact=root/'submitted-artifact'
        shutil.copytree(args.artifact,artifact)
    if previous:
        backups=list(root.glob('report-before-resume-*.json'))
        write(root/f'report-before-resume-{len(backups)+1:02}.json',previous)
    write(root/'report.json',report)
    try:
        for index, scenario in enumerate(meta['scenarios']):
            # Completed sessions are reused; an explicit recovery can only
            # retry a trusted infrastructure failure which produced no score.
            sid=scenario['id']
            attempts=sorted(p for p in (root/'harbor').glob(sid+'*') if p.is_dir()) if (root/'harbor').exists() else []
            completed=[]
            for attempt in attempts:
                paths=list(attempt.glob('*/verifier/report.json'))
                if len(paths)==1 and read(paths[0]).get('valid'):completed.append(attempt)
            if len(completed)>1:raise ValueError('multiple scored runs exist; best-of selection is forbidden')
            cached=bool(completed)
            job=completed[0] if cached else root/'harbor'/sid
            if attempts and not cached:
                last=attempts[-1]
                errors=list(last.glob('*/verifier/infrastructure-error.json'))
                copy_errors=[p for p in last.glob('*/agent/exit-code.txt') if p.read_text().strip()!='0']
                if not (errors or copy_errors):
                    raise RuntimeError('existing trial has no recoverable infrastructure marker; inspect retained evidence')
                if not retry:raise RuntimeError('infrastructure failure retained; explicit --resume --retry-infrastructure required')
                if artifact is None:raise RuntimeError('no frozen artifact to recover; development is never repeated')
                job=root/'harbor'/f'{sid}-infra-retry-{len(attempts):02}'
            report['attempts'][sid]=[str(p.relative_to(root)) for p in attempts]
            if not cached:
                task=root/'tasks'/job.name
                if task.exists():raise FileExistsError('staged task exists without a recoverable trial')
                shutil.copytree(release/scenario['task'],task)
                for p in task.rglob('*'):
                    p.chmod(0o700 if p.is_dir() else (0o700 if p.suffix=='.sh' else 0o600))
                task.chmod(0o700)
                agent=args.agent if artifact is None else 'oracle'
                if artifact is not None:
                    if artifact_files(artifact)!=hashes:raise ValueError('submitted artifact changed')
                    solution=task/'solution';shutil.rmtree(solution,ignore_errors=True);solution.mkdir()
                    shutil.copytree(artifact,solution/'artifact')
                    (solution/'solve.sh').write_text('#!/bin/bash\nset -euo pipefail\nmkdir -p /app/bot\ncp -a /solution/artifact/. /app/bot/\n')
                    for entry in [solution, *solution.rglob('*')]:
                        entry.chmod(0o755 if entry.is_dir() or entry.stat().st_mode & 0o111 else 0o644)
                    (solution/'solve.sh').chmod(0o755)
                else:
                    shutil.rmtree(task/'solution',ignore_errors=True)
                cmd=harbor_command(task,root/'harbor',job.name,agent,args.model if agent!='oracle' else None,args.effort)
                print(f"{sid}: {'development + evaluation' if artifact is None else 'same artifact, fresh evaluation'}",flush=True)
                report['attempts'][sid].append(str(job.relative_to(root)));write(root/'report.json',report)
                with (root/(job.name+'.log')).open('w') as log:
                    try:
                        subprocess.run(cmd,env=env,stdout=log,stderr=subprocess.STDOUT,check=True)
                    finally:
                        # Harbor can finish development before the first
                        # evaluation fails. Preserve that exact submission for
                        # an explicit infrastructure-only recovery.
                        if artifact is None and list(job.glob('*/verifier/infrastructure-error.json')):
                            artifacts=list(job.glob('*/artifacts/app/bot'))
                            if len(artifacts)==1:
                                hashes=artifact_files(artifacts[0]);artifact=root/'submitted-artifact'
                                shutil.copytree(artifacts[0],artifact)
                                report['artifact_files']=hashes
                if agent=='oracle':
                    exits=list(job.glob('*/agent/exit-code.txt'))
                    if any(p.read_text().strip()!='0' for p in exits):
                        raise RuntimeError('trusted artifact-copy stage failed; this is infrastructure, not a trading result')
            else:
                print(f'{sid}: reusing its completed scored run; no repeat',flush=True)
            results=list(job.glob('*/verifier/report.json'))
            if len(results)!=1:
                errors=list(job.glob('*/verifier/infrastructure-error.json'))
                if len(errors)==1:raise RuntimeError(f"{sid}: {read(errors[0])['error']}; no trading score")
                raise RuntimeError('missing verifier report; inspect retained Harbor logs')
            result=read(results[0])
            if result.get('fixture_hash')!=scenario['fixture_hash']:raise ValueError('wrong fixture evaluated')
            if not result.get('valid'):
                reason=result.get('terminal_disqualification')
                if reason:
                    report['status']='disqualified';report['disqualification']=reason
                    report['scenarios'].append({'id':scenario['id'],'status':'disqualified','report':str(results[0].relative_to(root))})
                    report['not_run']=[s['id'] for s in meta['scenarios'][index+1:]]
                    write(root/'report.json',report)
                    print(json.dumps({'report':str(root/'report.json'),'status':'disqualified','reason':reason}));return
                raise RuntimeError('invalid verifier report')
            if result.get('completed_slots')!=2000 or result['timing']['coalesced_slots']!=0:
                raise RuntimeError('incomplete scenario')
            if artifact is None:
                artifacts=list(job.glob('*/artifacts/app/bot'))
                if len(artifacts)!=1:raise RuntimeError('expected exactly one submitted artifact')
                hashes=artifact_files(artifacts[0]);artifact=root/'submitted-artifact';shutil.copytree(artifacts[0],artifact)
            if index==0:
                artifact_hash=result['artifact_hash'];hardware=result['hardware']['fingerprint']
                report['artifact_hash']=artifact_hash;report['artifact_files']=hashes
            if result['artifact_hash']!=artifact_hash or result['hardware']['fingerprint']!=hardware:
                raise ValueError('artifact or hardware changed between scenarios')
            if result['runtime_binary_sha256']!=meta['runtime_binary_sha256']:
                raise ValueError('runtime changed')
            candidate=result['participants'][0];profit=candidate['score']['reward']
            report['reward_by_scenario'][scenario['id']]=profit
            report['scenarios'].append({'id':scenario['id'],'regime':scenario['regime'],'fixture_hash':scenario['fixture_hash'],
                'net_profit_sol':profit,'candidate':candidate,'participants':result['participants'],
                'timing':{k:v for k,v in result['timing'].items() if k!='slots'},
                'user_executions':result['user_executions'],'user_failures':result['user_failures'],
                'report':str(results[0].relative_to(root))})
            write(root/'report.json',report)
            print(f"{scenario['id']}: net profit {profit:+.9f} SOL",flush=True)
        report['status']='complete';write(root/'report.json',report)
        print(json.dumps({'report':str(root/'report.json'),'reward_by_scenario':report['reward_by_scenario'],'aggregation':None},indent=2))
    except BaseException as error:
        report['status']='incomplete';report['error']=str(error);write(root/'report.json',report)
        raise


def main():
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('--release',type=Path,required=True);p.add_argument('--jobs-dir',type=Path,default=Path('jobs'))
    p.add_argument('--job-name',required=True);p.add_argument('--agent',default='codex');p.add_argument('--model')
    p.add_argument('--effort',default='high');p.add_argument('--dry-run',action='store_true')
    p.add_argument('--artifact',type=Path,help='Evaluate an existing compiled artifact; no model development')
    p.add_argument('--resume',action='store_true',help='Reuse completed sessions from this same job')
    p.add_argument('--retry-infrastructure',action='store_true',help='Explicitly retry only a recorded infrastructure failure; never a scored session')
    p.add_argument('--calibration',action='store_true',help='Maintainer validation of an unfrozen package; requires --artifact')
    args=p.parse_args();os.umask(0o077);evaluate(args)

if __name__=='__main__':main()
