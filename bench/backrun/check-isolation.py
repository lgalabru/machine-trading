#!/usr/bin/env python3
"""Maintainer host-side assertions against running Compose containers."""
import argparse, json, subprocess

parser = argparse.ArgumentParser()
parser.add_argument('--project', required=True)
parser.add_argument('--raw-port', type=int, default=8899)
parser.add_argument('--metadata-only', action='store_true', help='Do not create processes inside timed searcher containers')
args = parser.parse_args()
def docker(*arguments):
    return subprocess.check_output(['docker', *arguments], text=True)
def inspect(service):
    return json.loads(docker('inspect', f'{args.project}-{service}-1'))[0]
def cores(value):
    result=set()
    for group in value.split(','):
        bounds=group.split('-'); first=int(bounds[0]); last=int(bounds[-1])
        result.update(range(first,last+1))
    return result

main=inspect('main'); grader_cpus=cores(main['HostConfig']['CpusetCpus'])
seen=set(); networks=[]
for service in ['candidate','fast','broad','patient']:
    c=inspect(service); h=c['HostConfig']; limits=cores(h['CpusetCpus'])
    assert len(limits)==1 and not limits & (seen | grader_cpus), (service,'shared CPU')
    seen |= limits
    assert h['NanoCpus']==1_000_000_000 and h['Memory']==536870912 and h['MemorySwap']==536870912
    assert h['ReadonlyRootfs'] and not h['Privileged'] and 'ALL' in h['CapDrop']
    assert 'no-new-privileges:true' in h['SecurityOpt'] and c['Config']['User']=='1000:1000'
    mounts=c['Mounts']
    assert all(m['Destination'] in ['/input','/tmp'] for m in mounts)
    assert all(not m['RW'] for m in mounts if m['Destination']=='/input')
    network=list(c['NetworkSettings']['Networks'])
    assert len(network)==1 and network[0] not in networks
    networks.extend(network)
    settings = json.loads(docker('network','inspect',network[0]))[0]
    assert settings['Internal'] and not settings['EnableIPv6']
    assert settings['Options']['com.docker.network.bridge.gateway_mode_ipv4'] == 'isolated'
    assert network[0] in main['NetworkSettings']['Networks']
    # Check from the actual candidate namespace, without reading credentials.
    if not args.metadata_only:
        docker('exec',c['Id'],'python3','-c',r"""
import os, socket, sys
assert not any(os.path.exists(p) for p in ['/private','/logs/verifier','/var/run/docker.sock','/bootstrap'])
assert not os.access('/input',os.W_OK)
if sys.argv[1]=='candidate': assert not os.path.exists('/usr/local/bin/mth-backrun')
for path in ['/input/reward.txt','/logs/verifier/reward.txt','/usr/local/bin/mth-backrun']:
    try:
        with open(path,'wb') as f: f.write(b'tampered')
    except OSError: pass
    else: raise AssertionError('writable trusted path: '+path)
# The test companion listens only on grader loopback, like raw Surfpool RPC.
for host,port in [('main',int(sys.argv[2])),('1.1.1.1',443)]:
    try: connection=socket.create_connection((host,port),timeout=.3)
    except OSError: pass
    else:
        connection.close(); raise AssertionError('unexpected network access: '+host)
for host in ['example.com','candidate','fast','broad','patient']:
    if host==sys.argv[1]: continue
    try: socket.getaddrinfo(host,443)
    except OSError: pass
    else: raise AssertionError('unexpected DNS access: '+host)
""",service,str(args.raw_port))
print(json.dumps({'isolation':'passed','searcher_cpus':sorted(seen),'grader_cpus':sorted(grader_cpus),'memory_bytes_each':536870912}))
