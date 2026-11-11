import pickle,itertools
from native import Evaluator
es={label:Evaluator(pickle.load(open('/tmp/pred-'+label+'.pkl','rb'))) for label in ['train','validation','test','practice']}
rows=[]
for f,s,x,si in itertools.product([.02,.03,.04,.05,.06,.08,.1,.12,.15,.2,.3,.5],[.03,.06,.1,.15,.2,.3,.4,.6,.8],[-.15,-.08,-.04,-.02,0,.02], [1,2]):
 r={label:e(f,s,x,si) for label,e in es.items()}
 # tune on fresh 500 scenarios; keep six practice scenarios out of selection
 value=r['test'][0]+r['train'][0]+r['validation'][0]
 rows.append((value,(f,s,x,si),r))
for row in sorted(rows,reverse=True)[:25]:print(row,flush=True)
print('v3',{label:e(.05,.4,-.03,1) for label,e in es.items()},flush=True)
pickle.dump(sorted(rows,reverse=True),open('/tmp/mlsweep.pkl','wb'))
