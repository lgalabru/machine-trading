"""Collect completed chain reports; never called by the trading executable."""
import json,hashlib,shutil
from pathlib import Path
ROOT=Path('/app/bot')
SOURCES={
 'final-chain':'/tmp/safe-final.json',
 'ensemble-validation':'/tmp/v6-val.json',
 'forecast-fresh':'/tmp/v4-hold.json',
 'forecast-validation':'/tmp/v4-val.json',
}
def summarize(rows):
 pnl=sum(r['pnl_usd'] for r in rows);bound=sum(r['bound_pnl_usd'] for r in rows)
 return {'sessions':len(rows),'pnl_usd':pnl,'bound_pnl_usd':bound,'reward':max(0,min(1,pnl/bound)) if bound else 0,'max_drawdown_usd':max((r['max_drawdown_usd'] for r in rows),default=0),'worst_pnl_usd':min((r['pnl_usd'] for r in rows),default=0),'profitable_sessions':sum(r['pnl_usd']>0 for r in rows),'disqualified':[r['scenario'] for r in rows if r.get('disqualified')],'refused_calls':sum(len(r['refused_calls']) for r in rows),'max_position':max((r['peak_position'] for r in rows),default=0),'late_slots':sum(r['late_slots'] for r in rows),'overran_turns':sum(r['turns']['overran'] for r in rows)}
if __name__=='__main__':
 dest=ROOT/'reports';dest.mkdir(exist_ok=True);summary={}
 for name,filename in SOURCES.items():
  path=Path(filename)
  if not path.exists():continue
  raw=json.loads(path.read_text());shutil.copyfile(path,dest/(name+'.json'))
  summary[name]={'all':summarize(raw['scenarios'])}
  for prefix in ['practice-','hold-','val-','stress-']:
   rows=[r for r in raw['scenarios'] if r['scenario'].startswith(prefix)]
   if rows:summary[name][prefix.rstrip('-')]=summarize(rows)
 summary['delivered_binary_sha256']=hashlib.sha256((ROOT/'bin/bot').read_bytes()).hexdigest()
 summary['note']='final-chain tests the final safety revision. ensemble-validation uses identical forecasts and policy with the preceding emergency-price limit. Stress scenarios are constructed tests and are reported separately from generated scenarios.'
 (dest/'summary.json').write_text(json.dumps(summary,indent=2)+'\n')
 if Path('/tmp/safe-tests.log').exists():shutil.copyfile('/tmp/safe-tests.log',dest/'unit-tests.txt')
 print(json.dumps(summary,indent=2))
