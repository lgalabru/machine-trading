import itertools
from pathsim import *
S=load('gen/gen300'); tr,te=S[:150],S[150:]
res=[]
for K,h,buf,z,cl in itertools.product([0.002,0.003,0.006,1.0],[0.12,0.25,0.4],[200,400],[1.5,2.5],[0.03**2,0.035**2,0.045**2]):
    p=make_ref(K=K,hyst=h,buf=buf,z=z,churn_line=cl,widen=9); a,_=total(tr,p); res.append((a/150,K,h,buf,z,cl**0.5))
res.sort(reverse=True)
for r in res[:6]:
    p=make_ref(K=r[1],hyst=r[2],buf=r[3],z=r[4],churn_line=r[5]**2,widen=9); b,so=total(te,p); print('train %.0f test %.0f so %d'%(r[0],b/150,so), r[1:])
for name,p in [('told',make_ref(truth=True)),('max',const(1)),('fixed',const(0.3))]:
    b,so=total(te,p); print(name, round(b/150), so)
