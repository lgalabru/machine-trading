"""Replay generated levered-carry scenario files step by step (2 slots), with costs, funding and the priced
stop-out, to tune the reference against the engine's own paths."""
import json, glob, math, sys
MARK=143.05; DD=1000.0; CAP=699.9
def load(d):
    out=[]
    for f in sorted(glob.glob(d+"/*.json")):
        s=json.load(open(f)); T=s['duration_slots']; steps=T//2
        wp=s['path']; book=[]; idx=[]; j=-1; lv=0.0; pr=0.0
        for k in range(steps):
            slot=2*(k+1)
            while j+1<len(wp) and wp[j+1]['slot']+2<=slot: j+=1; lv=wp[j]['level_pct']; pr=wp[j].get('premium_pct',0.0)
            i=MARK*(1+lv/100); idx.append(i); book.append(i*(1+pr/100))
        fu=s['funding']; regs=s['market_model']['regimes']
        st=[]; 
        for k in range(steps):
            slot=2*(k+1); name=[m['name'] for m in regs if m['slot']<=slot][-1]; st.append(name=='stressed')
        out.append(dict(book=book,idx=idx,st=st,period=fu['period_seconds'],interval=fu['interval_seconds'],capusd=fu['max_rate']*1e-3,steps=steps))
    return out
def cost(q,b,cum):
    # q shares traded after cum shares already this step
    return q*b*(3.5e-4+(3.0+(cum+q/2)/10.0)*1e-4)
def run(s,pol):
    pos=0.0; cash_pnl=0.0; peak=0.0; st={}
    book=s['book']; idx=s['idx']
    val=0.0
    for k in range(s['steps']):
        b=book[k]; i=idx[k]
        if k>0:
            val+= pos*(b-book[k-1])
            gap=max(i-b,0.0); rate=min(gap/s['period'], s['capusd']/s['interval'])
            val+= pos*rate*0.5
        peak=max(peak,val)
        if peak-val>DD:
            cum=0.0
            while pos>0.5:
                q=min(100.0,pos); val-=cost(q,b,cum); cum+=q; pos-=q
            return val,True
        tgt=pol(s,k,pos,peak-val,st)
        tgt=max(0.0,min(CAP,tgt)); cum=0.0
        for _ in range(4):
            d=tgt-pos
            if abs(d)<0.5: break
            q=min(abs(d),50.0); val-=cost(q,b,cum); cum+=q; pos+=math.copysign(q,d)
    return val,False
def make_ref(K=0.0016, hl=40, warm=30, churn_line=0.035**2, calm_ratio=0.8, w=0.12, widen=1.3, z=2.0, H=60.0, buf=200.0, hyst=0.12, use_crowd=True, truth=False):
    def pol(s,k,pos,dd,st):
        b=s['book'][k]; i=s['idx'][k]
        if k==0: st.update(v=(1.553e-4)**2, lb=b, ld=None, calm=None, churn=churn_line, n=0, stressed=True); return 0.0
        ch=math.log(b/st['lb']); st['lb']=b
        a=1-0.5**(1/hl)
        if k>3: st['v']=(1-a)*st['v']+a*ch*ch/0.5
        d=1-b/i
        if st['ld'] is None: st['ld']=(d,k)
        elif d!=st['ld'][0]:
            last,at=st['ld']; lev=max(st['calm'] if st['calm'] else d,1e-4)
            secs=max(k-at,1)*0.5; c=(d-last)/lev
            st['churn']=(1-w)*st['churn']+w*c*c/secs; st['n']+=1; st['ld']=(d,k)
            line=churn_line*calm_ratio if st['stressed'] else churn_line
            wid=st['calm'] is not None and d>widen*st['calm']
            st['stressed']= st['n']<8 or st['churn']>line or wid
            if not st['stressed']:
                c0=st['calm'] if st['calm'] else d; st['calm']=c0+0.05*(d-c0)
            elif st['calm'] is None and st['n']>=8: st['calm']=d
        gap=max(i-b,0.0); y=min(gap/s['period'], s['capusd']/s['interval'])/b
        v=st['v']; sd=math.sqrt(v); full=CAP*b
        room=max(DD-dd-buf,0.0); stop=room/(full*z*sd*math.sqrt(H))
        share=min(1.0,K*y/v,stop)
        if truth: share=0.0 if s['st'][k] else min(1.0,stop)
        elif use_crowd and st['stressed']: share=0.0
        if gap<=0 or (k<warm and not truth): share=0.0
        tgt=share*CAP
        if abs(tgt-pos)<hyst*CAP and tgt>0: tgt=pos
        return tgt
    return pol
def told(s,k,pos,dd,st):
    b=s['book'][k]; 
    if s['st'][k]: return 0.0
    return CAP
def const(l): return lambda s,k,pos,dd,st: l*CAP
def total(S,pol):
    r=[run(s,pol) for s in S]; return sum(x[0] for x in r), sum(x[1] for x in r)
if __name__=='__main__':
    S=load(sys.argv[1])
    tr,te=S[:150],S[150:]
    for name,p in [('told',make_ref(truth=True)),('max',const(1)),('fixed',const(0.3)),('ref-default',make_ref()),('ref-nocrowd',make_ref(use_crowd=False))]:
        t,so=total(te,p); print(f"{name:14s} {t/len(te):8.1f}/session stopouts {so}")
