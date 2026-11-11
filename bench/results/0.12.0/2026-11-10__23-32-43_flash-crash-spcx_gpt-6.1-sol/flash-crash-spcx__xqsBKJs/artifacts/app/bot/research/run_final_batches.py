"""Run isolated, small chain batches and combine their actual session reports."""
import subprocess,json,glob,os,time,statistics,math
from pathlib import Path
paths=sorted(glob.glob('/opt/mtx/practice/*.json'))+sorted(glob.glob('/tmp/holdchain/*.json'))+sorted(glob.glob('/tmp/stress/*.json'))
assert len(paths)==90
base=Path('/tmp/final-batches');base.mkdir(exist_ok=True)
processes=[]
for i in range(4):
 report=base/f'batch-{i}.json'
 args=['mtx','grade','--jobs','1','--snapshot',os.environ['MTX_SNAPSHOT'],'--bot','/tmp/bot-safe','--logs',str(base/f'logs-{i}'),'--report',str(report)]+paths[i::4]
 out=open(base/f'output-{i}.txt','w');err=open(base/f'errors-{i}.txt','w')
 p=subprocess.Popen(args,stdout=out,stderr=err)
 processes.append((p,report,out,err));print('started',i,'sessions',len(paths[i::4]),flush=True)
while any(p.poll() is None for p,_,_,_ in processes):time.sleep(1)
rows=[]
for p,report,out,err in processes:
 out.close();err.close()
 if p.returncode!=0:raise RuntimeError(f'batch {report} exited {p.returncode}')
 rows.extend(json.loads(report.read_text())['scenarios'])
assert len(rows)==90 and len({r['scenario'] for r in rows})==90
for r in rows:
 assert not r['disqualified'] and not r['refused_calls'] and not r['liquidatable']
 assert r['peak_position']<=70.0000001 and r['max_drawdown_usd']<=1000
pnl=sum(r['pnl_usd'] for r in rows);bound=sum(r['bound_pnl_usd'] for r in rows);pnls=[r['pnl_usd'] for r in rows]
result={'assembled_from':[str(report) for _,report,_,_ in processes],'pnl_usd':pnl,'bound_pnl_usd':bound,'reward':max(0,min(1,pnl/bound)),'risk':{'max_drawdown_usd':max(r['max_drawdown_usd'] for r in rows),'worst_pnl_usd':min(pnls),'profitable_share':sum(x>0 for x in pnls)/len(pnls)},'scenarios':rows}
Path('/tmp/safe-final.json').write_text(json.dumps(result,indent=2)+'\n')
print('completed',len(rows),'pnl',pnl,'maxdd',result['risk']['max_drawdown_usd'],flush=True)
