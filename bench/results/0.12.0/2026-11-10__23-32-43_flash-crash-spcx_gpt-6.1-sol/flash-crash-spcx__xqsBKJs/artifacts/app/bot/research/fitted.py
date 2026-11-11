import os,sys,pickle,math,time,itertools
from learn import features,predict_paths
from sim import load
from native import Evaluator
import numpy as np
from sklearn.ensemble import HistGradientBoostingRegressor

def make_data(paths):
 xs=[];deltas=[];nexts=[];orientations=[];oracle=[];base=0
 for _,path in paths:
  x,ids,ds=features(path)
  if not len(ids):continue
  mp={int(k):base+i for i,k in enumerate(ids)}
  dx=np.diff(path,append=path[-1])/path[0]*100
  nxt=np.array([mp.get(int(k)+1,-1) for k in ids])
  ori=np.ones(len(ids))
  for i,k in enumerate(ids):
   if nxt[i]>=0:ori[i]=ds[nxt[i]-base]/ds[i]
  y=np.zeros(len(path));c=.065
  for k in range(len(path)-2,-1,-1):y[k]=dx[k]+np.clip(y[k+1],-c,c)
  xs.append(x);deltas.append(dx[ids]*ds);nexts.append(nxt);orientations.append(ori);oracle.append(y[ids]*ds);base+=len(ids)
 return np.concatenate(xs),np.concatenate(deltas),np.concatenate(nexts),np.concatenate(orientations),np.concatenate(oracle)

def eval_model(model,label,paths):
 ps=predict_paths(paths,[model,model,model]);e=Evaluator(ps);best=[]
 for ft,ex in itertools.product([.005,.01,.02,.03,.04,.05,.06,.08,.1,.15],[-.1,-.06,-.04,-.02,0,.02]):
  r=e(ft,1000,ex,1);best.append((r,(ft,ex)))
 print(label,sorted(best,reverse=True)[:4],flush=True)
 pickle.dump(ps,open('/tmp/fitted-pred-'+label+'.pkl','wb'))
 return best

if __name__=='__main__':
 paths=load('/tmp/mltrain/*.json');x,delta,nxt,ori,target=make_data(paths)
 valid=nxt>=0
 print('data',x.shape,flush=True)
 tests={label:load(pat) for label,pat in [('train','/tmp/train/*.json'),('validation','/tmp/validate/*.json'),('test','/tmp/mltest/*.json'),('practice','/opt/mtx/practice/*.json')]}
 for iteration in range(16):
  model=HistGradientBoostingRegressor(max_iter=110,max_leaf_nodes=25,min_samples_leaf=220,learning_rate=.1,l2_regularization=1,early_stopping=False,random_state=86)
  model.fit(x,target)
  pickle.dump(model,open('/tmp/fitted-'+str(iteration)+'.pkl','wb'))
  print('iteration',iteration,flush=True)
  for label in ['train','validation','test','practice']:eval_model(model,label,tests[label])
  pred=model.predict(x)
  target=delta.copy();target[valid]+=np.clip(pred[nxt[valid]]*ori[valid],-.065,.065)
