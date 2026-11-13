//! SPCX funding and price discovery strategy.
//! Forecasts are trained on independently generated market paths. The strategy
//! uses book/oracle history, session funding parameters and completed episodes.
mod features;
mod model;
use features::Features;
use mtx_kit::{log, prelude::*, rpc, rpc_account, venue::{PhoenixVenue, key, phoenix_rise::accounts::{perp_asset_map::PerpAssetMap,trader::Trader}}};
use base64::Engine;

#[derive(Default)]
struct Bot {
    features: Features, scale: f64, period: f64, interval: f64, funding_cap: f64,
    trader: String, map_address: String, map_data: Vec<u8>, map_offset: usize,
    lot_scale: f64, cumulative: f64, projected: f64,
    peak_equity: f64, equity: f64, stopped: bool,
    last_i: f64, last_b: f64, target: f64,
}
impl Bot {
    fn funding(&mut self) {
        if self.map_data.is_empty() { return; }
        if let Ok(answer)=rpc("getAccountInfo", &serde_json::json!([self.map_address,{"encoding":"base64","dataSlice":{"offset":self.map_offset,"length":1584}}]).to_string()) {
            if let Ok(v)=serde_json::from_str::<serde_json::Value>(&answer) {
                if let Some(s)=v["value"]["data"][0].as_str() {
                    if let Ok(data)=base64::engine::general_purpose::STANDARD.decode(s) {
                        if data.len()==1584 { self.map_data[self.map_offset..self.map_offset+1584].copy_from_slice(&data); }
                    }
                }
            }
        }
        if let Ok(map)=PerpAssetMap::try_from_account_bytes(&self.map_data) {
            if let Ok(Some(e))=map.find_by_symbol("SPCX") {
                let f=e.metadata.funding_accumulator();
                self.cumulative=f.cumulative_funding_rate.as_inner() as f64;
                self.projected=(f.acc.as_inner() as f64/self.period).clamp(-(f.max_funding_rate.as_inner() as f64),f.max_funding_rate.as_inner() as f64);
            }
        }
    }
    fn risk(&mut self,b:f64)->Option<f64> {
        self.funding();
        let data=rpc_account(&self.trader).ok()??;
        let trader=Trader::try_from_account_bytes(&data).ok()?;
        let mut equity=trader.header().state().quote_lot_collateral.as_inner() as f64/1e6;
        for (_,p) in trader.positions() {
            let p=p.to_position();
            let lots=p.base_lot_position.as_inner() as f64;
            equity+=lots/self.lot_scale*b+p.virtual_quote_lot_position.as_inner() as f64/1e6;
            equity-=lots*(self.cumulative-p.cumulative_funding_snapshot.as_inner() as f64)/1e6;
        }
        Some(equity)
    }
}
impl Strategy for Bot {
    fn start(&mut self,s:&Session)->BotResult {
        self.scale=10000.0/s.start_mark;self.period=36.0;self.interval=4.0;self.lot_scale=1000.0;self.peak_equity=10000.0;self.equity=10000.0;
        let venue=PhoenixVenue::from_json(&s.venue)?;
        self.trader=venue.trader_account(&key(&s.authority)?).to_string();
        self.map_address=venue.perp_asset_map()?.to_string();
        if let Some(data)=rpc_account(&self.map_address)? {
            let map=PerpAssetMap::try_from_account_bytes(&data)?;
            if let Some(e)=map.find_by_symbol("SPCX")? {
                let f=e.metadata.funding_accumulator();self.period=f.funding_period_seconds.as_inner() as f64;self.interval=f.funding_interval_seconds.as_inner() as f64;
                self.lot_scale=10f64.powi(e.metadata.static_market_params().base_lot_decimals as i32);
                self.cumulative=f.cumulative_funding_rate.as_inner() as f64;
                self.funding_cap=f.max_funding_rate.as_inner() as f64*self.lot_scale/1e6*self.scale/self.interval;
            }
            for offset in (48..data.len().saturating_sub(1583)).step_by(1584) {
                if data[offset..offset+4]==*b"SPCX" {self.map_offset=offset;break;}
            }
            if self.map_offset>0 {self.map_data=data;}
        }
        log(&format!("funding period={} interval={} lots={}",self.period,self.interval,self.lot_scale));
        Ok(())
    }
    fn step(&mut self,m:&Market,o:&mut Orders)->BotResult {
        let raw=match(m.best_bid,m.best_ask){(Some(b),Some(a))=>(a+b)*0.5,_=>m.mark};
        let i=if m.oracle>0.0 {m.oracle}else{m.index};
        if !(raw>0.0 && i>0.0) {return Ok(());}
        // Consuming the book moves its best prices without moving the makers'
        // centre. Keep that centre between oracle ticks, except for new jumps.
        let b=if self.last_i==i && (raw-self.last_b).abs()<i*0.0015 {self.last_b}else{raw};
        self.last_i=i;self.last_b=b;
        let (x,orientation)=self.features.push(i*self.scale,b*self.scale,self.period,self.interval,self.funding_cap);
        let forecasts=model::predict(&x).map(|p|p*orientation);
        let signal=forecasts[0]*0.375+forecasts[1]*0.125+forecasts[2]*0.5;
        let mut target=m.position.size;
        if signal>10.0 {target=m.cap;}
        else if signal< -10.0 {target=-m.cap;}
        else if (target>0.0 && signal< -7.0)||(target<0.0 && signal>7.0) {target=0.0;}
        let mut dd=0.0;
        if let Some(e)=self.risk(b) {
            self.equity=e;self.peak_equity=self.peak_equity.max(e);dd=(self.peak_equity-e).max(0.0);
            if dd>650.0 {self.stopped=true;}
            let limit=((850.0-dd)/(b*0.045)).clamp(0.0,m.cap);
            target=target.clamp(-limit,limit);
        } else {target=0.0;}
        if self.stopped {target=0.0;}
        if m.steps_left<8 && !self.stopped {
            if m.position.size==0.0 || target.signum()!=m.position.size.signum() {target=m.position.size;}
        }
        self.target=target;
        if !m.last {
            o.target_position_with(target,Execution {max_per_step:140.0,slice:5.0,slippage_bps:400.0,min_trade:1.0});
        }
        if m.k%20==0 || (target-m.position.size).abs()>1.0 {
            log(&format!("k={} b={:.4} i={:.4} f={:?} pos={:.1} target={:.1} equity={:.2} dd={:.2}",m.k,b,i,forecasts,m.position.size,target,self.equity,dd));
        }
        Ok(())
    }
}
mtx_kit::export!(Bot);
