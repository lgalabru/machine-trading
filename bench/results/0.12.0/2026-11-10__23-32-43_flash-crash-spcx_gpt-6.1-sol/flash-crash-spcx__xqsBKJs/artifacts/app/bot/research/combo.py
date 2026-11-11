import pickle,itertools,sys
import numpy as np
from native import Evaluator
iteration=int(sys.argv[1]) if len(sys.argv)>1 else 10
labels=['train','validation','test','holdout','practice']
raw={label:(pickle.load(open('/tmp/large-pred-forecast-0-'+label+'.pkl','rb')),pickle.load(open('/tmp/large-pred-q-'+str(iteration)+'-'+label+'.pkl','rb'))) for label in labels}
results=[]
for w in [.0,.25,.5,.75,1.]:
 es={}
 for label,(fs,qs) in raw.items():
  combo=[]
  for f,q in zip(fs,qs):
   pr=f[2].copy();pr[:,0]=w*q[2][:,0]+(1-w)*f[2][:,0];pr[:,1]=q[2][:,0];pr[:,2]=f[2][:,1]
   combo.append((f[0],f[1],pr,f[3]))
  es[label]=Evaluator(combo)
 for f,s,x in itertools.product([.03,.05,.07,.09,.11,.14,.18],[.08,.12,.2,.4,1000],[-.15,-.1,-.06,-.03,0]):
  res={label:e(f,s,x,1,stop=650,cost=.001) for label,e in es.items()}
  results.append((res['holdout'][0],(w,f,s,x),res))
for row in sorted(results,reverse=True)[:30]:print(row,flush=True)
pickle.dump(sorted(results,reverse=True),open('/tmp/combo-results-'+str(iteration)+'.pkl','wb'))
