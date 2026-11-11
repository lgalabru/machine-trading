import pickle,struct,sys
from large import features
from sim import load
qs=[pickle.load(open(path,'rb')) for path in ['/tmp/large-q-10.pkl','/tmp/large-q-24.pkl','/tmp/second-q-5.pkl']]
fs=pickle.load(open('/tmp/large-forecast.pkl','rb'))
qensemble=list(zip(qs,[.25,.25,.5]))
ensembles=[[(m,w*.5) for m,w in qensemble]+[(fs[0],.5)],qensemble,[(fs[1],1.)]]
b=bytearray(struct.pack('<I',len(ensembles)))
for ensemble in ensembles:
 baseline=sum(float(m._baseline_prediction[0,0])*weight for m,weight in ensemble)
 trees=[(p[0].nodes,weight) for m,weight in ensemble for p in m._predictors]
 b.extend(struct.pack('<dI',baseline,len(trees)))
 for nodes,weight in trees:
  b.extend(struct.pack('<I',len(nodes)))
  for node in nodes:b.extend(struct.pack('<BBddHH',int(node['feature_idx']),int(node['is_leaf']),float(node['num_threshold']),float(node['value'])*weight,int(node['left']),int(node['right'])))
open('/app/bot/src/model.bin','wb').write(b)
print('exported',len(b),'bytes')
fixtures=[]
for _,path in load('/tmp/validate/*.json')[:5]:
 x,_,_=features(path)
 for row in x[::7]:fixtures.append((list(map(float,row)),[sum(weight*float(m.predict(row[None,:])[0]) for m,weight in ens) for ens in ensembles]))
with open('/app/bot/src/model_fixtures.rs','w') as f:
 f.write('pub const FIXTURES: &[([f64;33],[f64;3])] = &[\n')
 for x,y in fixtures:f.write('('+repr(x)+','+repr(y)+'),\n')
 f.write('];\n')
