//! Leave room for another discontinuous move and execution costs.
#[derive(Default)]
pub struct Guard {pub peak:f64,pub halted:bool}
impl Guard {
    pub fn observe(&mut self,equity:Option<f64>,limit:f64)->Option<f64> {
        let equity=equity.filter(|x|x.is_finite())?;
        self.peak=self.peak.max(equity);
        let drawdown=(self.peak-equity).max(0.0);
        if drawdown>0.62*limit {self.halted=true;}
        Some(drawdown)
    }
}
pub fn capacity(drawdown:f64,mark:f64,cap:f64,limit:f64)->f64 {
    if !drawdown.is_finite() || !mark.is_finite() || mark<=0.0 || !cap.is_finite() || cap<=0.0 || !limit.is_finite() || limit<=0.0 {return 0.0;}
    // Reserve for a 4.5% step, above the observed 3.8% bound. Leave 15%
    // of the drawdown mandate for costs and post-trade equity peaks.
    ((0.85*limit-drawdown)/(mark*0.045)).clamp(0.0,cap)
}
