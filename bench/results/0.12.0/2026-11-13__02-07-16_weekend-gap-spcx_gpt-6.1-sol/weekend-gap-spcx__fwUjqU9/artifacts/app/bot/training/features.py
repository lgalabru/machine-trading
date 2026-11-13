import math
import numpy as np
from collections import deque
LN2=math.log(2)
HALVES=[12,32,80,160]
AHALVES=[60,120,240]
NAMES=['t','remaining','fraction','spread','y','mom12','mom32','mom80','lag32','lag80','lag160','slope24','slope60','slope120','implied60','implied120','implied240','beta']
NAMES += ['flow'+str(h) for h in HALVES]+['absflow'+str(h) for h in HALVES]
NAMES += ['totalnet','totalqty','strategicnet','strategicqty']
ANAMES=['n','net','qty','imbalance','first','age','gap','gapcv','meanlimit','maxlimit','minlimit','high','low','decay60','decay120','decay240','lastsize','direction']
for i in range(3):NAMES += ['actor'+str(i)+'_'+n for n in ANAMES]
NAMES += ['resid'+str(i)+'_'+str(h) for i in range(4) for h in AHALVES]
NAMES += ['balanced24','balanced60','balanced120','balanced_implied60','balanced_implied120','balanced_implied240','beta_weight','availability']
class Actor:
 def __init__(self,t):
  self.n=0;self.net=0.;self.qty=0.;self.countnet=0;self.first=t;self.last=t;self.gapsum=0.;self.gap2=0.;self.limitsum=0.;self.maxlimit=0.;self.minlimit=10.;self.high=0;self.low=0;self.decay=np.zeros(3);self.lastsize=0
 def add(self,t,q,ratio):
  if self.n:
   gap=t-self.last;self.gapsum+=gap;self.gap2+=gap*gap
  self.n+=1;self.net+=q;self.qty+=abs(q);self.countnet+=1 if q>0 else -1;self.last=t
  self.limitsum+=ratio;self.maxlimit=max(self.maxlimit,ratio);self.minlimit=min(self.minlimit,ratio);self.high+=ratio>4.15;self.low+=ratio<2.7;self.decay+=q;self.lastsize=q
 def score(self):
  return self.n+3*self.high+max(0,self.maxlimit-3) if abs(self.countnet)==self.n and self.low==0 else -100+self.n*.01
 def features(self,t):
  n=self.n;gap=self.gapsum/max(1,n-1);cv=math.sqrt(max(0,self.gap2/max(1,n-1)-gap*gap))/max(1,gap)
  return [n,self.net,self.qty,self.countnet/n,self.first,t-self.last,gap,cv,self.limitsum/n,self.maxlimit,self.minlimit,self.high,self.low,*self.decay,self.lastsize,float(self.countnet>0)*2-1]
class State:
 def __init__(self,reopen,spread):
  self.reopen=reopen;self.spread=spread;self.t=0;self.y=None;self.ema=np.zeros(3);self.slope=np.zeros(3);self.flow=np.zeros(4);self.absflow=np.zeros(4);self.actors={};self.history=[];self.lastflow=-100.;self.pending=[];self.beta=.14;self.betaw=3.;self.slope_sum=np.zeros(3);self.slope_time=np.zeros(3)
 def update(self,t,y,flows):
  dt=t-self.t;self.t=t
  for a in self.actors.values():a.decay*=np.exp(-LN2*dt/np.array(AHALVES))
  self.flow*=np.exp(-LN2*dt/np.array(HALVES));self.absflow*=np.exp(-LN2*dt/np.array(HALVES))
  if self.y is None:self.y=y;self.ema[:]=y
  dy=y-self.y
  dd=np.exp(-LN2*dt/np.array([24,60,120]));self.slope_sum*=dd;self.slope_time*=dd
  if not flows and t-self.lastflow>4:
   self.slope_sum+=dy;self.slope_time+=dt
  if not flows and t-self.lastflow>6:
   alpha=1-np.exp(-LN2*dt/np.array([24,60,120]));self.slope+=(dy/max(1,dt)-self.slope)*alpha
  alpha=1-np.exp(-LN2*dt/np.array([12,32,80]));self.ema+=(y-self.ema)*alpha
  if flows:
   self.lastflow=t
   qsum=sum(q for _,q,_ in flows);qabs=sum(abs(q) for _,q,_ in flows)
   if abs(qsum)>0.3 and abs(qsum)>qabs*.9:self.pending.append((t,self.y,qsum))
  while self.pending and t-self.pending[0][0]>=4:
   tt,old,q=self.pending.pop(0)
   if self.lastflow==tt:
    b=np.clip((y-old)/q,.03,.30);w=min(2,abs(q));self.beta=(self.beta*self.betaw+b*w)/(self.betaw+w);self.betaw=min(40,self.betaw+w)
  for aid,q,ratio in flows:
   if aid not in self.actors:self.actors[aid]=Actor(t)
   self.actors[aid].add(t,q,ratio);self.flow+=q;self.absflow+=abs(q)
  self.history.append((t,y));self.y=y
 def features(self):
  t=self.t;y=self.y
  lag=[]
  for h in [32,80,160]:
   v=self.history[0][1]
   for tt,yy in reversed(self.history):
    if tt<=t-h:v=yy;break
   lag.append(y-v)
  actors=sorted(self.actors.values(),key=lambda a:a.score(),reverse=True)[:3]
  allactors=list(self.actors.values()); net=sum(a.net for a in allactors);qty=sum(a.qty for a in allactors)
  sn=sum(a.net for a in allactors if a.score()>=3);sq=sum(a.qty for a in allactors if a.score()>=3)
  f=[t,self.reopen-t,t/self.reopen,self.spread,y,*(y-self.ema),*lag,*self.slope,*(y+self.slope[1]*h/LN2 for h in AHALVES),self.beta,*self.flow,*self.absflow,net,qty,sn,sq]
  for i in range(3):f+=actors[i].features(t) if i<len(actors) else [0]*len(ANAMES)
  totaldecay=sum((a.decay for a in allactors),np.zeros(3))
  for i in range(4):
   for j,h in enumerate(AHALVES):
    pred=totaldecay[j]
    if i<3 and i<len(actors):pred+=actors[i].net-actors[i].decay[j]
    f.append(y-self.beta*pred)
  balanced=self.slope_sum/np.maximum(self.slope_time,1.)
  f.extend([*balanced,*(y+balanced[1]*h/LN2 for h in AHALVES),self.betaw,self.slope_time[1]])
  return f

def scenario_samples(d,rng,interval=6):
 o=d['offhours'];r=o['reopen_slot'];spread=o['half_spread_bps'];observed_spread=(round(1.4311*spread)+.5)/1.4311;s=State(r,observed_spread)
 path=d['path'];pidx=0;tidx=0;tak=o['takers'];y=0.;prev=0.;offset=rng.normal(0,.003);bias=rng.normal(0,.025)
 end=d['duration_slots'];samples=[];labels=[];times=[];ys=[]
 def price_at(t):
  # scenario levels are percentages; price is piecewise constant
  idx=np.searchsorted(pslots,t,side='right')-1
  return pvals[idx] if idx>=0 else 0.
 pslots=np.array([p['slot'] for p in path]);pvals=np.array([p['level_pct'] for p in path]);terminal=price_at(r+4)
 for t in range(2,r,2):
  while pidx<len(path) and path[pidx]['slot']<=t:y=path[pidx]['level_pct'];pidx+=1
  obs=round((143.11*(1+(y+offset)/100))/.01)*.01/143.11*100-100
  if pidx==0:obs=-.0419257913
  flows=[]
  while tidx<len(tak) and tak[tidx]['slot']<t:
   a=tak[tidx];q=a['size']*(1 if a['side']=='buy' else -1)
   # Quote distance is measured from previous book, with tick and centre noise.
   limit=round(143.11*(1+price_at(a['slot'])/100)*(1+(1 if q>0 else -1)*a['limit_bps']/10000)*100)/100
   ratio=(limit/(143.11*(1+(s.y if s.y is not None else 0.)/100))-1)*(1 if q>0 else -1)*10000/observed_spread+bias+rng.normal(0,.025)
   flows.append((a['account'],q,ratio));tidx+=1
  s.update(t,obs,flows)
  if t%interval==0 or flows:
   f=s.features();samples.append(f)
   labels.append([price_at(min(t+h,r+2))-y for h in [32,80,180]]+[terminal-y])
   times.append(t);ys.append(y)
 return np.array(samples,dtype=np.float32),np.array(labels,dtype=np.float32),np.array(times),np.array(ys),terminal
