"""Research-only price-path simulator. Not used by the deployed executable."""
import json,glob,math,sys,random

def load(pattern):
 out=[]
 for f in glob.glob(pattern):
  j=json.load(open(f)); events=iter(j['path']); nex=next(events,None); price=143.05; path=[]
  for slot in range(2,j['duration_slots']+2,2):
   while nex and nex['slot']<=slot:
    price=round(143.11*(1+nex['level_pct']/100),2); nex=next(events,None)
   path.append(price)
  out.append((j['name'],path))
 return out

def sim(path,cfg,trace=False):
 # persistent shock, active directional momentum, otherwise fade back to its origin
 threshold, wait, complete, expiry, fade, mom, recover_wait, stop, cost=cfg
 last=anchor=extreme=path[0]; direction=0; event=lastmove=0; latest=0; pos=0.; pnl=peak=0.; dd=0.; turnover=0.; halted=False; biggest=0.
 log=[]
 for k,p in enumerate(path):
  pnl+=pos*(p-last); peak=max(peak,pnl); dd=peak-pnl;biggest=max(biggest,dd)
  if dd>stop:halted=True
  change=p/last-1
  if direction==0:
   if abs(change)>threshold:
    direction=1 if change>0 else -1; anchor=last; extreme=p;event=k;lastmove=k;latest=direction
  else:
   if abs(change)>threshold:
    lastmove=k; latest=1 if change>0 else -1
   if (p-extreme)*direction>0:extreme=p
  target=0
  if direction:
   displacement=abs(extreme/anchor-1)
   recovery=(extreme-p)*direction/anchor
   age=k-lastmove
   if age<wait:
    target=latest*mom
   elif recovery>max(threshold*0.7, displacement*complete) and age>=recover_wait:
    direction=0
   elif k-event>expiry:
    direction=0
   else:
    target=-direction*fade
  safe=min(70,max(0,(1000-dd-140)/(p*.045)))
  target=math.trunc(max(-safe,min(safe,target)))
  if halted or k==len(path)-1:target=0
  if abs(target-pos)>=1:
   pnl-=abs(target-pos)*p*cost;turnover+=abs(target-pos)
   if trace:log.append((k,round(p,2),pos,target,round(pnl,2),direction))
   pos=target
  last=p
 return pnl,biggest,turnover,log

def score(paths,cfg):
 r=[sim(p,cfg) for _,p in paths]
 return sum(x[0] for x in r),max(x[1] for x in r),sum(x[2] for x in r),min(x[0] for x in r)

if __name__=='__main__':
 paths=load(sys.argv[1] if len(sys.argv)>1 else '/tmp/train/*.json')
 cfg=(.002,4,.30,180,70,70,5,500,.00065)
 print(cfg,score(paths,cfg))
 if '--sweep' in sys.argv:
  rng=random.Random(91); best=[]
  for i in range(1800):
   cfg=(rng.choice([.0015,.002,.0025,.003,.004]),rng.choice([1,2,3,4,5,6]),rng.choice([.15,.25,.35,.5,.7]),rng.choice([70,100,140,180,240]),rng.choice([40,55,70]),rng.choice([0,35,70]),rng.choice([3,4,5,7,9]),500,.00065)
   s=score(paths,cfg);best.append((s,cfg))
  for row in sorted(best,reverse=True)[:30]:print(row)
