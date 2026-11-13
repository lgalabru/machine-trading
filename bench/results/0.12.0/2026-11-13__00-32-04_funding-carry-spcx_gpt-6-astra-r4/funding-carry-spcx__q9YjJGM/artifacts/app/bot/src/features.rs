#[derive(Default)]
pub struct Features {
 history:Vec<(f64,f64)>, ema:[f64;6],ep_idx:f64,ep_mid:f64,ep_peak:f64,
 ep_start:usize,inactive:usize,last_tick:usize,vol:f64,vol2:f64,ep_dir:f64,
 ep_max_idx:f64,ep_min_idx:f64,count_i:f64,count_c:f64,start_smooth:f64,
}
impl Features {
 pub fn update(&mut self,mid:f64,idx:f64,period:f64,interval:f64,maxfund:f64)->[f32;53] {
  let k=self.history.len();let gap=mid-idx;let unit=idx*0.01;
  if k==0 { self.ema=[idx;6];self.ep_idx=idx;self.ep_mid=mid;self.ep_max_idx=idx;self.ep_min_idx=idx;self.start_smooth=idx; }
  else {
   let (pm,pi)=self.history[k-1];let change=(idx-pi).abs();
   self.vol=0.975*self.vol+0.025*change;self.vol2=0.975*self.vol2+0.025*change*change;
   if change>0.001 || (mid-pm).abs()>0.001 {self.last_tick=k;}
  }
  self.history.push((mid,idx));
  if gap.abs()/idx<0.0004 {self.inactive+=1;} else {self.inactive=0;}
  if self.inactive>=4 {
   if self.ep_dir!=0. {
    let strength=self.ep_dir*(self.ema[2]-self.start_smooth)/self.ep_peak.max(idx*0.003);
    let probability=((strength-0.25)/0.75).clamp(0.,1.);
    self.count_i+=probability;self.count_c+=1.-probability;
   }
   self.ep_dir=0.;self.ep_peak=0.;
  }
  if gap.abs()/idx>0.001 && (self.ep_dir==0. || gap*self.ep_dir<0.) {
   self.ep_dir=gap.signum();self.ep_idx=idx;self.ep_mid=mid;self.ep_peak=gap.abs();self.ep_start=k;self.ep_max_idx=idx;self.ep_min_idx=idx;self.start_smooth=self.ema[2];
  }
  self.ep_peak=self.ep_peak.max(gap.abs());self.ep_max_idx=self.ep_max_idx.max(idx);self.ep_min_idx=self.ep_min_idx.min(idx);
  let mut x=[0f32;53];let mut j=0;
  macro_rules! put { ($v:expr)=>{{x[j]=($v) as f32;j+=1;}}; }
  put!(gap/unit);put!(gap/unit/period);put!(period);put!(interval);put!(maxfund/unit);
  for w in [2,4,8,12,20,32,48,80,120] {
   let (om,oi)=self.history[k.saturating_sub(w)];
   put!((idx-oi)/unit);put!((mid-om)/unit);put!((gap-(om-oi))/unit);
  }
  for (z,tau) in [1.,2.,4.,8.,16.,32.].iter().enumerate() {
   self.ema[z]+=0.5/(tau+0.5)*(idx-self.ema[z]);put!((idx-self.ema[z])/unit);
  }
  put!(self.vol/unit);put!(self.vol2.sqrt()/unit);put!(((k-self.last_tick) as f64*0.5).min(20.));
  let active=self.ep_dir!=0.;
  put!(if active {((k-self.ep_start) as f64*0.5).min(120.)} else {0.});
  put!(if active {self.ep_peak/unit} else {0.});
  put!(if active {gap.abs()/self.ep_peak.max(0.01)} else {0.});
  put!(if active {(idx-self.ep_idx)/unit} else {0.});
  put!(if active {(mid-self.ep_mid)/unit} else {0.});put!(self.ep_dir);
  put!(if active {(idx-self.ep_max_idx)/unit} else {0.});
  put!(if active {(idx-self.ep_min_idx)/unit} else {0.});put!((k as f64*0.5).min(120.));
  put!(self.count_i);put!(self.count_c);put!(1./(1.+((self.count_c-self.count_i)*1.5).clamp(-20.,20.).exp()));
  let _=j;for v in &mut x {*v=(*v as f64*1e6).round() as f32/1e6;}x
 }
}
