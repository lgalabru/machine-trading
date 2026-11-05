#!/usr/bin/env python3
"""Build private per-scenario verifier images and public development image; never push."""
import argparse
from pathlib import Path
import json
import os
import shutil
import subprocess
from fixture_set import read,write,digest,verify


def image_id(image):
    return json.loads(subprocess.check_output(['docker','image','inspect',image],text=True))[0]['Id']


def build_base(identifier):
    # BuildKit treats a bare sha256 ID in FROM as a registry repository.
    # Give the already-resolved local image a content-addressed build tag;
    # runnable tasks still use the immutable image ID.
    tag='mth-backrun-base:'+identifier.split(':')[1]
    subprocess.run(['docker','tag',identifier,tag],check=True)
    if image_id(tag)!=identifier:raise ValueError('base image changed')
    return tag


def package(args):
    repo=Path(__file__).resolve().parents[2];root=args.out.resolve()
    if root.exists(): raise FileExistsError('package already exists')
    fixture_set=verify(args.fixture_set,args.binary)
    candidate = read(args.base_release/'release.json')['images']['mth-backrun-candidate:local'] if args.base_release else args.candidate
    root.mkdir(parents=True,mode=0o700)
    images={'runtime':image_id(args.runtime),'toolchain':image_id(args.toolchain),
            'candidate':image_id(candidate)}
    programs=args.programs.resolve(); binary=args.binary.resolve()
    subprocess.run([str(binary),'fixture','public','--fixture',str(args.fixture_set/'incentivized'),'--out',str(root/'public')],check=True,capture_output=True)
    write(root/'public/fixture.json',{'fixture_set_hash':fixture_set['set_hash']})
    dev=root/'development';dev.mkdir();shutil.copytree(root/'public',dev/'public');(dev/'programs').mkdir()
    for name in ['raydium_cp_swap.so','whirlpool.so','mth_backrun_executor.so']:shutil.copy2(programs/name,dev/'programs'/name)
    (dev/'Dockerfile').write_text(f"FROM {build_base(images['toolchain'])}\nCOPY public /opt/backrun/public\nCOPY programs /opt/backrun/binaries\nRUN chmod -R a+rX /opt/backrun\n")
    subprocess.run(['docker','build','--iidfile',str(root/'development.id'),str(dev)],check=True)
    images['development']=(root/'development.id').read_text().strip()
    scenarios=[]
    for scenario in fixture_set['scenarios']:
        sid=scenario['id'];context=root/'contexts'/sid;context.mkdir(parents=True)
        shutil.copytree(args.fixture_set/scenario['path'],context/'fixture')
        (context/'programs').mkdir()
        for p in programs.iterdir():
            if p.suffix=='.so' or p.name=='manifest.json':shutil.copy2(p,context/'programs'/p.name)
        shutil.copy2(repo/'bench/tasks/atomic-arb/tests/test.sh',context/'test.sh')
        (context/'Dockerfile').write_text(f"FROM {build_base(images['runtime'])}\nCOPY fixture /private/fixture\nCOPY programs /private/programs\nCOPY test.sh /tests/test.sh\nRUN chmod -R go-rwx /private\n")
        identifier=root/(sid+'.id')
        subprocess.run(['docker','build','--iidfile',str(identifier),str(context)],check=True)
        images[sid]=identifier.read_text().strip()
        task=root/'tasks'/sid
        shutil.copytree(repo/'bench/tasks/atomic-arb',task,ignore=shutil.ignore_patterns('artifact','__pycache__'))
        mapping={'mth-backrun-runtime:local':images['runtime'],'mth-backrun-candidate:local':images['candidate'],
                 'mth-backrun-verifier:local':images[sid],'mth-backrun-development:local':images['development']}
        for name in ['tests/docker-compose.yaml','task.toml','environment/Dockerfile']:
            p=task/name;text=p.read_text()
            for old,new in mapping.items():text=text.replace(old,new)
            if name=='task.toml':text=text.replace('[environment]\n','[environment]\ndocker_image = '+json.dumps(images['development'])+'\n')
            p.write_text(text)
        subprocess.run(['harbor','lint',str(task)],check=True)
        scenarios.append({**{k:scenario[k] for k in ['id','regime','fixture_hash','flow_sha256']},'task':'tasks/'+sid})
    runtime_hash=subprocess.check_output(['docker','run','--rm',images['runtime'],'sha256sum','/usr/local/bin/mth-backrun'],text=True).split()[0]
    meta={'version':4,'kind':'private-scenario-set','protocol':'continuous-arrival-v1','aggregation':None,
          'fixture_set_hash':fixture_set['set_hash'],'source_fixture_hash':fixture_set['source_fixture_hash'],
          'runtime_binary_sha256':runtime_hash,'images':images,'scenarios':scenarios,
          'task_files':{str(p.relative_to(root)):digest(p) for p in sorted((root/'tasks').rglob('*')) if p.is_file()},
          'generation_manifest_sha256':digest(args.fixture_set/'set.json'),
          'policy':'one development attempt, one identical artifact, one run per private scenario; no best-of repeats'}
    write(root/'package.json',meta)
    print(json.dumps({'package':str(root),'fixture_set_hash':fixture_set['set_hash']}))


def main():
    p=argparse.ArgumentParser(description=__doc__)
    for name in ['fixture-set','programs','out']:p.add_argument('--'+name,type=Path,required=True)
    p.add_argument('--base-release',type=Path,help='Reuse the immutable candidate image from an earlier release')
    p.add_argument('--candidate',default='mth-backrun-candidate:scenario-set-v1')
    p.add_argument('--binary',type=Path,default=Path('target/debug/mth-backrun'))
    p.add_argument('--runtime',default='mth-backrun-runtime:scenario-set-v1')
    p.add_argument('--toolchain',default='mth-backrun-toolchain:scenario-set-v1')
    args=p.parse_args();os.umask(0o077);package(args)

if __name__=='__main__':main()
