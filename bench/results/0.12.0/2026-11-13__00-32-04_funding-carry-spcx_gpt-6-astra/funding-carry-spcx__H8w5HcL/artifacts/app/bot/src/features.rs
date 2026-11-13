#[derive(Default)]
pub struct Features {
    oracle:Vec<f64>, book:Vec<f64>, ei:[f64;6], eg:[f64;6],
    active:bool, age:f64,anchor:f64,maxgap:f64,ep_sign:f64,tickage:f64,
    logodds:f64,nep:f64,ninf:f64,half:f64,firstmove:f64,lagmean:f64,nlags:f64,halfobs:f64,
    vol:f64, lasti:f64,lastdelta:f64,age_jump:f64,jump:f64,
}
impl Features {
    pub fn update(&mut self,i:f64,b:f64,period:f64,fcap:f64)->([f32;61],f64) {
        let k=self.oracle.len();let g=b-i;
        if k==0 {self.ei=[i;6];self.eg=[g;6];self.anchor=i;self.vol=0.06;self.lasti=i;self.age_jump=100.;self.half=15.;self.lagmean=5.;}
        self.oracle.push(i);self.book.push(b);
        for (z,tau) in [2.0f64,5.,10.,20.,40.,80.].iter().enumerate() {
            let a=1.-(-0.5/tau).exp();self.ei[z]+=a*(i-self.ei[z]);self.eg[z]+=a*(g-self.eg[z]);
        }
        let mut sign=if g>=0. {1.} else {-1.};
        if g.abs()<0.05 {sign=if i-self.ei[2]>=0. {1.} else {-1.};}
        if !self.active && g.abs()>0.15 {self.active=true;self.age=0.;self.anchor=self.ei[0];self.maxgap=g.abs();self.ep_sign=g.signum();self.firstmove=0.;self.halfobs=0.;}
        if self.active {
            self.age+=0.5;self.maxgap=self.maxgap.max(g.abs());
            if self.firstmove==0. && (i-self.anchor)*self.ep_sign>(0.12f64).max(0.35*self.maxgap) {self.firstmove=self.age;}
            if self.halfobs==0. && self.age>3. && g.abs()<self.maxgap*0.5 {self.halfobs=self.age;}
            if g.abs()<0.04 || g*self.ep_sign<0. {
                if self.age>8. {
                    let movement=(i-self.anchor)*self.ep_sign;
                    if movement>0.7*self.maxgap {
                        self.logodds=(self.logodds+1.5).min(5.);self.nep+=1.;self.ninf+=1.;
                        if self.firstmove>0. {self.lagmean=(self.lagmean*self.nlags+self.firstmove)/(self.nlags+1.);self.nlags+=1.;}
                    } else if movement.abs()<0.5*self.maxgap {
                        self.logodds=(self.logodds-1.5).max(-5.);self.nep+=1.;
                        if self.halfobs>0. {self.half=0.5*self.half+0.5*self.halfobs;}
                    }
                }
                self.active=false;self.age=0.;self.maxgap=0.;self.anchor=i;
            }
        }
        self.tickage+=0.5;self.age_jump+=0.5;
        if (i-self.lasti).abs()>0.001 {
            let d=i-self.lasti;self.vol=0.95*self.vol+0.05*d.abs().min(0.5);
            if d.abs()>(0.12f64).max(self.vol*2.5) {self.jump=d;self.age_jump=0.;}
            self.lastdelta=d;self.lasti=i;self.tickage=0.;
        }
        let mut x=[0f32;61];x[0]=(g*sign) as f32;x[1]=(g.clamp(-fcap,fcap)/period*8.*sign) as f32;x[2]=period as f32;x[3]=fcap as f32;
        let mut j=4;
        for lag in [1,2,4,8,16,24,40,64,96,160] {let old=k.saturating_sub(lag);x[j]=((i-self.oracle[old])*sign) as f32;x[j+1]=((g-(self.book[old]-self.oracle[old]))*sign) as f32;j+=2;}
        for z in 0..6 {x[j]=((i-self.ei[z])*sign) as f32;x[j+1]=((g-self.eg[z])*sign) as f32;j+=2;}
        x[36]=self.vol as f32;x[37]=self.age as f32;x[38]=((i-self.anchor)*sign) as f32;x[39]=self.maxgap as f32;x[40]=(g.abs()/self.maxgap.max(0.05)) as f32;
        x[41]=self.tickage as f32;x[42]=(self.lastdelta*sign) as f32;x[43]=(self.jump*sign) as f32;x[44]=self.age_jump as f32;
        for (z,w) in [8,24,60,120].iter().enumerate() {let mut lo=i;let mut hi=i;for v in &self.oracle[k.saturating_sub(*w)..=k] {lo=lo.min(*v);hi=hi.max(*v);}x[45+2*z]=((i-(hi+lo)/2.)*sign) as f32;x[46+2*z]=(hi-lo) as f32;}
        x[53]=(1./(1.+(-self.logodds).exp())) as f32;x[54]=self.nep as f32;x[55]=((self.ninf+1.)/(self.nep+2.)) as f32;x[56]=self.half as f32;x[57]=self.firstmove as f32;x[58]=((i-self.anchor)*self.ep_sign/self.maxgap.max(0.1)) as f32;x[59]=self.lagmean as f32;x[60]=self.halfobs as f32;
        for v in &mut x { if v.abs()<1e-7 {*v=0.;} }
        (x,sign)
    }
}
