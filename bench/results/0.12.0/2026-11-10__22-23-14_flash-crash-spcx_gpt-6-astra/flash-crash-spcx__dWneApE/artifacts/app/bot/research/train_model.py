import os,glob,json,sys,argparse
import numpy as np
import lightgbm as lgb
LAGS=[1,2,3,4,6,8,12,16,24,32,48,64,96,128]
WINS=[8,24,64,128]
HORIZONS=[2,8,32,80]
def features(p):
 n=len(p);k=np.arange(n);f=[k/2,(n-1-k)/2,(p/p[0]-1)*100]
 for lag in LAGS:f.append((p/p[np.maximum(k-lag,0)]-1)*100)
 for win in WINS:
  mi=np.array([p[max(0,i-win+1):i+1].min() for i in k]);ma=np.array([p[max(0,i-win+1):i+1].max() for i in k]);mean=np.array([p[max(0,i-win+1):i+1].mean() for i in k]);
  f.extend([(p/mi-1)*100,(p/ma-1)*100,(p/mean-1)*100])
 events=np.zeros((n,15))
 last=p[0];base=last;extreme=last;direction=0;last_move=-200;start=-200;count=0;prev_dir=0;prev_size=0;prev_end=-200;variation=0;biggest=0;changes=0;prev_duration=0
 for i,price in enumerate(p):
  change=(price/last-1)*100
  variation+=abs(change)
  if abs(change)>.25:
   if direction==0 or change*direction<0 or i-last_move>15:
    prev_dir=direction;prev_size=(extreme/base-1)*100;prev_end=last_move;prev_duration=last_move-start
    base=last;extreme=price;direction=np.sign(change);start=i;count+=1;changes=0
   last_move=i;changes+=1
  if (price-extreme)*direction>0:extreme=price
  size=(extreme/base-1)*100
  biggest=max(biggest,abs(size))
  events[i]=[direction,min(i-last_move,200),size,min(i-start,200),count,prev_dir,prev_size,min(i-prev_end,200),(price/base-1)*100,(price/extreme-1)*100,variation,biggest,changes,prev_duration,abs(change)]
  last=price
 return np.column_stack(f+[events]).astype('float32')
def paths(folder):
 result=[]
 for filename in sorted(glob.glob(folder+'/*.json')):
  a=json.load(open(filename));n=(a['duration_slots']+1)//2;p=np.full(n,143.11)
  for x in a['path']:
   i=max(0,(x['slot']+1)//2-1)
   p[i:]=143.11*(1+x['level_pct']/100)
  result.append((a['name'],p))
 return result

def dataset(folder,out):
 seq=paths(folder);xs=[];ys=[];prices=[];offsets=[0]
 for j,(name,p) in enumerate(seq):
  xs.append(features(p)); k=np.arange(len(p));ys.append(np.column_stack([(p[np.minimum(k+h,len(p)-1)]/p-1)*100 for h in HORIZONS]));prices.extend(p);offsets.append(len(prices))
  if j%1000==0:print(out,j,flush=True)
 x=np.concatenate(xs);y=np.concatenate(ys).astype('float32');np.savez(out,X=x,Y=y,price=np.array(prices),offsets=np.array(offsets));return x,y
if __name__ == '__main__':
 parser=argparse.ArgumentParser(description='Train forecasts from generated scenario JSON files.')
 parser.add_argument('--train',required=True)
 parser.add_argument('--validation',required=True)
 parser.add_argument('--out',required=True)
 parser.add_argument('--config',default=os.path.join(os.path.dirname(__file__),'model-config.json'))
 args=parser.parse_args()
 config=json.load(open(args.config))
 HORIZONS=config['horizons_steps']
 os.makedirs(args.out,exist_ok=True)
 x,y=dataset(args.train,os.path.join(args.out,'train.npz'))
 xv,yv=dataset(args.validation,os.path.join(args.out,'validation.npz'))
 for h,horizon in enumerate(HORIZONS):
  model=lgb.LGBMRegressor(**config['lightgbm'])
  model.fit(x,y[:,h],eval_set=[(xv,yv[:,h])],callbacks=[lgb.early_stopping(30),lgb.log_evaluation(50)])
  model.booster_.save_model(os.path.join(args.out,'model%d.txt'%h))
  with open(os.path.join(args.out,'model%d.json'%h),'w') as f:json.dump(model.booster_.dump_model(),f)
