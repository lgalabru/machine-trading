import pickle,itertools,numpy as np
from native import Evaluator
labels=['train','validation','test','holdout','practice']
raw={label:[pickle.load(open('/tmp/'+p+label+'.pkl','rb')) for p in ['large-pred-forecast-0-','large-pred-q-10-','large-pred-q-24-','large-pred-second-5-']] for label in labels}
weights=[(1,0,0),(0,1,0),(0,0,1),(.5,.5,0),(.5,0,.5),(0,.5,.5),(1/3,1/3,1/3),(.25,.25,.5),(.5,.25,.25)]
results=[]
for weights in weights:
 for mix in [.25,.5,.75]:
  es={}
  for label,sets in raw.items():
   ps=[]
   for data in zip(*sets):
    f=data[0];q=sum(w*row[2][:,0] for w,row in zip(weights,data[1:]));pr=f[2].copy();pr[:,0]=mix*q+(1-mix)*f[2][:,0];pr[:,1]=q
    ps.append((f[0],f[1],pr,f[3]))
   es[label]=Evaluator(ps)
  for ft,st,ex in itertools.product([.05,.07,.09,.11,.13,.15],[.08,.12,.2,1000],[-.15,-.1,-.06,-.03]):
   rs={label:e(ft,st,ex,1,stop=650,cost=.001) for label,e in es.items()}
   results.append((rs['holdout'][0],(weights,mix,ft,st,ex),rs))
for row in sorted(results,reverse=True)[:30]:print(row,flush=True)
pickle.dump(sorted(results,reverse=True),open('/tmp/ensemble-results.pkl','wb'))
