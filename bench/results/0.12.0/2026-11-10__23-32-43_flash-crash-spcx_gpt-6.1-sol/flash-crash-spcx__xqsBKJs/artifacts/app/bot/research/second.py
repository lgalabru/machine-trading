import os,sys,time,pickle
import numpy as np
from sklearn.ensemble import HistGradientBoostingRegressor
from large import Cache,data,check
from sim import load
if __name__=='__main__':
 while not os.path.exists('/tmp/large-pred-q-24-holdout.pkl'):time.sleep(2)
 time.sleep(2)
 started=time.time();cache=Cache(load('/tmp/secondtrain/*.json'));x,delta,nxt,ori,_,_=data(cache)
 print('new data',x.shape,'seconds',time.time()-started,flush=True)
 valid=nxt>=0;old=pickle.load(open('/tmp/large-q-24.pkl','rb'));pred=old.predict(x)
 tests={label:Cache(load(pat)) for label,pat in [('train','/tmp/train/*.json'),('validation','/tmp/validate/*.json'),('test','/tmp/mltest/*.json'),('holdout','/tmp/bigtest/*.json'),('practice','/opt/mtx/practice/*.json')]}
 for it in range(6):
  target=delta.copy();target[valid]+=np.clip(pred[nxt[valid]]*ori[valid],-.10,.10)
  model=HistGradientBoostingRegressor(max_iter=160,max_leaf_nodes=31,min_samples_leaf=650,learning_rate=.08,l2_regularization=3,early_stopping=False,max_features=.9,random_state=1931+it)
  model.fit(x,target);pickle.dump(model,open('/tmp/second-q-'+str(it)+'.pkl','wb'))
  if it in [0,2,5]:
   for label,test in tests.items():check(test,[model],label,it,'second')
  print('fit',it,'elapsed',time.time()-started,flush=True);pred=model.predict(x)
