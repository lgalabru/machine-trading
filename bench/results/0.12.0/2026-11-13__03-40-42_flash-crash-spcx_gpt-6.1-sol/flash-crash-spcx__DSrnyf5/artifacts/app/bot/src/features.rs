#[derive(Clone)]
pub struct Features {
    prices: Vec<f64>,
    last: [i64;3],
    magnitude: [f64;3],
    previous_magnitude: [f64;3],
    gap: [i64;3],
    ema: [f64;4],
}
impl Default for Features {
    fn default()->Self {Self{prices:Vec::new(),last:[-100;3],magnitude:[0.0;3],previous_magnitude:[0.0;3],gap:[0;3],ema:[0.0;4]}}
}
impl Features {
    pub fn update(&mut self,p:f64,steps:u64)->([f32;35],f64) {
        let i=self.prices.len();
        let prior=self.prices.last().copied().unwrap_or(0.0);
        let change=p-prior;
        for (j,threshold) in [0.001,0.002,0.004].iter().enumerate() {
            if change.abs()>*threshold {
                self.previous_magnitude[j]=self.magnitude[j];
                self.gap[j]=i as i64-self.last[j];
                self.last[j]=i as i64;
                self.magnitude[j]=change;
            }
        }
        self.prices.push(p);
        let direction=if self.magnitude[1]<0.0 {-1.0} else {1.0};
        let mut out=[0.0f32;35];
        out[0]=(i as f64*0.5) as f32;
        out[1]=(steps.saturating_sub(i as u64) as f64*0.5) as f32;
        out[2]=(p*direction) as f32;
        let mut col=3;
        for j in 0..3 {
            out[col]=((i as i64-self.last[j]).min(100)) as f32;
            out[col+1]=(self.magnitude[j]*direction) as f32;
            out[col+2]=(self.previous_magnitude[j]*direction) as f32;
            out[col+3]=self.gap[j].min(100) as f32;
            col+=4;
        }
        for lag in [1usize,2,3,4,6,8,12,20,40,80] {
            let old=if i>=lag {self.prices[i-lag]} else {0.0};
            out[col]=((p-old)*direction) as f32; col+=1;
        }
        for (j,h) in [4.0,12.0,40.0,120.0].iter().enumerate() {
            self.ema[j]+=(p-self.ema[j])/h;
            out[col]=((p-self.ema[j])*direction) as f32;col+=1;
        }
        for h in [8usize,24,80] {
            let window=&self.prices[(i+1).saturating_sub(h)..];
            let hi=window.iter().copied().fold(f64::NEG_INFINITY,f64::max);
            let lo=window.iter().copied().fold(f64::INFINITY,f64::min);
            out[col]=(if direction>0.0 {hi-p} else {p-lo}) as f32;
            out[col+1]=(if direction>0.0 {p-lo} else {hi-p}) as f32;
            col+=2;
        }
        (out,direction)
    }
}
