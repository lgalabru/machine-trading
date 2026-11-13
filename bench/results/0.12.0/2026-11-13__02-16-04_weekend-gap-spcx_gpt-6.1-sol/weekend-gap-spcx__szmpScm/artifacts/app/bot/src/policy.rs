//! Transaction-cost hysteresis and account-level loss limits.
use mtx_kit::Market;

pub const ENTRY: f64 = 0.30;
pub const EXIT: f64 = 0.20;

#[derive(Default)]
pub struct Policy {
    pub stopped: bool,
}

impl Policy {
    pub fn target(&mut self, market: &Market, mid: f64, start: f64,
                  remaining: u64, drawdown: f64, forecast: f64) -> f64 {
        let position = market.position.size;
        let liquidation_drawdown = (drawdown + position * (market.mark - mid)
            + position.abs() * mid * 0.0007).max(0.0);
        if liquidation_drawdown > market.max_drawdown * 0.63 {
            self.stopped = true;
        }
        if self.stopped { return 0.0; }

        let cap = market.cap.min(70.0);
        let mut target = position;
        if forecast > ENTRY { target = cap; }
        else if forecast < -ENTRY { target = -cap; }
        else if position > 0.0 && forecast < -EXIT { target = 0.0; }
        else if position < 0.0 && forecast > EXIT { target = 0.0; }

        if remaining <= 24 {
            let room = (market.max_drawdown * 0.90 - liquidation_drawdown).max(0.0);
            // Use the adverse of the book and mark: entering at a favorable
            // book price can create a new equity peak immediately at the mark.
            let adverse_move = if target >= 0.0 {
                (mid.max(market.mark) - start * 0.84).max(start * 0.01)
            } else {
                (start * 1.16 - mid.min(market.mark)).max(start * 0.01)
            };
            let allowed = (room / (adverse_move + mid * 0.0007)).min(cap);
            target = target.clamp(-allowed, allowed);
        }
        target.clamp(-cap, cap)
    }
}

/// Required risk reductions bypass the normal minimum rebalance size.
pub fn needs_trade(position: f64, target: f64, remaining: u64, stopped: bool) -> bool {
    let delta = (target - position).abs();
    delta > 4.0 || (delta >= 0.1 && (stopped
        || (remaining <= 24 && target.abs() < position.abs())))
}

#[cfg(test)]
mod tests {
 use super::*;
 #[test]
 fn protects_the_open_even_when_the_signal_is_wrong(){
  for mark in [134.,143.,152.] {for sign in [-1.,1.] {
   let m=Market{mark,cap:70.,max_drawdown:1000.,..Market::default()};let mut p=Policy::default();
   let q=p.target(&m,mark,143.,2,100.,sign*3.);
   let worst=if q>0.{143.*0.84}else{143.*1.16};
   assert!(100.+q*(mark-worst)+q.abs()*mark*0.0007<=900.0001);
  }}
 }
 #[test]
 fn a_favorable_mark_gap_cannot_create_an_unprotected_equity_peak(){
  for (mark,mid,signal) in [(138.,134.,3.),(148.,152.,-3.)] {
   let m=Market{mark,cap:70.,max_drawdown:1000.,..Market::default()};
   let mut p=Policy::default();let q=p.target(&m,mid,143.,2,0.,signal);
   let worst=if q>0.{143.*0.84}else{143.*1.16};
   assert!(q*(mark-worst)<=900.0001);
  }
 }
 #[test]
 fn small_risk_reductions_must_execute(){
  assert!(needs_trade(30.,29.,2,false));
  assert!(!needs_trade(30.,29.,100,false));
  assert!(needs_trade(0.5,0.,100,true));
  assert!(!needs_trade(0.,0.,2,false));
 }
 #[test]
 fn reopening_stress_includes_an_existing_position_and_book_gap(){
  for old_position in [-70.,70.] {for mark in [134.,143.,152.] {for gap in [-0.5,0.5] {for dd in [0.,300.,600.] {for direction in [-1.,1.] {
   let mid=mark+gap;
   let m=Market{mark,cap:70.,max_drawdown:1000.,position:mtx_kit::Position{size:old_position,entry_usd:143.},..Market::default()};
   let mut p=Policy::default();let q=p.target(&m,mid,143.,2,dd,direction*3.);
   let worst=if q>=0.{143.*0.84}else{143.*1.16};
   let stress=dd+old_position*(mark-mid)+q*(mid-worst)+(old_position.abs()+q.abs())*mid*0.0007;
   assert!(stress.max(q*(mark-worst))<=900.0001,"stress {stress}, old {old_position}, new {q}");
  }}}}}
 }
 #[test]
 fn a_loss_stop_stays_stopped(){
  let m=Market{mark:143.,cap:70.,max_drawdown:1000.,..Market::default()};let mut p=Policy::default();
  assert_eq!(p.target(&m,143.,143.,100,640.,2.),0.);
  assert_eq!(p.target(&m,143.,143.,100,0.,2.),0.);
 }
}
