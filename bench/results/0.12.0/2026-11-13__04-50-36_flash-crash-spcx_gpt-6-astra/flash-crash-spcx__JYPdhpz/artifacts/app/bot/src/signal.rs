const LAGS: [usize;14] = [1,2,3,4,6,8,12,16,24,32,48,64,96,128];
const WINDOWS: [usize;4] = [8,20,60,120];
const EMAS: [f64;5] = [4.,12.,32.,64.,128.];
const THRESHOLDS: [f64;5] = [0.10,0.25,0.5,1.0,2.0];
#[derive(Default)]
pub struct Signal {
    history: Vec<f64>,
    direction: f64,
    emas: [f64;5],
    last: [Option<(usize,f64)>;5],
    impulses: Vec<(usize,f64)>,
    run_base: f64,
    run_begin: usize,
    run_count: usize,
    run_direction: f64,
    low: f64,
    high: f64,
}
impl Signal {
    pub fn update(&mut self, p: f64, steps_left: u64) -> ([f32;54],f64) {
        let k = self.history.len();
        let prev = self.history.last().copied().unwrap_or(0.0);
        let delta = p-prev;
        for j in 0..5 {
            if delta.abs()>THRESHOLDS[j] {self.last[j]=Some((k,delta));}
            self.emas[j]+=(p-self.emas[j])/EMAS[j];
        }
        if delta.abs()>0.25 {
            self.direction=delta.signum();
            self.impulses.push((k,delta));
            if self.direction!=self.run_direction {
                self.run_base=prev;self.run_begin=k;self.run_count=0;self.run_direction=self.direction;
            }
            self.run_count+=1;
        }
        if self.direction==0.0 {self.direction=1.0;}
        let d=self.direction;
        self.history.push(p);self.low=self.low.min(p);self.high=self.high.max(p);
        let mut x=[0f32;54];let mut i=0;
        let mut push=|v:f64| {x[i]=v as f32;i+=1;};
        push(k as f64);push(steps_left as f64);push(p*d);
        for lag in LAGS {
            let prev=if k>=lag {self.history[k-lag]} else {0.0};
            push((p-prev)*d);
        }
        for window in WINDOWS {
            let mut low=if self.history.len()<window {0.0} else {p};
            let mut high=low;
            for &v in self.history.iter().rev().take(window) {low=low.min(v);high=high.max(v);}
            if d>0. {push(high-p);push(p-low);} else {push(p-low);push(high-p);}
        }
        if d>0. {push(self.high-p);push(p-self.low);} else {push(p-self.low);push(self.high-p);}
        for ema in self.emas {push((p-ema)*d);}
        for last in self.last {
            if let Some((at,change))=last {push((k-at) as f64);push(change*d);}
            else {push(1000.);push(0.);}
        }
        for j in 0..4 {
            if self.impulses.len()>j {
                let (at,change)=self.impulses[self.impulses.len()-1-j];
                push(change*d);push((k-at) as f64);
            } else {push(0.);push((k+1000) as f64);}
        }
        push((p-self.run_base)*d);push((k-self.run_begin) as f64);push(self.run_count as f64);push(self.impulses.len() as f64);
        (x,d)
    }
}
