import numpy as np, sys, json
CAP=700; MARK=143.05; NOT=CAP*MARK; DD=1000.0
FEE=3.5e-4
def cost(q):  # USD cost to trade q SPCX in one step (fee + spread + walk)
    q=abs(q); 
    if q<1e-9: return 0.0
    return q*MARK*(FEE + (1.4 + q/80.0)*1e-4)
P=dict(sd=1.553e-4, wsd=0.343, mult=(0.8,2.0), share=0.7, q_cs=1/700., q_sc=1/300.,
       df=5.47, ga=0.1123, gb=0.7205, block=60,
       prem=(0.3,1.2), period=(400,1600), cap=(0.6,1.6),
       lam=(0.0002,0.004), sq=(1.0,3.0), sqd=(2,10), pw=1.0, ptau=60, pn=(0.02,0.02))
def draw(rng,P=P):
    T=int(rng.integers(300,501))
    lev=np.exp(P['wsd']*rng.standard_normal())
    st=np.zeros(T,int); s=0 if rng.random()<P['share'] else 1
    for t in range(T):
        st[t]=s
        if P.get('block_switch'):
            if t>0 and t%60==0:
                r=(P['q_cs'] if s==0 else P['q_sc'])*60
                if rng.random()<r: s=1-s
        else:
            r=P['q_cs'] if s==0 else P['q_sc']
            if rng.random()<r: s=1-s
    sig=P['sd']*lev*np.array(P['mult'])[st]
    # GARCH-t on blocks
    ret=np.zeros(T); h=1.0
    for b0 in range(0,T,P['block']):
        n=min(P['block'],T-b0); df=P['df']
        z=rng.standard_normal(); scale=np.sqrt((df-2)/rng.chisquare(df))
        sdb=np.sqrt(np.sum(sig[b0:b0+n]**2))
        diff=sdb*np.sqrt(h)*z; exc=diff*(scale-1)
        pieces=rng.standard_normal(n); pieces-=pieces.mean()
        w=sig[b0:b0+n]/np.sqrt(np.mean(sig[b0:b0+n]**2))
        ret[b0:b0+n]=diff*(w**2)/np.sum(w**2)+ sig[b0:b0+n]*np.sqrt(h)*pieces
        ret[b0+int(rng.integers(n))]+=exc
        e=np.sqrt(h)*z*scale; h=(1-P['ga']-P['gb'])+P['ga']*e*e+P['gb']*h
    sign=1 if rng.random()<0.5 else -1
    p0=np.exp(rng.uniform(np.log(P['prem'][0]),np.log(P['prem'][1])))/100
    period=rng.uniform(*P['period']); capf=rng.uniform(*P['cap'])/100
    prem=np.zeros(T); p=p0; pbar=p0
    # squeeze
    sq_t=None
    for t in range(T):
        lam=P['lam'][st[t]]
        if sq_t is None and rng.random()<lam: sq_t=t
    adv=np.zeros(T)
    if sq_t is not None:
        S=rng.uniform(*P['sq'])/100; D=int(rng.integers(P['sqd'][0],P['sqd'][1]+1))
        for k in range(D):
            if sq_t+k<T: adv[sq_t+k]=S/D
        reb=int(rng.integers(30,91)); p1=np.exp(rng.uniform(np.log(P['prem'][0]),np.log(P['prem'][1])))/100
    for t in range(T):
        if sq_t is not None and t>=sq_t:
            k=t-sq_t
            p=0.0 if k<D else min(1,(k-D)/reb)*p1
        else:
            tgt=pbar*(P['pw'] if st[t]==1 else 1.0)
            p=p+(tgt-p)/P['ptau']+P['pn'][st[t]]*pbar*rng.standard_normal()
        prem[t]=max(p,0)
    # carry side is short when sign=+1 (book above index). book log return for carry pos:
    # carry pnl per unit notional = funding - sign*dBook ; adverse squeeze = book moves against carry
    book_ret=ret + np.diff(np.r_[prem[0],prem])*1.0  # premium changes move book
    carry_ret = -book_ret + (-adv)   # in carry coordinates (sign folded): adverse squeeze negative
    # but premium reset at squeeze: book = index*(1+p); premium drop is favourable to carry; offset so that book net adverse S
    if sq_t is not None:
        carry_ret[sq_t:sq_t+D] += -(prem[sq_t-1] if sq_t>0 else p0)/D*0  # keep book net adverse S
    # fix: premium reset handled by index jump so book moves S adverse; remove premium diff effect there
    if sq_t is not None:
        pre=prem[sq_t-1] if sq_t>0 else p0
        dprem=np.diff(np.r_[prem[0],prem])
        carry_ret[sq_t:]+= dprem[sq_t:]*1.0  # neutralize premium-path effect after squeeze (index absorbs)
    fund=np.minimum(prem,capf)/period
    return dict(prem=prem,T=T,st=st,sig=sig,ret=ret,carry=carry_ret,fund=fund,sq=sq_t,lev=lev,p0=p0,period=period,
                tv=np.sum(np.abs(book_ret))+ (np.sum(adv) if sq_t is not None else 0), fsum=fund.sum())
def run(sess,policy):
    T=sess['T']; pos=0.0; val=0.0; peak=0.0; st={}
    for t in range(T):
        tgt=policy(sess,t,pos,peak-val,st)
        tgt=max(0.0,min(1.0,tgt))
        dq=(tgt-pos)*CAP
        if abs(dq)>=0.5:
            val-=cost(min(abs(dq),120)); pos+= np.sign(dq)*min(abs(dq),120)/CAP
        # market moves
        val+= pos*NOT*(sess['carry'][t]+sess['fund'][t])
        peak=max(peak,val)
        if peak-val>DD:
            val-=cost(pos*CAP)*1.0 if pos*CAP<=120 else sum(cost(min(120,pos*CAP-i)) for i in range(0,int(pos*CAP),120))
            return val, True
    return val, False
def const(l): return lambda s,t,pos,dd,st: l
def prem_scaled(k):
    return lambda s,t,pos,dd,st: k*s['fund'][t]/ (1.2e-2/1000)
def told(kappa,ls):
    def f(s,t,pos,dd,st):
        if s['st'][t]==1: return ls
        l=min(1,kappa*s['fund'][t]/(s['sig'][t]**2)/1e4)
        if abs(l-pos)<0.12: return pos
        return l
    return f
def told_lev(kappa):
    # knows current sigma (regime*level) and hazard state
    def f(s,t,pos,dd,st):
        if s['st'][t]==1: return 0.0
        return kappa*s['fund'][t]/ (s['sig'][t]**2)/1e4
    return f
def oracle():
    def f(s,t,pos,dd,st):
        if s['sq'] is not None and s['sq']-3<=t<=s['sq']+12: return 0.0
        return 1.0 if s['st'][t]==0 else 0.5
    return f
def ref(kappa,z,hl=20,H=60):
    a=1-0.5**(1/hl)
    def f(s,t,pos,dd,st):
        r=s['ret'][t-1] if t>0 else 0.0
        v=st.get('v',(1.553e-4)**2)
        v=(1-a)*v+a*r*r; st['v']=v
        sd=np.sqrt(v)
        if t<15: return 0.0
        l=kappa*s['fund'][t]/v/1e4
        room=max(DD-dd-200,0)
        lstop=room/(NOT*z*sd*np.sqrt(H))
        l=min(1,l,lstop)
        if abs(l-pos)<0.12: return pos
        return l
    return f
def ref2(kappa,z,vr,pr,hl=20,H=60):
    a=1-0.5**(1/hl); b=1-0.5**(1/60)
    def f(s,t,pos,dd,st):
        r=s['ret'][t-1] if t>0 else 0.0
        v=st.get('v',(1.553e-4)**2); v=(1-a)*v+a*r*r; st['v']=v
        fu=s['fund'][t]
        if t<30:
            st['v0']=v if t==0 else st['v0']*(t)/(t+1)+v/(t+1); st['f0']=fu if t==0 else st['f0']*t/(t+1)+fu/(t+1)
            return 0.0
        v0=st['v0']; f0=st['f0']
        # slow baseline update only when calm-looking
        stress = (v/v0>vr) or (fu/f0>pr)
        if not stress:
            st['v0']=(1-0.005)*v0+0.005*v
        sd=np.sqrt(v)
        if stress: l=0.0
        else:
            l=kappa*fu/v/1e4
            room=max(DD-dd-200,0); l=min(1,l,room/(NOT*z*sd*np.sqrt(H)))
        if abs(l-pos)<0.12: return pos
        return l
    return f
def bayes(th,P=P):
    from math import log,exp
    m=P['mult']; pn=P['pn']; qcs=P['q_cs']; qsc=P['q_sc']
    def f(s,t,pos,dd,st):
        prem=s['fund'][t]  # proxy only for sizing
        if t==0:
            st['pi']=1-P['share']; st['pb']=None; st['ps']=[]; st['ll']=0.0
            return 0.0
        r=s['ret'][t-1]
        # observed premium: reconstruct from fund*period is not available; use book-index premium series
        p=s['prem'][t]; pp=s['prem'][t-1]
        st['ps'].append(p)
        pbar=np.mean(st['ps'][:30]) if len(st['ps'])>=5 else None
        pi=st['pi']; pi=pi*(1-qsc)+(1-pi)*qcs
        sig0=1.553e-4
        def ll(k):
            sd=sig0*m[k]; L=-0.5*(r/sd)**2-log(sd)
            if pbar and pp>0:
                d=p-pp-(pbar*(1.5 if k else 1)-pp)/P['ptau']; sdp=pn[k]*pbar
                L+=-0.5*(d/sdp)**2-log(sdp)
            return L
        l0,l1=ll(0),ll(1); mx=max(l0,l1)
        a=(1-pi)*exp(l0-mx); b=pi*exp(l1-mx); pi=b/(a+b); st['pi']=pi
        v=st.get('v',(1.553e-4)**2); v=0.966*v+0.034*r*r; st['v']=v
        if t<10 or pi>th: return 0.0
        l=min(1,16*s['fund'][t]/v/1e4)
        return l if abs(l-pos)>=0.12 else pos
    return f
def evalset(sessions,policy):
    out=[run(s,policy) for s in sessions]
    return np.array([o[0] for o in out]), np.mean([o[1] for o in out])
if __name__=='__main__':
    seed=int(sys.argv[1]) if len(sys.argv)>1 else 1
    if len(sys.argv)>2: P.update({k:(tuple(v) if isinstance(v,list) else v) for k,v in json.loads(sys.argv[2]).items()})
    NTR=int(sys.argv[3]) if len(sys.argv)>3 else 300
    rng=np.random.default_rng(seed)
    train=[draw(rng) for _ in range(NTR)]
    test=[draw(rng) for _ in range(2*NTR)]
    def best(make,grid,data):
        sc=[(evalset(data,make(*g))[0].sum(),g) for g in grid]
        return max(sc)[1]
    res={}
    bc=best(lambda l:const(l),[(l,) for l in np.linspace(0.05,1,20)],train)
    bp=best(lambda k:prem_scaled(k),[(k,) for k in np.linspace(0.05,2,20)],train)
    bt=best(lambda a,b:told(a,b),[(a,b) for a in (1,4,16,64,1000) for b in (0,0.1)],train)
    br=best(lambda k,z:ref(k,z),[(k,z) for k in (2,4,8,16,32) for z in (1,2,3)],train)
    br2=best(lambda k,z,vr,pr:ref2(k,z,vr,pr),[(k,z,vr,pr) for k in (8,32) for z in (1,2) for vr in (1.8,2.5,4) for pr in (1.2,1.35,1e9)],train)
    bb=best(lambda th:bayes(th),[(th,) for th in (0.2,0.35,0.5,0.7)],train)
    rungs={'nop':const(0),'max-lever':const(1.0),'fixed-3x':const(0.3),'best-const%.2f'%bc:const(bc[0]),
           'prem-scaled':prem_scaled(bp[0]),'reference':ref(*br),'ref2':ref2(*br2),'bayes':bayes(bb[0]),'told':told(*bt),'path-oracle':oracle()}
    vals={}
    for k,p in rungs.items():
        v,so=evalset(test,p); vals[k.split('0')[0].split('1')[0]]=v; res[k]=(v.mean(),so, v.std()/np.sqrt(len(v)))
        print(f"{k:16s} mean {v.mean():8.1f} se {v.std()/np.sqrt(len(v)):6.1f} stopout {so:.2f}")
    bound=np.mean([NOT*(s['tv']+s['fsum']) for s in test]); cb=np.mean([NOT*s['fsum'] for s in test])
    print('ref2',br2,'params best const',bc,'prem',bp,'told',bt,'ref',br,'bound',bound,'carry-bound',cb,
          'stressed share',np.mean([s['st'].mean() for s in test]),'squeeze share',np.mean([s['sq'] is not None for s in test]))

    names=list(vals); V=np.array([vals[n] for n in names]); idx={n:i for i,n in enumerate(names)}
    B=2000; n=100; ok=0; g0=0; ratios=[]
    for b in range(B):
        ii=rng.integers(0,V.shape[1],n); S=V[:,ii].sum(1)
        g=lambda k:S[idx[k]]
        nonread=max(0,g('max-lever'),g('fixed-3x'),g('best-const'),g('prem-scaled'))
        rule = max(g('max-lever'),g('fixed-3x'),g('prem-scaled'))<g('reference')<g('told')<g('path-oracle') and g('reference')>0
        ok+=rule; g0+= g('told')>=2*nonread; ratios.append(g('told')/max(nonread,1e-9))
    tot={k:V[idx[k]].sum() for k in names}
    nr=max(0,*(tot[k] for k in ('max-lever','fixed-3x','best-const','prem-scaled')))
    print('GATE0 told/best-nonreading = %.2f (told %.0f, best non-reading %.0f)'%(tot['told']/max(nr,1e-9),tot['told'],nr))
    print('bootstrap(100-session sets): order rule holds %.3f, gate0 holds %.3f, ratio p5 %.2f median %.2f'%(ok/B,g0/B,np.percentile(ratios,5),np.median(ratios)))
    print('skill vs reference:',{k:round(tot[k]/tot['reference'],2) for k in names})
    print('reward ref (house bound) %.3f, (carry bound) %.3f'%(tot['reference']/(bound*len(test)),tot['reference']/(cb*len(test))))
