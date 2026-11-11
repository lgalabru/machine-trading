import sys,math,os,pickle,time,json,itertools
import numpy as np
from sklearn.ensemble import HistGradientBoostingRegressor
from sim import load
from native import Evaluator

def features(path):
 n=len(path);out=[];ids=[];dirs=[];last=anchor=extreme=path[0];d=0;event=lm=0;latest=0;lastjump=0.;noiseage=0;legs=0;lastprimary=0;legorigin=last
 for k,p in enumerate(path):
  change=p/last-1;windowchange=p/path[max(0,k-4)]-1
  if change:noiseage=k
  if d==0:
   if abs(change)>.002 or abs(windowchange)>.0035:
    if abs(change)>.002:d=1 if change>0 else -1;anchor=last
    else:d=1 if windowchange>0 else -1;anchor=path[max(0,k-4)]
    latest=d;extreme=p;event=lm=k;lastjump=change;legs=1;lastprimary=k;legorigin=anchor
  else:
   if abs(change)>.002:
    lm=k;latest=1 if change>0 else -1;lastjump=change
    if latest==d:
     if k-lastprimary>12:legs+=1;legorigin=last
     lastprimary=k
   if (p-extreme)*d>0:extreme=p
  if d:
   displacement=abs(extreme/anchor-1);recovery=(extreme-p)*d/anchor;age=k-lm
   f=[displacement*100,(p/anchor-1)*d*100,recovery/max(.001,displacement),age,k-event,lastjump*d*100,latest*d,k,n-k-1,k-noiseage]
   f += [(p/path[max(0,k-lag)]-1)*d*100 for lag in [1,2,3,4,6,10,16,32,64,100]]
   for window in [6,16,40,100]:
    a=path[max(0,k-window+1):k+1];lo=min(a);hi=max(a)
    f += [(hi-lo)/p*100,(p-lo)/(hi-lo) if hi>lo else .5]
    if d<0:f[-1]=1-f[-1]
   f += [(p/path[0]-1)*d*100,legs,k-lastprimary,(p/legorigin-1)*d*100,(p/path[max(0,k-4)]-1)*d*100]
   out.append(f);ids.append(k);dirs.append(d)
   if age>=3 and (recovery>max(.0014,displacement*.35) or age>100):d=0
  last=p
 return np.asarray(out,dtype=np.float32).reshape(-1,33),np.asarray(ids),np.asarray(dirs)

class Cache:
 def __init__(self,paths):
  self.paths=paths;xs=[];self.info=[];base=0
  for name,path in paths:
   x,ids,ds=features(path);self.info.append((base,len(x),ids,ds));base+=len(x);xs.append(x)
  self.x=np.concatenate(xs)
 def predict(self,models):
  vals=np.column_stack([m.predict(self.x) for m in models]);ps=[]
  if len(models)==1:vals=np.repeat(vals,3,axis=1)
  for (name,path),(base,n,ids,ds) in zip(self.paths,self.info):
   pr=np.zeros((len(path),3));signs=np.zeros(len(path));pr[ids]=vals[base:base+n]*ds[:,None];signs[ids]=ds;ps.append((name,np.asarray(path),pr,signs))
  return ps

def data(cache):
 ds=[];ns=[];ori=[];hs=[];targets=[]
 for (_,path),(base,n,ids,direction) in zip(cache.paths,cache.info):
  if not n:continue
  mp={int(k):base+i for i,k in enumerate(ids)};dx=np.diff(path,append=path[-1])/path[0]*100
  nxt=np.array([mp.get(int(k)+1,-1) for k in ids]);o=np.ones(n)
  for i,k in enumerate(ids):
   if nxt[i]>=0:o[i]=direction[nxt[i]-base]/direction[i]
  y=np.zeros(len(path));c=.10
  for k in range(len(path)-2,-1,-1):y[k]=dx[k]+np.clip(y[k+1],-c,c)
  ts=[]
  for tau in [4.,16.,64.]:
   f=np.zeros(len(path));decay=math.exp(-1/tau)
   for k in range(len(path)-2,-1,-1):f[k]=dx[k]+decay*f[k+1]
   ts.append(f[ids]*direction)
  ds.append(dx[ids]*direction);ns.append(nxt);ori.append(o);hs.append(y[ids]*direction);targets.append(np.array(ts).T)
 return cache.x,np.concatenate(ds),np.concatenate(ns),np.concatenate(ori),np.concatenate(hs),np.concatenate(targets)

def check(cache,models,label,iteration,kind):
 ps=cache.predict(models);e=Evaluator(ps);best=[]
 if kind=='q':parameters=itertools.product([.02,.04,.06,.08,.1,.12,.15,.2],[-.15,-.1,-.06,-.03,0,.03])
 else:parameters=itertools.product([.03,.05,.08,.12,.2],[.1,.2,.4,.8],[-.15,-.08,-.03,0],[1,2])
 for params in parameters:
  if kind=='q':f,ex=params;r=e(f,1000,ex,1,cost=.001)
  else:f,s,ex,si=params;r=e(f,s,ex,si,cost=.001)
  best.append((r,params))
 print(kind,iteration,label,sorted(best,reverse=True)[:5],flush=True)
 pickle.dump(ps,open('/tmp/large-pred-'+kind+'-'+str(iteration)+'-'+label+'.pkl','wb'))

if __name__=='__main__':
 started=time.time()
 if os.path.exists('/tmp/large-data.pkl'):
  cache,x,delta,nxt,ori,target,forecast=pickle.load(open('/tmp/large-data.pkl','rb'))
 else:
  cache=Cache(load('/tmp/bigtrain/*.json'));x,delta,nxt,ori,target,forecast=data(cache)
  pickle.dump((cache,x,delta,nxt,ori,target,forecast),open('/tmp/large-data.pkl','wb'))
 print('data',x.shape,'seconds',time.time()-started,flush=True)
 tests={label:Cache(load(pat)) for label,pat in [('train','/tmp/train/*.json'),('validation','/tmp/validate/*.json'),('test','/tmp/mltest/*.json'),('practice','/opt/mtx/practice/*.json'),('holdout','/tmp/bigtest/*.json')]}
 models=[]
 for i in range(3):
  m=HistGradientBoostingRegressor(max_iter=230,max_leaf_nodes=31,min_samples_leaf=600,learning_rate=.08,l2_regularization=5,early_stopping=False,random_state=947)
  m.fit(x,forecast[:,i]);models.append(m);print('forecast fit',i,flush=True)
 pickle.dump(models,open('/tmp/large-forecast.pkl','wb'))
 for label,t in tests.items():check(t,models,label,0,'forecast')
 valid=nxt>=0
 for it in range(25):
  m=HistGradientBoostingRegressor(max_iter=130,max_leaf_nodes=31,min_samples_leaf=650,learning_rate=.1,l2_regularization=3,early_stopping=False,random_state=47)
  m.fit(x,target)
  pickle.dump(m,open('/tmp/large-q-'+str(it)+'.pkl','wb'))
  if it in [0,1,3,6,10,15,20,24]:
   for label,t in tests.items():check(t,[m],label,it,'q')
  print('q fit',it,'elapsed',time.time()-started,flush=True)
  pred=m.predict(x);target=delta.copy();target[valid]+=np.clip(pred[nxt[valid]]*ori[valid],-.10,.10)
