use base64::Engine as _;
mod model;
mod features;
use mtx_kit::{log, prelude::*, rpc_account, venue::{PhoenixVenue, key, phoenix_rise::accounts::{perp_asset_map::PerpAssetMap,trader::Trader}}};
#[derive(Default)]
struct Bot {
 features: features::Features, period: f64, interval:f64,maxfund:f64, map: String, trader: String, asset: u64,
 lots: f64, cumulative: f64, peak: f64, stopped: bool,
 prev_pos: f64, last_side: f64, map_data:Vec<u8>, map_offset:usize,
 prev_clock:i64,pending_due:f64,pending_position:f64,pending_sign:f64,now:i64, interval_start:f64, last_update:f64, acc:f64,last_diff:f64,
}
impl Bot {
 // Cache the asset map, refreshing only the SPCX entry after initialization.
 fn funding(&mut self) {
  if self.map_data.is_empty() {
   if let Ok(Some(data))=rpc_account(&self.map) {
    for i in 0..((data.len().saturating_sub(48))/1584) {
     let off=48+i*1584;
     if &data[off..off+5]==b"SPCX\0" {self.map_offset=off;break;}
    }
    self.map_data=data;
   }
  } else if self.map_offset>0 {
   let params=serde_json::json!([&self.map,{"encoding":"base64","dataSlice":{"offset":self.map_offset,"length":1584}}]).to_string();
   if let Ok(result)=mtx_kit::rpc("getAccountInfo",&params) {
    if let Ok(v)=serde_json::from_str::<serde_json::Value>(&result) {
     if let Some(s)=v["value"]["data"][0].as_str() {
      if let Ok(d)=base64::engine::general_purpose::STANDARD.decode(s) {
       if d.len()==1584 {self.map_data[self.map_offset..self.map_offset+1584].copy_from_slice(&d);}
      }
     }
    }
   }
  }
  if let Ok(map)=PerpAssetMap::try_from_account_bytes(&self.map_data) {
   if let Ok(Some(e))=map.find_by_symbol("SPCX") {
    let f=e.metadata.funding_accumulator();
    self.period=f.funding_period_seconds.as_inner() as f64;
    self.cumulative=f.cumulative_funding_rate.as_inner() as f64;
    self.interval=f.funding_interval_seconds.as_inner() as f64;
    self.maxfund=f.max_funding_rate.as_inner() as f64*self.lots/1e6;
    self.interval_start=f.start_interval_timestamp.as_inner() as f64;
    self.last_update=f.last_funding_update_timestamp.as_inner() as f64;
    self.acc=f.acc.as_inner() as f64*self.lots/1e6;
    self.last_diff=f.last_diff.as_inner() as f64*self.lots/1e6;
   }
  }
  if let Ok(Some(data))=rpc_account("SysvarC1ock11111111111111111111111111111111") {
   if data.len()>=40 {self.now=i64::from_le_bytes(data[32..40].try_into().unwrap_or([0;8]));}
  }
 }
 // Funding already settled is in collateral; add only the pending payment.
 fn equity(&self,mid:f64)->Option<f64> {
  let data=rpc_account(&self.trader).ok()??;
  let trader=Trader::try_from_account_bytes(&data).ok()?;
  let mut value=trader.header().state().quote_lot_collateral.as_inner() as f64/1e6;
  for (asset,p) in trader.positions() {
   if asset==self.asset {
    let lots=p.base_lot_position().as_inner() as f64;
    value+=lots/self.lots*mid+p.virtual_quote_lot_position().as_inner() as f64/1e6;
    value-=(self.cumulative-p.cumulative_funding_snapshot().as_inner() as f64)*lots/1e6;
   }
  }
  Some(value)
 }
}
impl Strategy for Bot {
 fn start(&mut self,s:&Session)->BotResult {
  self.period=36.; self.peak=10000.; self.lots=1000.;
  if let Ok(v)=PhoenixVenue::from_json(&s.venue) {
   if let Ok(k)=v.perp_asset_map() {self.map=k.to_string();}
   if let Ok(k)=key(&s.authority) {self.trader=v.trader_account(&k).to_string();}
   if let Ok(m)=v.market("SPCX") {self.asset=m.asset_id as u64;self.lots=10f64.powi(m.base_lots_decimals as i32);}
  }
  self.funding(); Ok(())
 }
 fn step(&mut self,m:&Market,o:&mut Orders)->BotResult {
  if m.last {return Ok(());}
  if (m.position.size-self.prev_pos).abs()>0.1 { self.last_side=(m.position.size-self.prev_pos).signum(); }
  self.prev_pos=m.position.size;
  let mid=match (m.best_bid,m.best_ask) {
   (Some(b),Some(a)) if a-b>0.035 && self.last_side>0. =>b+0.005,
   (Some(b),Some(a)) if a-b>0.035 && self.last_side<0. =>a-0.005,
   (Some(b),Some(a))=>(b+a)*0.5,_=>m.mark
  };
  if !mid.is_finite() || !m.oracle.is_finite() || m.oracle<=0. {return Ok(());}
  self.prev_clock=self.now;self.funding();
  let equity=self.equity(mid).unwrap_or(10000.+m.pnl);
  self.peak=self.peak.max(equity);
  let dd=(self.peak-equity).max(0.);
  if dd>m.max_drawdown*0.55 {self.stopped=true;}
  let idx=m.oracle;
  let x=self.features.update(mid,idx,self.period,self.interval,self.maxfund);
  let signal=model::predict(&x);
  // Optimize inventory after estimated fees, depth impact, and a $1 no-trade band.
  let current=m.position.size;
  let edge=signal*0.01*idx;
  let terminal=|q:f64| 0.25*q.abs()*mid*(0.00035+0.000005*q.abs());
  let original=current*edge-terminal(current);
  let mut best=original;let mut target=current;
  for z in -7..=7 {
   let q=m.cap*z as f64/7.;let d=(q-current).abs();
   let value=q*edge-1.5*d*mid*(0.00035+0.000005*d)-terminal(q);
   if value>best {best=value;target=q;}
  }
  if best<original+1. {target=current;}
  // A scheduled timestamp is not proof of settlement: wait for Phoenix to advance it.
  let due=self.interval_start+self.interval;
  if self.pending_due>0. && self.interval_start>=self.pending_due {self.pending_due=0.;}
  if self.pending_due>0. {
   if self.pending_sign>0. {target=target.min(self.pending_position);}else{target=target.max(self.pending_position);}
  } else if self.now==self.prev_clock && due>self.now as f64 && due<=self.now as f64+1. {
   let payment=((self.acc+self.last_diff*(due-self.last_update).max(0.))/self.period).clamp(-self.maxfund,self.maxfund);
   let drift=signal*0.01*idx/8.+(mid-idx)/self.period;
   let gain=payment.abs()-payment.signum()*drift*1.5-mid*0.0009;
   let delta=(gain/(mid*0.000024)).max(0.).min(m.cap+payment.signum()*target);
   if delta>10. {
    target-=payment.signum()*delta;
    self.pending_due=due;self.pending_position=target;self.pending_sign=payment.signum();
   }
  }
  // Reduce exposure well before the mandate limit, reserving room for price gaps.
  let risk_scale=((m.max_drawdown*0.55-dd)/(m.max_drawdown*0.2)).clamp(0.,1.);
  target=target.clamp(-m.cap*risk_scale,m.cap*risk_scale);
  if self.stopped {target=0.;}
  if (target-m.position.size).abs()>1. {o.target_position(target);}
  log(&format!("k={} mid={:.5} idx={:.5} signal={:.5} target={:.1} pos={:.2} equity={:.3} dd={:.2}",m.k,mid,idx,signal,target,m.position.size,equity,dd));

  Ok(())
 }
}
mtx_kit::export!(Bot);
