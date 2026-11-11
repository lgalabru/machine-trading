import json,os
os.makedirs('/tmp/stress',exist_ok=True)
for d in [-1,1]:
 for pause in [32,140]:
  for outcome in ['revert','stall','extend']:
   name=f'stress-{d:+d}-{pause}-{outcome}'
   path=[{'slot':12,'level_pct':.7*d},{'slot':28,'level_pct':.1*d}]
   slot=80;level=.1*d
   for leg in range(3):
    for jump in [3.6,1.7,2.5]:
     level+=jump*d;path.append({'slot':slot,'level_pct':round(level,3)});slot+=6
    # Challenge the held position with temporary counter-moves during the hold.
    if pause==140:
     path.extend([{'slot':slot+24,'level_pct':round(level-1.6*d,3)},{'slot':slot+48,'level_pct':round(level-.3*d,3)}])
    slot+=pause
   destination={'revert':.5*d,'stall':level-.2*d,'extend':level+5*d}[outcome]
   diff=destination-level
   while abs(diff)>3.4:
    level+=3.4*(1 if diff>0 else -1);path.append({'slot':slot,'level_pct':round(level,3)});slot+=6;diff=destination-level
   path.append({'slot':slot,'level_pct':destination})
   j={'name':name,'description':'Three maximum-size legs with intervening counter-moves; risk stress test','symbol':'SPCX','duration_slots':720,'mandate':{'version':'v0','universe':[{'instrument':'phoenix:perp:SPCX','max_position':70.0}],'limits':{'max_drawdown_usd':1000.0}},'path':path}
   json.dump(j,open('/tmp/stress/'+name+'.json','w'))
