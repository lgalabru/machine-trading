import pickle,struct,sys
sys.path.insert(0,'/app/bot/research')
from learn import features
from sim import load
models=pickle.load(open('/tmp/models.pkl','rb'))
b=bytearray(struct.pack('<I',len(models)))
for m in models:
 b.extend(struct.pack('<dI',float(m._baseline_prediction[0,0]),len(m._predictors)))
 for stage in m._predictors:
  t=stage[0].nodes;b.extend(struct.pack('<I',len(t)))
  for node in t:
   b.extend(struct.pack('<BBddHH',int(node['feature_idx']),int(node['is_leaf']),float(node['num_threshold']),float(node['value']),int(node['left']),int(node['right'])))
open('/app/bot/src/model.bin','wb').write(b)
print('exported',len(b),'bytes')
paths=load('/tmp/validate/*.json');fixture=[]
for _,path in paths[:5]:
 x,_,_=features(path)
 for row in x[::7]:fixture.append((list(map(float,row)),[float(m.predict(row[None,:])[0]) for m in models]))
with open('/app/bot/src/model_fixtures.rs','w') as f:
 f.write('pub const FIXTURES: &[([f64;29],[f64;3])] = &[\n')
 for x,y in fixture:f.write('('+repr(x)+','+repr(y)+'),\n')
 f.write('];\n')
