//! Account-level order flow and price features. Prices here use percentage points.
// Account statistics: count; signed/gross quantity; buy/sell counts; first/last
// trade; gap sum/squares/maximum; limit sum/squares/min/max; last side/size;
// repeat count; maximum size; last observed price. Decays use 24/60/120/240 slots.
#[derive(Clone,Default)]
pub struct Account {pub key:String,pub a:[f64;19],pub d:[f64;4]}
#[derive(Default)]
pub struct Flow { pub accounts:Vec<Account>,pub history:Vec<(f64,f64)>,pub trades:Vec<(f64,usize,f64)>,pub t:f64,pub p:f64,pub reopen:f64,pub spread:f64 }
impl Flow {
 pub fn update(&mut self,t:f64,p:f64,new:Vec<(f64,String,f64,f64)>) {
  let dt=t-self.t;
  for ac in &mut self.accounts {for (j,h) in [24.,60.,120.,240.].iter().enumerate(){ac.d[j]*=(-dt*std::f64::consts::LN_2/h).exp();}}
  for (ts,key,signed_q,lim) in new {
   let i=if let Some(i)=self.accounts.iter().position(|a|a.key==key){i}else{self.accounts.push(Account{key,..Account::default()});self.accounts.len()-1};
   if i>=8 {continue;}
   let ac=&mut self.accounts[i];let a=&mut ac.a;let sg=signed_q.signum();let q=signed_q.abs();
   if a[0]>0. {let gap=ts-a[6];a[7]+=gap;a[8]+=gap*gap;a[9]=a[9].max(gap);if sg==a[14]{a[16]+=1.;}}
   else{a[5]=ts;a[12]=lim;}
   a[0]+=1.;a[1]+=signed_q;a[2]+=q;if sg>0.{a[3]+=1.;}else{a[4]+=1.;}a[6]=ts;
   a[10]+=lim;a[11]+=lim*lim;a[12]=a[12].min(lim);a[13]=a[13].max(lim);a[14]=sg;a[15]=q;a[17]=a[17].max(q);a[18]=p;
   for (j,h) in [24.,60.,120.,240.].iter().enumerate(){ac.d[j]+=signed_q*(-(t-ts)*std::f64::consts::LN_2/h).exp();}
   self.trades.push((ts,i,signed_q));
  }
  self.history.push((t,p));self.t=t;self.p=p;
 }
 pub fn features(&self,sg:f64)->Vec<f64>{
  let t=self.t;let p=self.p;
  let mut rank:Vec<usize>=(0..self.accounts.len().min(8)).collect();
  rank.sort_by(|&i,&j|self.accounts[j].a[1].abs().total_cmp(&self.accounts[i].a[1].abs()).then_with(||self.accounts[j].a[0].total_cmp(&self.accounts[i].a[0])));
  let mut f=vec![t/self.reopen,self.reopen-t,self.reopen,p*sg,self.spread,t,self.trades.len() as f64];
  for h in [10.,24.,50.,100.,200.] {let old=self.history.iter().rev().find(|(ts,_)|*ts<=t-h).map_or(0.,|(_,pr)|*pr);f.push((p-old)*sg);}
  let mut sumd=[0.;4];let mut net=0.;let mut gross=0.;
  for ac in &self.accounts {for j in 0..4{sumd[j]+=ac.d[j];}net+=ac.a[1];gross+=ac.a[2];}
  for d in sumd{f.push(d*sg);}f.extend([net*sg,gross]);
  let empty=Account::default();
  for j in 0..4 {
   let ac=rank.get(j).map(|&i|&self.accounts[i]).unwrap_or(&empty);let a=&ac.a;let n=a[0];let n1=(n-1.).max(1.);let nc=n.max(1.);
   f.extend([n,a[1]*sg,a[2],(a[3]-a[4])*sg,if n>0.{t-a[5]}else{t},if n>0.{t-a[6]}else{t},a[7]/n1,(a[8]/n1-(a[7]/n1).powi(2)).max(0.).sqrt(),a[9],a[10]/nc/self.spread,(a[11]/nc-(a[10]/nc).powi(2)).max(0.).sqrt()/self.spread,a[12]/self.spread,a[13]/self.spread,a[14]*sg,a[15],a[16]/n1,a[17],a[18]*sg]);
   for d in ac.d{f.push(d*sg);}
   for z in [1,2,3]{f.push((a[1]-ac.d[z])*sg);}
   f.extend([(p-0.14*sumd[2])*sg,(p-0.14*(sumd[2]-ac.d[2]+a[1]))*sg]);
  }
  for h in [30.,80.,160.] {
   let mut dx=0.;let mut dy=0.;let mut n=0.;
   for w in self.history.windows(2) {
    let(t0,p0)=w[0];let(t1,p1)=w[1];if t0<t-h{continue;}
    if !self.trades.iter().rev().take(40).any(|e|t0-4.<e.0&&e.0<=t1) && (p1-p0).abs()<0.045{dx+=t1-t0;dy+=p1-p0;n+=1.;}
   }
   f.extend([dy/dx.max(1.)*sg*100.,n]);
  }
  f
 }
}
