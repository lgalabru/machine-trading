#[derive(Default)]
pub struct Features {
    pub i: Vec<f64>, pub b: Vec<f64>, pub g: Vec<f64>,
    active: bool, start: usize, initial_i: f64, initial_g: f64,
    peak: f64, prior: f64, episodes: f64, score: f64, lastend: i64, tick_age: f64,
}
impl Features {
    fn ago(a:&[f64],lag:usize)->f64 {a[a.len().saturating_sub(lag+1)]}
    fn avg(a:&[f64],w:usize)->f64 {let n=a.len();a[n.saturating_sub(w)..].iter().sum::<f64>()/(w.min(n) as f64)}
    pub fn push(&mut self,i:f64,b:f64,period:f64,interval:f64,cap:f64)->(Vec<f32>,f64) {
        let g=b-i; let k=self.i.len();
        if k==0 {self.prior=0.5;self.lastend=-999;}
        if k>0 && i==self.i[k-1] && g==self.g[k-1] {self.tick_age+=0.5;} else {self.tick_age=0.0;}
        self.i.push(i);self.b.push(b);self.g.push(g);
        let orient=if g.abs()>5.0 {if g>=0.0 {1.0}else{-1.0}} else {if i-Self::avg(&self.i,16)>=0.0 {1.0}else{-1.0}};
        let mut x=Vec::with_capacity(58);x.push((g*orient) as f32);
        for lag in [1,2,4,8,12,20,32,48,80,120] {
            for a in [&self.i,&self.b,&self.g] {x.push(((a[k]-Self::ago(a,lag))*orient) as f32);}
        }
        for w in [4,8,16,32,64] {
            for a in [&self.i,&self.b,&self.g] {x.push(((a[k]-Self::avg(a,w))*orient) as f32);}
        }
        let lastg=if k>0 {self.g[k-1]}else{0.0};
        if g.abs()>15.0 && (!self.active || g*lastg<0.0) {
            if self.active {self.lastend=k as i64;}
            self.start=k;self.active=true;self.peak=g.abs();self.initial_g=g;self.initial_i=self.i[k.saturating_sub(6)];
        }
        let (mut age,mut peak,mut delta,mut g0)=(0.0,0.0,0.0,0.0);
        if self.active {
            self.peak=self.peak.max(g.abs());age=(k-self.start) as f64*0.5;peak=self.peak;delta=(i-self.initial_i)*orient;g0=self.initial_g*orient;
            if g.abs()<4.0 {
                self.score+=if (i-self.initial_i)*self.initial_g.signum()>self.initial_g.abs()*0.6 {1.0}else{0.0};
                self.episodes+=1.0;self.prior=(self.score+1.0)/(self.episodes+2.0);self.active=false;self.lastend=k as i64;
            }
        }
        for a in [age,peak,delta,g0,self.prior,((k as i64-self.lastend) as f64*0.5).min(120.0)] {x.push(a as f32);}
        for w in [16,64,160] {
            let start=(k+1).saturating_sub(w);
            let sum=(start..=k).map(|j|if j>0 {(self.i[j]-self.i[j-1]).powi(2)} else {0.0}).sum::<f64>();
            x.push((sum/((k-start+1) as f64)).sqrt() as f32);
        }
        x.push(period as f32);x.push(interval as f32);x.push(self.tick_age as f32);
        for w in [8,16,32,64] {
            let mut imin=f64::INFINITY;let mut imax=f64::NEG_INFINITY;
            for (which,a) in [&self.i,&self.g].iter().enumerate() {
                let values:Vec<f64>=(0..w).map(|j|a[(k+1+j).saturating_sub(w)]).collect();
                let mean=values.iter().sum::<f64>()/w as f64;
                let center=(w-1) as f64*0.5;
                let mut numerator=0.0;let mut denominator=0.0;
                for (j,value) in values.iter().enumerate() {let t=j as f64-center;numerator+=value*t;denominator+=t*t;}
                let slope=numerator/denominator;
                let resid=a[k]-mean-slope*center;
                x.push((slope*orient) as f32);x.push((resid*orient) as f32);
                if which==0 {
                    let rms=(values.iter().enumerate().map(|(j,value)|(value-mean-slope*(j as f64-center)).powi(2)).sum::<f64>()/w as f64).sqrt();
                    x.push(rms as f32);
                    for v in &values {imin=imin.min(*v);imax=imax.max(*v);}
                }
            }
            x.push(((i-imin)*orient) as f32);x.push(((i-imax)*orient) as f32);
        }
        x.push((g*orient/(peak+5.0)) as f32);
        x.push((delta/(peak+5.0)) as f32);
        x.push((age/period) as f32);
        x.push(x[53]/(peak as f32+5.0));
        x.push(cap as f32);x.push(((g/period).clamp(-cap,cap)*orient) as f32);x.push((cap*period) as f32);
        (x,orient)
    }
}
