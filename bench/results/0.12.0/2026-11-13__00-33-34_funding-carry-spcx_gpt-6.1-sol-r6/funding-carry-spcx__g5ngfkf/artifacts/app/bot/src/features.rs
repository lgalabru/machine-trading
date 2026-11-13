//! Causal price, premium, funding, episode, and noise features.
//! The stored history contains only observations from completed/current turns.
pub const NF:usize=25;
#[derive(Default,Clone)]
pub struct Features {
 pub base:f64,pub sign:f64,pub opening:f64,pub startidx:f64,pub informed:bool,pub quietcount:u32,pub age:f64,pub dayodds:f64,
 ema:[f64;4], hist:Vec<(f64,f64,f64)>,noise:f64, prev_i:f64, last_change:f64,
}
impl Features {
 pub fn new(i:f64)->Self {Self{base:i,ema:[i;4],noise:i*0.0005,prev_i:i,..Self::default()}}
 pub fn step(&mut self,t:f64,idx:f64,book:f64,period:f64,interval:f64,fmax:f64)->([f32;NF],f64) {
  let gap=book-idx;let quiet=gap.abs()<idx*0.0005;
  for(j,tau)in [2.,6.,18.,40.].iter().enumerate(){self.ema[j]+=(0.5/tau)*(idx-self.ema[j]);}
  if self.sign==0. && gap.abs()>idx*0.0015 {self.sign=gap.signum();self.opening=gap.abs();self.startidx=self.base;self.informed=false;self.quietcount=0;self.age=0.;}
  if self.sign!=0. {
   self.age+=0.5;self.opening=self.opening.max(gap.abs());
   if (idx-self.startidx)*self.sign>self.opening*0.4+idx*0.0008 {self.informed=true;}
   if quiet{self.quietcount+=1;}else{self.quietcount=0;}
   if self.quietcount>3 || self.sign*gap<0. {
    if (idx-self.startidx)*self.sign>self.opening*0.7 {self.dayodds=(self.dayodds+1.6).clamp(-5.,5.);}else{self.dayodds=(self.dayodds-1.6).clamp(-5.,5.);}
    if self.informed {self.base=idx;self.ema=[idx;4];}
    self.sign=0.;self.quietcount=0;
   }
  }
  if self.sign==0. || !self.informed {let r=idx-self.base;if r.abs()<idx*0.0015 {self.base+=r*0.5/20.;}}
  if (idx-self.prev_i).abs()>0.004 {
   let diff=(idx-self.prev_i).abs().min(self.noise*4.);
   self.noise=self.noise*0.97+diff*0.03;self.last_change=t;
  }
  self.prev_i=idx;
  self.hist.push((t,idx,book));let n=self.hist.len();let old=|k:usize|self.hist[n.saturating_sub(k+1)];
  let anchor=if gap.abs()>idx*0.0005{gap.signum()}else if idx!=self.base{(idx-self.base).signum()}else{1.};
  let z=10000./idx*anchor;
  let o2=old(4);let o6=old(12);let o12=old(24);let o24=old(48);
  let carry=gap.clamp(-fmax*period/interval.max(1.),fmax*period/interval.max(1.))/period;
  let feat=[gap*z,gap.abs()*10000./idx,self.opening*10000./idx,if self.opening>0.001{gap.abs()/self.opening}else{0.},self.age.min(120.),
   (idx-self.startidx)*z,(self.ema[0]-self.ema[1])/4.*z,(self.ema[1]-self.ema[2])/12.*z,(self.ema[2]-self.ema[3])/22.*z,
   (idx-self.ema[0])*z,(idx-self.ema[1])*z,(idx-self.base)*z,self.dayodds,period,carry*z,
   self.noise*10000./idx,(idx-o2.1)*z,(idx-o6.1)*z,(idx-o12.1)*z,(idx-o24.1)*z,
   (gap-(o6.2-o6.1))*z,(gap-(o12.2-o12.1))*z,if quiet{1.}else{0.},if self.informed{1.}else{0.},(t-self.last_change).min(10.)];
  (feat.map(|v|v as f32),anchor)
 }
}
