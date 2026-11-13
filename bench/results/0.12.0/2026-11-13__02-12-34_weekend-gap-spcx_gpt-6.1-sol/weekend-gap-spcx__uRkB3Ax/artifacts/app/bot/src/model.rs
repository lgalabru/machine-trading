//! A small ensemble separates persistent informed impact from decaying flow.
const LN2:f64=std::f64::consts::LN_2;
#[derive(Clone)]
struct Hypothesis { actor:usize, half:f64, impact:f64, x:[f64;3], p:[[f64;3];3], ll:f64, w:f64 }
#[derive(Clone,Default)]
pub struct Actor {
 pub net:f64,pub gross:f64,pub count:usize,pub last:u64,pub first:u64,pub last_price:f64,
 pub gaps:f64,pub direction:f64,
}
impl Actor {
 pub fn pace(&self)->f64 {if self.count>1 {(self.gaps/(self.count-1) as f64).clamp(3.,30.)}else{20.}}
 pub fn strength(&self)->f64 {if self.net.abs()<self.gross*0.99 {0.}else{((self.count as f64-1.)/3.).clamp(0.,1.)}}
 pub fn active(&self,t:u64)->bool {self.count>0&&(t.saturating_sub(self.last) as f64)<self.pace()*1.7+4.}
 pub fn rate(&self)->f64 {self.gross/(self.count.max(1) as f64)/self.pace()}
}
pub struct Model { hs:Vec<Hypothesis>,pub actors:[Actor;8],last_flow:u64,pub probs:[f64;9], pub half:f64,pub impact:f64 }
impl Default for Model {
 fn default()->Self {
  let mut hs=Vec::new();
  for c in 0..9 {for h in [65.,100.,155.,235.] {for impact in [0.00085,0.00115,0.0015,0.0019] {
   let mut p=[[0.;3];3];for (i,row) in p.iter_mut().enumerate(){row[i]=1e-8;}
   hs.push(Hypothesis{actor:c,half:h,impact,x:[0.;3],p,ll:0.,w:0.});
  }}}
  Self {hs,actors:std::array::from_fn(|_|Actor::default()),last_flow:0,probs:[0.;9],half:150.,impact:0.0014}
 }
}
impl Model {
 pub fn flow(&mut self,t:u64,a:usize,q:f64) {
  if a>=8{return}
  let f=&mut self.actors[a];if f.count>0 {f.gaps+=t.saturating_sub(f.last) as f64;}else{f.first=t;}
  f.net+=q;f.gross+=q.abs();f.count+=1;f.last=t;f.direction=f.net.signum();self.last_flow=t;
 }
 pub fn update(&mut self,t:u64,price:f64,flows:&[(usize,f64)],disturbed:bool) {
  for a in &mut self.actors {if a.count>0&&t.saturating_sub(a.last)<=4 {a.last_price=price;}}
  // Workers finish within 50 shares; sustained larger one-way flow is informed.
  let forced=self.actors.iter().position(|a|a.gross>55.&&a.net.abs()>a.gross*0.99);
  let mut max:f64=-1e100;
  for h in &mut self.hs {
   let f=[1.,(-LN2*2./h.half).exp(),(-LN2*2./300.).exp()];
   for i in 0..3 {h.x[i]*=f[i];for j in 0..3 {h.p[i][j]*=f[i]*f[j];}}
   h.p[2][2]+=2./13.*0.0005f64.powi(2);
   for &(a,q) in flows {let i=if h.actor==a {0}else{1};h.x[i]+=h.impact*q;h.p[i][i]+=(h.impact*q*0.3).powi(2);}
   let y=price-h.x.iter().sum::<f64>();let mut v=0.00007f64.powi(2);
   if self.actors.iter().any(|a|a.count>0) && t.saturating_sub(self.last_flow)<4 {v+=0.002f64.powi(2);}
   if disturbed {v+=0.0006f64.powi(2);}
   // At the band the observed price censors further price impact.
   if price.abs()>0.065 {v+=0.1f64.powi(2);}
   let mut row=[0.;3];for i in 0..3 {row[i]=h.p[i].iter().sum();v+=row[i];}
   if v>0.&&v.is_finite() {
    for i in 0..3 {h.x[i]+=row[i]/v*y;for j in 0..3 {h.p[i][j]-=row[i]*row[j]/v;}}
    h.ll+=-0.5*v.ln()-0.5*y*y/v;
   }
   let prior=if let Some(c)=forced {if h.actor==c {0.}else{-1e6}}else if h.actor==8 {0.4f64.ln()}else{let a=&self.actors[h.actor];if a.count==0 || a.net.abs()<a.gross*0.99 {-100.}else{(0.6f64/8.).ln()}};
   h.w=h.ll+prior;max=max.max(h.w);
  }
  let mut total=0.;for h in &mut self.hs {h.w=(h.w-max).exp();total+=h.w;}
  self.probs=[0.;9];self.half=0.;self.impact=0.;
  for h in &mut self.hs {h.w/=total;self.probs[h.actor]+=h.w;self.half+=h.w*h.half;self.impact+=h.w*h.impact;}
 }
 pub fn signal(&self,t:u64,left:u64,price:f64)->(f64,f64,f64) {
  let horizon=(left as f64).min(60.);let decay=(-LN2*horizon/self.half).exp();
  let mut worker=[0.;8];let mut rate=[0.;8];
  for a in 0..8 {
   let f=&self.actors[a];rate[a]=f.rate();worker[a]=((50.-f.gross)/35.).clamp(0.,1.)*f.strength()*if f.active(t){1.}else{0.};
  }
  let mut terminal=0.;let mut prediction=0.;
  for c in 0..9 {
   let prob=self.probs[c];let mut perm=0.;let mut trans=0.;let mut drift=0.;
   if prob>1e-30 {for h in &self.hs {if h.actor==c {perm+=h.x[0]*h.w/prob;trans+=h.x[1]*h.w/prob;drift+=h.x[2]*h.w/prob;}}}
   let mut news=0.;
   if c<8 {let f=&self.actors[c];let lo=(f.direction*perm).clamp(0.005,0.115);news=if f.active(t) {f.direction*(0.12-lo)/(0.12/lo).ln()}else{perm};}
   let mut flow=0.;
   for a in 0..8 {
    let f=&self.actors[a];
    if a==c {
     let inc=(self.impact*rate[a]*horizon).min((f.direction*(news-price)).max(0.));
     flow+=f.direction*inc*f.strength()*if f.active(t){1.}else{0.};
    } else {flow+=f.direction*self.impact*rate[a]*horizon*worker[a]*0.82;}
   }
   prediction+=prob*(flow+trans*(decay-1.)+drift*((-LN2*horizon/300.).exp()-1.));
   terminal+=prob*news;
  }
  terminal-=price;
  let blend=(1.-left as f64/20.).max(0.);
  (prediction*(1.-blend)+terminal*blend,terminal,prediction)
 }
}
