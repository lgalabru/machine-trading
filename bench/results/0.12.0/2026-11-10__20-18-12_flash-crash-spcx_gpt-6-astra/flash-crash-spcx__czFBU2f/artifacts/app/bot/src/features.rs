#[derive(Default, Clone, Copy)]
struct Zig {direction:f64,anchor:f64,extreme:f64,start:usize,extreme_time:usize,previous:f64,duration:usize,count:usize}

#[derive(Default)]
pub struct Features {
    zig: [Zig;3],
    initial: f64,
    history: Vec<f64>,
    events: Vec<(usize, f64, f64)>,
    direction: f64,
    run_start: usize,
    run_anchor: f64,
    run_last: f64,
    run_count: usize,
    previous_run: f64,
}
impl Features {
    pub fn observe(&mut self, k: usize, left: u64, price: f64) -> ([f32;77],f64) {
        if self.initial == 0.0 { self.initial = price; }
        let p = 100.0*(price/self.initial-1.0);
        let previous = self.history.last().copied().unwrap_or(p);
        while self.history.len() < k { self.history.push(previous); }
        let delta = p - previous;
        self.history.push(p);
        if delta.abs() >= 0.25 {
            let s = delta.signum();
            if self.direction == 0.0 || s != self.direction {
                if self.direction != 0.0 { self.previous_run = self.run_last-self.run_anchor; }
                self.direction=s;self.run_start=k;self.run_anchor=previous;self.run_last=p;self.run_count=1;
            } else { self.run_last=p;self.run_count+=1; }
            self.events.push((k,delta,previous));
        }
        let s = if self.direction == 0.0 {1.0} else {self.direction};
        let mut x=Vec::<f64>::with_capacity(77);
        x.extend([k as f64*0.5,left as f64*0.5,(p-self.history[0])*s]);
        for lag in [1,2,3,4,6,8,12,16,24,32,48,64,96,128] {
            x.push((p-self.history[k.saturating_sub(lag)])*s);
        }
        for w in [8,16,32,64,128,512] {
            let start=(k+1).saturating_sub(w);
            let mut lo=p;let mut hi=p;
            for v in &self.history[start..=k] {lo=lo.min(*v);hi=hi.max(*v);}
            if s>0.0 {x.extend([hi-p,p-lo]);} else {x.extend([p-lo,hi-p]);}
        }
        for z in 0..4 {
            if self.events.len()>z {
                let (t,d,b)=self.events[self.events.len()-1-z];
                x.extend([(k-t).min(512) as f64,d*s,(p-b)*s]);
            } else {x.extend([512.0,0.0,0.0]);}
        }
        if self.direction==0.0 {x.extend([512.0,0.0,0.0,0.0]);}
        else {x.extend([(k-self.run_start) as f64,(p-self.run_anchor)*s,self.previous_run*s,self.run_count as f64]);}
        for w in [32,64,128,512] {
            let start=(k+1).saturating_sub(w);let mut hi=start;let mut lo=start;
            for i in start..=k {
                if self.history[i]>self.history[hi] {hi=i;}
                if self.history[i]<self.history[lo] {lo=i;}
            }
            if s>0.0 {x.extend([(k-hi) as f64,(k-lo) as f64]);}
            else {x.extend([(k-lo) as f64,(k-hi) as f64]);}
        }
        for (j,threshold) in [0.35,1.0,2.5].iter().enumerate() {
            let z=&mut self.zig[j];
            if z.direction==0.0 {
                if (p-z.anchor).abs()>=*threshold {z.direction=(p-z.anchor).signum();z.extreme=p;z.extreme_time=k;}
            } else if (p-z.extreme)*z.direction>0.0 {z.extreme=p;z.extreme_time=k;}
            else if (z.extreme-p)*z.direction>=*threshold {
                z.previous=z.extreme-z.anchor;z.duration=z.extreme_time-z.start;
                z.anchor=z.extreme;z.start=z.extreme_time;z.direction= -z.direction;
                z.extreme=p;z.extreme_time=k;z.count+=1;
            }
            x.extend([(p-z.anchor)*s,(z.extreme-z.anchor)*s,(p-z.extreme)*s,(k-z.start) as f64,(k-z.extreme_time) as f64,z.previous*s,z.duration as f64,z.count as f64]);
        }
        let mut out=[0f32;77];for (i,v) in x.iter().enumerate() {out[i]=*v as f32;}
        (out,s)
    }
}
