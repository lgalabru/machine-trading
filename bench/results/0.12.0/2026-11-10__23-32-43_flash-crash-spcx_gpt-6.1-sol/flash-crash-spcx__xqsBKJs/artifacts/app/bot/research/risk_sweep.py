import pickle,itertools
from native import Evaluator
es={label:Evaluator(pickle.load(open('/tmp/large-pred-forecast-0-'+label+'.pkl','rb'))) for label in ['train','validation','test','holdout','practice']}
for stop in [400,500,600,650,700,750,800,850]:
 r={label:e(.08,.8,-.15,1,stop=stop,cost=.001) for label,e in es.items()}
 print(stop,r,flush=True)
