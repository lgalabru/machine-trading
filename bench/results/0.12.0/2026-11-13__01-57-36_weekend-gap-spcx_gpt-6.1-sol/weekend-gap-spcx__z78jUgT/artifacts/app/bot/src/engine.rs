//! Causal model: one potentially informed account, transitory impact from all others.
#[derive(Clone, Default, Debug)]
pub struct Flow {
    pub net:f64,pub vol:f64,pub buys:u32,pub sells:u32,
    pub first:u64,pub last:u64,pub gap:f64,pub last_price:f64,
    pub evidence:f64,pub pending:f64,pub pending_sq:f64,pub min_gap:f64,pub max_gap:f64,
}
impl Flow {
    pub fn sign(&self)->f64 {self.net.signum()}
    pub fn count(&self)->u32 {self.buys+self.sells}
    pub fn log_ratio(&self)->f64 {
        if self.buys>0 && self.sells>0 {return -50.0;}
        if self.count()==0{return 0.0;}
        // The same side on every order, size and aggressiveness, and a regular execution pace.
        let n=self.count() as f64;
        let sign_evidence=(n-1.0)*2.0f64.ln();
        let pace=if n>=2.0 {((80.0/self.gap.max(3.0)).ln()-1.0)*(n-1.0).min(10.0)} else{0.0};
        (sign_evidence+self.evidence+pace).clamp(-50.0,80.0)
    }
    pub fn active(&self,t:u64)->f64 {
        if self.count()==0{return 0.0;}
        let age=(t.saturating_sub(self.last) as f64-2.5).max(0.0);
        let lower=if self.max_gap>0.0 {((self.max_gap-2.0)/1.5).max(3.0)}else{3.0};
        let upper=if self.min_gap>0.0 {((self.min_gap+2.0)*2.0).min(30.0).max(lower)}else{30.0};
        let mut sum=0.0;let mut survive=0.0;
        for k in 0..16 {
            let pace=lower+(upper-lower)*(k as f64+0.5)/16.0;
            let weight=(pace/lower).powf(-((self.count()-1).min(15) as f64));
            sum+=weight;
            survive+=weight*((1.5*pace-age)/pace).clamp(0.0,1.0);
        }
        let surv=survive/sum.max(1e-12);
        0.95*surv/(0.05+0.95*surv)
    }
    pub fn mean_size(&self)->f64 {(self.vol/self.count().max(1) as f64).clamp(0.5,1.7)}
}
#[derive(Clone)]
struct Filter {
    info:usize, half:f64, impact:f64,
    x:[f64;3],p:[[f64;3];3],score:f64,
}
impl Filter {
    fn new(info:usize,half:f64,impact:f64)->Self {
        Self{info,half,impact,x:[0.0;3],p:[[0.0;3];3],score:0.0}
    }
    fn update(&mut self,dt:f64,vol:&[f64;8],sq:&[f64;8],y:f64) {
        let decay=[1.0,(-dt*2.0f64.ln()/self.half).exp(),(-dt*2.0f64.ln()/300.0).exp()];
        for i in 0..3 {
            self.x[i]*=decay[i];
            for j in 0..3{self.p[i][j]*=decay[i]*decay[j];}
        }
        let all:f64=vol.iter().sum();let allsq:f64=sq.iter().sum();
        let informed=vol.get(self.info).copied().unwrap_or(0.0);
        let infosq=sq.get(self.info).copied().unwrap_or(0.0);
        self.x[0]+=self.impact*informed;
        self.x[1]+=self.impact*(all-informed);
        self.p[0][0]+=(self.impact*0.27).powi(2)*infosq+1e-13*dt;
        self.p[1][1]+=(self.impact*0.27).powi(2)*(allsq-infosq).max(0.0)+1e-13*dt;
        self.p[2][2]+=0.00048f64.powi(2)*dt/12.0;
        if y.abs()>0.0663 {return;}
        let pred=self.x.iter().sum::<f64>();
        let err=y-pred;
        let mut ph=[0.0;3];for i in 0..3 {ph[i]=self.p[i].iter().sum();}
        let variance=(ph.iter().sum::<f64>()+0.000045f64.powi(2)).max(1e-12);
        // Likelihood tempering makes the filter robust to price ticks and asynchronous path updates.
        self.score-=0.25*(err*err/variance+variance.ln());
        let old=self.p;
        for i in 0..3 {
            let k=ph[i]/variance;
            self.x[i]+=k*err;
            for j in 0..3 {self.p[i][j]=old[i][j]-k*ph[j];}
        }
    }
}
#[derive(Clone)]
pub struct Config {
    pub confidence_power:f64,pub first_gap:f64, pub signal_scale:f64, pub horizon:f64,pub trade_threshold:f64,
    pub reopen_reserve:f64,pub risk_budget:f64,pub fade:f64,
}
impl Default for Config {
    fn default()->Self {Self{confidence_power:0.5,first_gap:15.0,signal_scale:0.002,horizon:70.0,trade_threshold:4.0,reopen_reserve:900.0,risk_budget:650.0,fade:1.0}}
}
#[derive(Clone)]
pub struct Engine {
    pub flows:[Flow;8], filters:Vec<Filter>, pub cfg:Config,
    pub p0:f64,pub reopen:u64,last_update:u64,initialized:bool,
    pub info_probs:[f64;9],pub worker_probs:[f64;8],
    pub fair_open:f64,pub expected_change:f64,pub halfspread:f64,
    pub target:f64,pub halted:bool,pub expected_temp:f64,pub expected_impact:f64,
    pub expected_half:f64,pub opening_edge:f64,
}
fn lse(a:&[f64])->f64 {let m=a.iter().copied().fold(f64::NEG_INFINITY,f64::max);m+a.iter().map(|x|(*x-m).exp()).sum::<f64>().ln()}
impl Engine {
    pub fn new(p0:f64,reopen:u64,cfg:Config)->Self {
        let mut filters=Vec::new();
        for j in 0..9 {for h in [60.0,90.0,130.0,180.0,240.0] {for a in [0.0008,0.0011,0.0014,0.0017,0.0020] {filters.push(Filter::new(j,h,a));}}}
        Self{flows:Default::default(),filters,cfg,p0,reopen,last_update:0,initialized:false,
            info_probs:[0.0;9],worker_probs:[0.0;8],fair_open:p0,expected_change:0.0,
            halfspread:0.0012,target:0.0,halted:false,expected_temp:0.0,expected_impact:0.0013,expected_half:130.0,opening_edge:0.0}
    }
    pub fn trade(&mut self,id:usize,t:u64,q:f64,mid:f64,aggression:Option<f64>) {
        if id>=8{return;}
        let f=&mut self.flows[id];
        if f.count()==0{f.first=t;}
        if f.count()>0 {let gap=t.saturating_sub(f.last) as f64;
            f.min_gap=if f.min_gap==0.0 {gap}else{f.min_gap.min(gap)};f.max_gap=f.max_gap.max(gap);
            f.gap=if f.count()==1 {gap} else {((f.last-f.first) as f64+gap)/(f.count() as f64)};
        }
        f.last=t;f.net+=q;f.vol+=q.abs();if q>0.0{f.buys+=1}else{f.sells+=1};
        f.last_price=mid;f.pending+=q;f.pending_sq+=q*q;
        let l=q.abs().max(0.1).ln();
        f.evidence+=((0.7f64/0.5).ln()-(l/0.5).powi(2)*0.5+((l-0.6f64.ln())/0.7).powi(2)*0.5).clamp(-2.0,1.0);
        if let Some(aggr)=aggression {
            // Smooth bounded likelihoods tolerate rounding to venue ticks.
            let r=aggr/self.halfspread.max(0.0006);
            let noise_prob=if r>4.0 {(-(r-4.0)*7.0).exp()}else if r<1.5 {(-(1.5-r)*7.0).exp()}else{1.0};
            let dir_prob=if r<3.0 {(-(3.0-r)*7.0).exp()}else if r>6.0 {(-(r-6.0)*7.0).exp()}else{1.0};
            f.evidence+=(dir_prob.max(0.001)/noise_prob.max(0.001)*2.5/3.0).ln().clamp(-5.0,5.0);
        }
    }
    fn posterior(&mut self,t:u64) {
        
        let info_ratios:Vec<f64>=self.flows.iter().map(|f|if f.first as f64>self.reopen as f64*0.51 {-50.0}else{f.log_ratio()}).collect();
        let worker_ratios:Vec<f64>=self.flows.iter().map(|f|{
            if f.vol>50.1 || f.first as f64>self.reopen as f64*0.61 || (f.count()>0 && f.vol<11.8 && t.saturating_sub(f.last)>50) {-50.0}
            else{f.log_ratio()+((50.0-f.vol).min(38.0).max(0.01)/38.0).ln()}
        }).collect();
        let mut joint=Vec::new();
        for j in 0..9 {
            let mut terms=vec![0.4f64.ln()];
            for i in 0..8 {if i!=j{terms.push((0.6/if j<8{7.0f64}else{8.0f64}).ln()+worker_ratios[i]);}}
            let prior=if j<8 {(0.6f64/8.0).ln()+info_ratios[j]}else{0.4f64.ln()};
            joint.push(prior+lse(&terms));
        }
        let scores:Vec<f64>=self.filters.iter().map(|f|f.score+joint[f.info]).collect();
        let norm=lse(&scores);
        self.info_probs=[0.0;9];self.worker_probs=[0.0;8];
        self.expected_temp=0.0;self.expected_impact=0.0;self.expected_half=0.0;
        for (f,score) in self.filters.iter().zip(scores) {
            let p=(score-norm).exp();
            self.info_probs[f.info]+=p;self.expected_temp+=p*(f.x[1]+f.x[2]);
            self.expected_impact+=p*f.impact;self.expected_half+=p*f.half;
        }
        // Marginalize the unknown worker assignment conditionally on the informed account.
        for j in 0..9 {
            let mut terms=vec![0.4f64.ln()];
            for i in 0..8 {if i!=j {terms.push((0.6/if j<8{7.0f64}else{8.0f64}).ln()+worker_ratios[i]);}}
            let norm=lse(&terms);
            for i in 0..8 {if i!=j {self.worker_probs[i]+=self.info_probs[j]*((0.6/if j<8{7.0f64}else{8.0f64}).ln()+worker_ratios[i]-norm).exp();}}
        }
    }
    pub fn update(&mut self,t:u64,mid:f64,mark:f64,position:f64,drawdown:f64)->f64 {
        let y=mid/self.p0-1.0;
        if !self.initialized {
            for f in &mut self.filters {f.x[2]=y;f.p[2][2]=1e-8;}
            self.initialized=true;
        }
        if t>=self.last_update+6 && t<self.reopen {
            let dt=(t-self.last_update) as f64;
            let vol=std::array::from_fn(|i|self.flows[i].pending);
            let sq=std::array::from_fn(|i|self.flows[i].pending_sq);
            for f in &mut self.filters{f.update(dt,&vol,&sq,y);}
            for f in &mut self.flows{f.pending=0.0;f.pending_sq=0.0;}
            self.last_update=t;
        }
        self.posterior(t);
        let remaining=self.reopen.saturating_sub(t) as f64;
        let horizon=self.cfg.horizon.min(remaining);
        let decay=1.0-(-horizon*2.0f64.ln()/self.expected_half).exp();
        let mut expected=-self.expected_temp*decay*self.cfg.fade;
        let mut fair=0.0;
        for j in 0..9 {
            let p=self.info_probs[j];
            if j==8 || self.flows[j].count()==0 {fair+=p;continue;}
            let f=&self.flows[j];let active=f.active(t);
            // An inactive informed account reveals its target through its last execution price.
            let at_band=(mid/self.p0-1.0).abs()>0.063;
            let extra=if at_band {0.022}else{0.014};
            let informed_open=active*(mid/self.p0+f.sign()*extra)+(1.0-active)*(f.last_price/self.p0-f.sign()*self.expected_impact*0.2);
            fair+=p*informed_open;
        }
        for i in 0..8 {
            let f=&self.flows[i];if f.count()==0{continue;}
            let active=f.active(t);
            let rate=f.mean_size()/if f.gap>0.0 {f.gap.max(3.0)}else{self.cfg.first_gap};
            let remaining_work=if f.vol<12.0 {31.0-f.vol}else{(50.0-f.vol).max(0.0)*0.5};
            let info_future=rate*horizon;
            let work_future=(rate*horizon).min(remaining_work);
            let confidence=self.info_probs[i]+self.worker_probs[i];
            let contribution=(self.info_probs[i]*info_future+self.worker_probs[i]*work_future)*confidence.powf(self.cfg.confidence_power);
            expected+=f.sign()*self.expected_impact*contribution*active;
        }
        self.fair_open=fair*self.p0;
        self.opening_edge=(self.fair_open-mid)/self.p0;
        if remaining<self.cfg.horizon {
            let w=(1.0-remaining/self.cfg.horizon).powi(2);
            expected=(1.0-w)*expected+w*self.opening_edge;
        }
        self.expected_change=expected;
        let cost=self.halfspread+0.00035;
        // Hysteresis in expected return, applied before the position cap.
        let lower=((expected-cost)/self.cfg.signal_scale*70.0).clamp(-70.0,70.0);
        let upper=((expected+cost)/self.cfg.signal_scale*70.0).clamp(-70.0,70.0);
        let mut target=if position<lower-self.cfg.trade_threshold {lower}
                       else if position>upper+self.cfg.trade_threshold {upper}
                       else {position};
        if remaining<=30.0 {
            let adverse=if target>=0.0 {(mark/self.p0-0.85).max(0.02)}else{(1.15-mark/self.p0).max(0.02)};
            let safe=(self.cfg.reopen_reserve-drawdown).max(0.0)/(self.p0*adverse);
            target=target.clamp(-safe,safe);
        }
        if drawdown>self.cfg.risk_budget {self.halted=true;}
        if t>=self.reopen {target=position;}
        if self.halted {target=0.0;}
        self.target=target.clamp(-70.0,70.0);self.target
    }
}
