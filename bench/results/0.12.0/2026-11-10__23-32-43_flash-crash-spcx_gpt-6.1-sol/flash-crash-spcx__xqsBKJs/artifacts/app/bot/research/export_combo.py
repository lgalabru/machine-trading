import pickle,struct,sys
from large import features
from sim import load
q=pickle.load(open(sys.argv[1] if len(sys.argv)>1 else '/tmp/large-q-10.pkl','rb'));fs=pickle.load(open('/tmp/large-forecast.pkl','rb'))
w=float(sys.argv[2]) if len(sys.argv)>2 else .75
ensembles=[[(q,w),(fs[0],1-w)],[(q,1.)],[(fs[1],1.)]]
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
