#!/usr/bin/python3
"""Runs only inside an isolated, unprivileged searcher container."""
import json, os, pathlib, subprocess, time, urllib.request

path = pathlib.Path('/input/config.json')
while not path.exists():
    time.sleep(.05)
config = json.loads(path.read_text())
os.environ['MTH_BACKRUN_GATEWAY'] = config['gateway']
os.environ['MTH_BACKRUN_TOKEN'] = config['token']
if config['participant'] == 0:
    os.chdir('/input/artifact')
    os.environ['MTH_BACKRUN_ARTIFACT'] = '/input/artifact'
    # Keep PID 1 alive if the candidate exits. Its remaining opportunities are
    # missed; it cannot erase a trading loss by crashing its container.
    result = subprocess.run(['/bin/bash', './run.sh'], check=False)
    print(json.dumps({'candidate_exit_code': result.returncode}), flush=True)
    while True:
        time.sleep(3600)
else:
    result = subprocess.run(['mth-backrun', 'reference', '--gateway', config['gateway'], '--strategy', str(config['participant'] - 1)], check=False)
    request=urllib.request.Request(config["gateway"]+"/exit", data=json.dumps({"exit_code":result.returncode}).encode(), headers={"Content-Type":"application/json","Authorization":"Bearer "+config["token"]})
    try: urllib.request.urlopen(request, timeout=3).close()
    except OSError: pass
    # An unexpected trusted competitor exit is an infrastructure failure.
    raise SystemExit(result.returncode or 1)
