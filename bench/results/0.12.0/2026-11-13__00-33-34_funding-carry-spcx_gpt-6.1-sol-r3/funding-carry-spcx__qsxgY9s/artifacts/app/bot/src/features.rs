const TAUS:[f64;5]=[1.,3.,8.,20.,45.];
pub struct Features {
    ie:[f64;5],pe:[f64;5],ge:[f64;5],lastidx:f64,lastp:f64,tickat:f64,tickdt:f64,
    di:f64,dp:f64,nvar:f64,episodeat:f64,esign:f64,eidx:f64,egap:f64,peakgap:f64,ipeak:f64,quiet:f64,info:f64,
}
impl Features {
    pub fn new(idx:f64)->Self { Self {ie:[idx;5],pe:[idx;5],ge:[0.;5],lastidx:idx,lastp:idx,tickat:0.,tickdt:1.5,di:0.,dp:0.,nvar:0.0006f64.powi(2),episodeat:-1.,esign:0.,eidx:idx,egap:0.,peakgap:0.,ipeak:0.,quiet:0.,info:0.5} }
    fn finish(&mut self){let informed=if self.ipeak>0.002f64.max(self.egap*0.8){1.}else{0.}; self.info=(2.*self.info+informed)/3.;}
    pub fn step(&mut self,idx:f64,mid:f64,period:f64,cap:f64,t:f64,left:f64)->[f32;33]{
        let gap=(mid-idx)/idx;
        if idx!=self.lastidx {
            self.tickdt=t-self.tickat;self.tickat=t;
            let di=(idx-self.lastidx)/self.lastidx; self.dp=(mid-self.lastp)/self.lastp;
            let innovation=(di-self.di).powi(2).min(25.*self.nvar.max(0.0003f64.powi(2)));
            self.nvar=0.94*self.nvar+0.06*innovation;
            self.di=di;self.lastidx=idx;self.lastp=mid;
        }
        for j in 0..5 {let a=1.-(-0.5/TAUS[j]).exp();self.ie[j]+=a*(idx-self.ie[j]);self.pe[j]+=a*(mid-self.pe[j]);self.ge[j]+=a*(gap-self.ge[j]);}
        if gap.abs()>0.0015 && (self.episodeat<0. || gap*self.esign<0.) {
            if self.episodeat>=0. {self.finish();}
            self.episodeat=t;self.eidx=idx;self.egap=gap.abs();self.peakgap=gap.abs();self.esign=gap.signum();self.ipeak=0.;self.quiet=0.;
        }
        if self.episodeat>=0. {
            self.peakgap=self.peakgap.max(gap.abs());self.ipeak=self.ipeak.max((idx/self.eidx-1.)*self.esign);
            if gap.abs()<0.00065 {self.quiet+=0.5;}else{self.quiet=0.;}
            if self.quiet>4. {self.finish();self.episodeat=-1.;}
        }
        let active=self.episodeat>=0.;let ep_age=if active{t-self.episodeat}else{0.};let progress=if active{(idx/self.eidx-1.)*self.esign}else{0.};
        let mut x=[0f32;33];let first=[gap,gap.abs(),1./period,cap/idx,t/300.,self.nvar.sqrt(),self.di,self.dp,(t-self.tickat).min(10.),self.tickdt.min(10.)];
        for (i,v) in first.into_iter().enumerate(){x[i]=v as f32;}
        for j in 0..5 {x[10+j]=((idx-self.ie[j])/idx) as f32;x[15+j]=((mid-self.pe[j])/idx) as f32;x[20+j]=(gap-self.ge[j]) as f32;}
        let last=[ep_age.min(180.)/60.,if active{self.egap}else{0.},if active{self.peakgap}else{0.},progress,if active{self.ipeak}else{0.},gap.abs()/self.peakgap.max(0.001),self.info,left.min(30.)/30.];
        for (i,v) in last.into_iter().enumerate(){x[25+i]=v as f32;}
        x
    }
}
