import sys,os,math,glob,json,pickle,time
sys.path.insert(0,'/app/bot/research')
import numpy as np
from sklearn.ensemble import HistGradientBoostingRegressor
from sim import load

LAGS=[1,2,3,4,6,10,16,32,64,100]

def features(path):
 n=len(path); out=[]; ids=[];dirs=[]
 last=anchor=extreme=path[0];d=0;event=lm=0;latest=0;lastjump=0.;noiseage=0
 for k,p in enumerate(path):
  change=p/last-1
  if change:noiseage=k
  if d==0:
   if abs(change)>.002:
    d=1 if change>0 else -1;latest=d;anchor=last;extreme=p;event=lm=k;lastjump=change
  else:
   if abs(change)>.002:lm=k;latest=1 if change>0 else -1;lastjump=change
   if (p-extreme)*d>0:extreme=p
  if d:
   displacement=abs(extreme/anchor-1);recovery=(extreme-p)*d/anchor
   age=k-lm
   f=[displacement*100,(p/anchor-1)*d*100,recovery/max(.001,displacement),age,k-event,lastjump*d*100,latest*d,k, n-k-1,k-noiseage]
   f += [(p/path[max(0,k-lag)]-1)*d*100 for lag in LAGS]
   for window in [6,16,40,100]:
    a=path[max(0,k-window+1):k+1];lo=min(a);hi=max(a)
    f += [(hi-lo)/p*100, (p-lo)/(hi-lo) if hi>lo else .5]
    if d<0:f[-1]=1-f[-1]
   f += [(p/path[0]-1)*d*100]
   out.append(f);ids.append(k);dirs.append(d)
   if age>=3 and (recovery>max(.0014,displacement*.35) or k-event>100):d=0
  last=p
 return np.asarray(out,dtype=np.float32).reshape(-1,29),np.asarray(ids),np.asarray(dirs)

def dataset(paths):
 xs=[];ys=[]
 for name,path in paths:
  x,ids,ds=features(path)
  if not len(ids):continue
  a=np.asarray(path);delta=np.diff(a,append=a[-1])/a[0]*100
  targets=[]
  for tau in [4.,16.,64.]:
   y=np.zeros(len(a));decay=math.exp(-1/tau)
   for k in range(len(a)-2,-1,-1):y[k]=delta[k]+decay*y[k+1]
   targets.append(y[ids]*ds)
  xs.append(x);ys.append(np.array(targets).T)
 return np.concatenate(xs),np.concatenate(ys)

def predict_paths(paths,models):
 results=[]
 for name,path in paths:
  x,ids,ds=features(path)
  preds=np.zeros((len(path),3));signs=np.zeros(len(path))
  if len(ids):
   vals=np.array([m.predict(x) for m in models]).T
   preds[ids]=vals*ds[:,None];signs[ids]=ds
  results.append((name,np.asarray(path),preds,signs))
 return results

def evaluate(predictions,fast_threshold=.08,slow_threshold=.12,exit_threshold=.02,fast_weight=1.,slow_index=2,stop=500,cost=.00065):
 out=[]
 for name,path,preds,signs in predictions:
  pos=0; pnl=peak=dd=biggest=0.;halted=False;turnover=0
  for k,p in enumerate(path):
   if k:pnl+=pos*(p-path[k-1])
   peak=max(peak,pnl);dd=peak-pnl;biggest=max(biggest,dd)
   if dd>stop:halted=True
   a,b,c=preds[k];slow=preds[k,slow_index]
   # short horizon forecasts override delayed reversion, only when strong enough
   target=0
   if abs(a)>fast_threshold:target=70*np.sign(a)
   elif abs(slow)>slow_threshold:target=70*np.sign(slow)
   elif signs[k] and pos and pos*slow>exit_threshold*70:target=pos
   safe=min(70,max(0,(1000-dd-140)/(p*.045)))
   target=math.trunc(max(-safe,min(safe,target)))
   if halted or k==len(path)-1:target=0
   if abs(target-pos)>=1:
    pnl-=abs(target-pos)*p*cost;turnover+=abs(target-pos);pos=target
  out.append((name,pnl,biggest,turnover))
 return sum(r[1] for r in out),max(r[2] for r in out),min(r[1] for r in out),sum(r[3] for r in out),out

if __name__=='__main__':
 if '--train' in sys.argv:
  paths=load('/tmp/mltrain/*.json');print('loaded',len(paths),flush=True)
  x,y=dataset(paths);print('dataset',x.shape,flush=True)
  models=[]
  for i in range(3):
   m=HistGradientBoostingRegressor(max_iter=180,max_leaf_nodes=25,min_samples_leaf=160,learning_rate=.08,l2_regularization=2,early_stopping=True,random_state=41)
   m.fit(x,y[:,i]);models.append(m);print('fit',i,m.n_iter_,flush=True)
  pickle.dump(models,open('/tmp/models.pkl','wb'))
 else:models=pickle.load(open('/tmp/models.pkl','rb'))
 for label,pat in [('train','/tmp/train/*.json'),('validation','/tmp/validate/*.json'),('test','/tmp/mltest/*.json'),('practice','/opt/mtx/practice/*.json')]:
  ps=predict_paths(load(pat),models);pickle.dump(ps,open('/tmp/pred-'+label+'.pkl','wb'))
  print(label,evaluate(ps)[:4],flush=True)
  best=[]
  for fast in [.03,.05,.08,.12,.2,.4,1.]:
   for slow in [.03,.06,.1,.15,.25,.4]:
    for ex in [-.03,0,.03]:
     for si in [1,2]:
      r=evaluate(ps,fast,slow,ex,slow_index=si)
      best.append((r[:4],fast,slow,ex,si))
  for r in sorted(best,reverse=True)[:10]:print(label,r,flush=True)
