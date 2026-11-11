//! Causal oracle-history features. Their order is part of the embedded model format.
//! Values are rounded to f32 to reproduce the training matrix at tree boundaries.
#[derive(Default)]
pub struct Features {
    prices:Vec<f64>,initial:f64,last_sig:i64,sig:f64,direction:f64,
    anchor:f64,run_start:usize,peak:f64,trough:f64,emas:[f64;4],count:usize,
    peak_k:usize,trough_k:usize,events:Vec<(usize,f64)>,latest:[(i64,f64);3],biggest:f64,tv:Vec<f64>,
}
impl Features {
    pub fn next(&mut self,price:f64,left:u64)->([f64;58],f64){
        if self.prices.is_empty(){
            self.initial=price;self.anchor=1.;self.peak=1.;self.trough=1.;self.emas=[1.;4];self.direction=1.;self.last_sig=-10000;self.latest=[(-10000,0.);3];
        }
        let cur=price/self.initial;let k=self.prices.len();let prev=self.prices.last().copied().unwrap_or(cur);let delta=cur-prev;
        if delta.abs()>0.0025{
            if k as i64-self.last_sig>12||delta*self.direction<0.{self.anchor=prev;self.run_start=k;}
            self.direction=delta.signum();self.sig=delta;self.last_sig=k as i64;self.count+=1;self.events.push((k,delta));
        }
        for(j,threshold)in [0.00075,0.005,0.01].iter().enumerate(){if delta.abs()> *threshold{self.latest[j]=(k as i64,delta);}}
        if delta.abs()>self.biggest.abs(){self.biggest=delta;}
        self.prices.push(cur);self.tv.push(self.tv.last().copied().unwrap_or(0.)+delta.abs());
        if cur>self.peak{self.peak=cur;self.peak_k=k;}
        if cur<self.trough{self.trough=cur;self.trough_k=k;}
        for(i,a)in [0.4,0.12,0.035,0.01].iter().enumerate(){self.emas[i]+=(cur-self.emas[i])*a;}
        let d=self.direction;let age=(k as i64-self.last_sig).min(200)as f64;
        let a=d*(self.peak-1.);let b=d*(self.trough-1.);
        let mut x=[0.;58];
        x[..10].copy_from_slice(&[k as f64*0.5,left as f64*0.5,age*0.5,(k-self.run_start)as f64*0.5,self.count.min(20)as f64,d*(cur-1.),d*self.sig,d*(cur-self.anchor),a.max(b),a.min(b)]);
        let mut j=10;
        for w in [1,2,3,4,6,8,12,16,24,32,48,64,96,128]{x[j]=d*(cur-self.prices[k.saturating_sub(w)]);j+=1;}
        for e in self.emas{x[j]=d*(cur-e);j+=1;}
        for w in [8,24,64,128]{let slice=&self.prices[k.saturating_sub(w)..=k];let lo=slice.iter().copied().fold(f64::INFINITY,f64::min);let hi=slice.iter().copied().fold(f64::NEG_INFINITY,f64::max);let a=d*(cur-lo);let b=d*(cur-hi);x[j]=a.max(b);x[j+1]=a.min(b);j+=2;}
        x[j]=(k-if d>0.{self.peak_k}else{self.trough_k})as f64*0.5;j+=1;
        x[j]=(k-if d>0.{self.trough_k}else{self.peak_k})as f64*0.5;j+=1;
        for w in [8,24,64,128]{x[j]=self.tv[k]-self.tv[k.saturating_sub(w)];j+=1;}
        for z in 0..4{if self.events.len()>z{let(t,v)=self.events[self.events.len()-z-1];x[j]=d*v;x[j+1]=(k-t).min(200)as f64*0.5;}else{x[j]=0.;x[j+1]=100.;}j+=2;}
        for(t,v)in self.latest{x[j]=d*v;x[j+1]=(k as i64-t).min(200)as f64*0.5;j+=2;}
        x[j]=self.biggest.abs();x[j+1]=d*self.biggest;
        for v in &mut x{*v=(*v as f32)as f64;}
        (x,d)
    }
}
