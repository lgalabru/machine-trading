//! Oracle-anchored SPCX market maker with public order-flow monitoring.
use mtx_kit::prelude::*;
use mtx_kit::{Pubkey, Trader, phoenix_rise};
use phoenix_rise::accounts::{orderbook::Orderbook, trader::Trader as TraderView};
use solana_rpc_client_api::request::RpcRequest;
use serde_json::{Value,json};
use std::{collections::VecDeque, str::FromStr};

struct Config(Value);
impl Default for Config {fn default()->Self{Self(Value::Null)}}
impl Config {
    fn load()->Self {
        let v=std::env::current_exe().ok().and_then(|p|p.parent()?.parent().map(|x|x.join("config.json")))
            .and_then(|p|std::fs::read_to_string(p).ok()).and_then(|s|serde_json::from_str(&s).ok()).unwrap_or(Value::Null);
        Self(v)
    }
    fn get(&self,k:&str,d:f64)->f64 {self.0[k].as_f64().unwrap_or(d)}
}
#[derive(Default)]
struct Forest { base:f64, trees:Vec<Vec<(usize,f64,usize,usize,f64,bool)>> }
impl Forest {
    fn load()->Self {
        let v:Value=serde_json::from_str(include_str!("../model.json")).expect("embedded model");
        let trees=v["trees"].as_array().unwrap().iter().map(|tree|tree.as_array().unwrap().iter().map(|n| (n[0].as_u64().unwrap() as usize,n[1].as_f64().unwrap(),n[2].as_u64().unwrap() as usize,n[3].as_u64().unwrap() as usize,n[4].as_f64().unwrap(),n[5].as_u64().unwrap()!=0)).collect()).collect();
        Self{base:v["base"].as_f64().unwrap(),trees}
    }
    fn predict(&self,x:&[f64])->f64 {
        let mut value=self.base;
        for tree in &self.trees {
            let mut i=0;
            loop {let &(f,t,l,r,v,leaf)=&tree[i]; if leaf {value+=v;break;} i=if x[f]<=t {l}else{r};}
        }
        1.0/(1.0+(-value).exp())
    }
}
#[derive(Default)]
struct Maker {
    cfg: Config, forest:Forest, cumret:f64, sqret:f64, moves:f64, buys:f64, sells:f64, last_jump:u64, last_signal:u64, trend_pos:f64,
    tick:f64, lot:f64, asset:u64, book_key:Pubkey, own:String,
    start_slot:u64, jumps:u64, last_slot:u64, last_block:u64, last_price:f64, last_q:f64,
    last_bid:f64,last_ask:f64,own_id:u32,
    half:f64, drift:f64, variance:f64, fill_flow:f64,
    peak:f64, equity:f64, initial_equity:f64, stopped:bool,
    flow:VecDeque<(u64,f64,f64)>,
    signal:f64, signal_until:u64, cooldown:u64,
    total_orders:f64, last_active:bool, danger_dir:f64, danger_until:u64,
}
impl Maker {
    fn read_flow(&mut self,t:&Trader,slot:u64) {
        let first=if self.last_block==0 {slot.saturating_sub(2)} else {self.last_block+1};
        let tag=phoenix_rise::ix::PhoenixInstruction::PlaceMarketOrder.discriminant();
        for bslot in first.max(slot.saturating_sub(6))..slot {
            let result:Result<Value,_>=t.rpc.send(RpcRequest::GetBlock,json!([bslot,{"encoding":"json","transactionDetails":"full","rewards":false,"maxSupportedTransactionVersion":0,"commitment":"confirmed"}]));
            let Ok(b)=result else {continue};
            if b.is_null(){continue;}
            self.last_block=bslot;
            let Some(txs)=b["transactions"].as_array() else {continue};
            for tx in txs {
                if !tx["meta"]["err"].is_null(){continue;}
                let msg=&tx["transaction"]["message"];
                let Some(keys)=msg["accountKeys"].as_array() else {continue};
                if keys.first().and_then(Value::as_str)==Some(&self.own){continue;}
                let Some(ixs)=msg["instructions"].as_array() else {continue};
                for ix in ixs {
                    let Some(data)=ix["data"].as_str().and_then(|s|bs58::decode(s).into_vec().ok()) else {continue};
                    if data.len()<19 || data[..8]!=tag || data[8]!=2 {continue;}
                    // Restrict observations to this market, even if other markets exist.
                    let key=self.book_key.to_string();
                    if !ix["accounts"].as_array().is_some_and(|a|a.iter().any(|v| v.as_u64().and_then(|n|keys.get(n as usize)).and_then(Value::as_str)==Some(&key))){continue;}
                    let side=if data[9]==0 {1.0}else{-1.0};
                    let offset=if data[10]==1 {19}else{11};
                    if data.len()<offset+8 {continue;}
                    let size=u64::from_le_bytes(data[offset..offset+8].try_into().unwrap()) as f64*self.lot;
                    self.flow.push_back((bslot,side,size));
                    self.total_orders+=1.0;
                    if side>0.0 {self.buys+=1.0;}else{self.sells+=1.0;}
                    if self.cfg.get("debug_flow",0.0)>0.0 {eprintln!("FLOW,{bslot},{side},{size}");}
                }
            }
        }
        while self.flow.front().is_some_and(|x|slot.saturating_sub(x.0)>80){self.flow.pop_front();}
    }
    fn book_and_equity(&mut self,t:&Trader,p:f64)->Option<(f64,f64)> {
        let accounts=t.rpc.get_multiple_accounts(&[self.book_key,t.account]).ok()?;
        if let Some(a)=&accounts[1] {
            if let Ok(v)=TraderView::try_from_account_bytes(&a.data) {
                let mut eq=v.header().state().quote_lot_collateral.as_inner() as f64/1e6;
                for (asset,pos) in v.positions(){if asset==self.asset {
                    eq+=pos.virtual_quote_lot_position().as_inner() as f64/1e6;
                    eq+=pos.base_lot_position().as_inner() as f64*self.lot*p;
                }}
                self.equity=eq;
                if self.initial_equity==0.0 {self.initial_equity=eq;self.peak=eq;}
                self.peak=self.peak.max(eq);
            }
        }
        let a=accounts[0].as_ref()?;
        let b=Orderbook::try_from_account_bytes(&a.data).ok()?;
        // Identify our resting order from the previous turn before comparing the book.
        if self.own_id==0 && self.last_slot>0 {
            let mut newest=0;
            for x in b.bid_orders().chain(b.ask_orders()) {
                let price=x.price_in_ticks().as_inner() as f64*self.tick;
                if x.order().initial_slot()==self.last_slot && ((price-self.last_bid).abs()<self.tick*0.1 || (price-self.last_ask).abs()<self.tick*0.1) {
                    if x.raw_sequence_number()>=newest {self.own_id=x.order().trader_position_id().trader_id().to_u32_or(0);newest=x.raw_sequence_number();}
                }
            }
        }
        let bid=b.bid_orders().find(|x|self.own_id==0 || x.order().trader_position_id().trader_id().to_u32_or(0)!=self.own_id)
            .map(|x|x.price_in_ticks().as_inner() as f64*self.tick).unwrap_or(p*(1.0-self.half));
        let ask=b.ask_orders().find(|x|self.own_id==0 || x.order().trader_position_id().trader_id().to_u32_or(0)!=self.own_id)
            .map(|x|x.price_in_ticks().as_inner() as f64*self.tick).unwrap_or(p*(1.0+self.half));
        Some((bid,ask))
    }
    fn burst(&self,slot:u64,k:u64)->(f64,f64) {
        let rate=(self.total_orders/(k.max(50) as f64*2.0)).clamp(0.20,0.50)/2.0;
        let mut best=(0.0,0.0);
        for width in [4,6,10,16,24,32] {
            let mut buys=0.0;let mut sells=0.0;
            for &(s,sign,_) in &self.flow {if slot.saturating_sub(s)<=width {if sign>0.0 {buys+=1.0}else{sells+=1.0}}}
            for (sign,n,other) in [(1.0,buys,sells),(-1.0,sells,buys)] {
                let expected=rate*width as f64;
                let excess=(n-expected).max(0.0);
                let score=if n>expected {n*(n/expected).ln()-excess}else{0.0};
                let score=score-self.cfg.get("opposing_penalty",0.25)*other;
                if score>best.1 {best=(sign,score);}
            }
        }
        best
    }
}
impl Strategy for Maker {
    async fn start(&mut self,s:&Session)->BotResult {
        self.cfg=Config::load();
        self.forest=Forest::load();
        self.start_slot=s.hello.start_slot;
        let m=s.trader.phoenix.market(&s.env.symbol)?;
        self.tick=s.trader.phoenix.ticks_to_usd(m,1);
        self.lot=10f64.powi(-m.base_lots_decimals as i32);
        self.asset=m.asset_id as u64;
        self.book_key=Pubkey::from_str(&m.market_pubkey)?;
        self.own=s.trader.authority().to_string();
        self.half=0.0015;
        Ok(())
    }
    async fn step(&mut self,m:&Market,o:&mut Orders<'_>)->BotResult {
        let p=m.oracle;let q=m.position.size;
        if !(p>0.0 && p.is_finite()) {o.cancel_all();return Ok(());}
        let mut bb=p*(1.0-self.half);let mut ba=p*(1.0+self.half);
        if let Some(t)=o.trader(){
            if let Some((b,a))=self.book_and_equity(t,p) {bb=b;ba=a;}
            self.read_flow(t,m.slot);
        }
        if m.k==0 {self.half=((ba-bb)/p/2.0).clamp(0.0008,0.003);}
        let mut jumped=false;
        if self.last_price>0.0 {
            let r=p/self.last_price-1.0;
            jumped=r.abs()>0.007;
            if jumped {
                self.jumps+=1;self.last_jump=m.slot;self.trend_pos=0.0;self.danger_until=0;self.danger_dir=0.0;
                self.flow.clear(); self.signal=0.0; self.signal_until=0;self.cooldown=m.slot+36;
            } else {
                self.drift=self.drift*self.cfg.get("drift_decay",0.975)+(1.0-self.cfg.get("drift_decay",0.975))*r;
                self.variance=self.variance*0.97+0.03*r*r;
                if r!=0.0 {self.cumret+=r;self.sqret+=r*r;self.moves+=1.0;}
            }
        }
        self.fill_flow=0.86*self.fill_flow-if self.last_active {0.0}else{q-self.last_q};
        let elapsed=m.slot.saturating_sub(self.start_slot) as f64;
        let mut predictions=[0.0;2];
        for (i,sign) in [1.0,-1.0].iter().enumerate() {
            let mut x=vec![elapsed,m.slots_left as f64,elapsed/(elapsed+m.slots_left as f64).max(1.0),self.jumps as f64,if self.jumps==0 {200.0}else{m.slot.saturating_sub(self.last_jump).min(200) as f64},self.total_orders/elapsed.max(100.0),self.moves,self.sqret,*sign*self.cumret,*sign*self.drift,*sign*(self.buys-self.sells)/self.total_orders.max(1.0).sqrt()];
            for width in [4,6,10,16,24,32,48,64] {
                let mut same=0.0;let mut other=0.0;
                for &(slot,direction,_) in &self.flow {if m.slot.saturating_sub(slot)<=width {if direction==*sign {same+=1.0;}else{other+=1.0;}}}
                x.push(same);x.push(other);
            }
            for side in [*sign,-*sign] {x.push(self.flow.iter().filter(|z|z.1==side).map(|z|m.slot.saturating_sub(z.0) as f64).fold(100.0,f64::min));}
            predictions[i]=self.forest.predict(&x);
        }
        let (burst_direction,burst_score)=self.burst(m.slot,m.k);
        if burst_score>self.cfg.get("withdraw_score",3.0) && m.slots_left>=60 && m.slot>=self.cooldown && self.jumps<3 {
            self.danger_dir=burst_direction;self.danger_until=m.slot+10;
        }
        if m.slot>=self.danger_until {self.danger_dir=0.0;}
        let probability=predictions[0]-predictions[1];
        let score=probability*10.0;
        let threshold=self.cfg.get("ml_enter",0.16)+self.cfg.get("ml_cost",1.5)*self.half/0.027;
        if m.slot>=self.cooldown && elapsed>=30.0 && m.slots_left>=60 && self.jumps<3 {
            if probability.abs()>threshold || burst_score>self.cfg.get("hybrid_score",10.0) {
                self.signal=if probability.abs()>threshold {probability.signum()}else{burst_direction};self.last_signal=m.slot;
            } else if m.slot.saturating_sub(self.last_signal)>self.cfg.get("ml_hold",40.0) as u64 && probability*self.signal<self.cfg.get("ml_exit",0.02) {
                self.signal=0.0;
            }
        }
        if m.slots_left<60 || self.jumps>=3 {self.signal=0.0;}
        if self.last_signal>0 {self.signal_until=if self.signal!=0.0 {u64::MAX}else{m.slot};}
        // Bayesian drift forecast, combining price changes with the noise flow bias.
        if self.cfg.get("forecast",1.5)>0.0 && self.jumps==0 && self.moves>=5.0 && m.slots_left>50 {
            let n=self.moves;
            let var=((self.sqret-self.cumret*self.cumret/n+20.0*2.5e-7)/(n+20.0)).max(4e-8);
            let (mut lp,mut lm,mut mp,mut mm)=(0.0,0.0,0.0,0.0);
            for mu in [0.00006,0.00009,0.00012,0.00015,0.00018] {
                let up=(mu*self.cumret/var-n*mu*mu/(2.0*var)).clamp(-50.0,50.0).exp()/5.0;
                let dn=(-mu*self.cumret/var-n*mu*mu/(2.0*var)).clamp(-50.0,50.0).exp()/5.0;
                lp+=up;lm+=dn;mp+=mu*up;mm+=mu*dn;
            }
            let (mut fp,mut fm)=(0.0,0.0);
            for probability in [0.55_f64,0.5675,0.585,0.6025,0.62] {
                fp+=(self.buys*(2.0*probability).ln()+self.sells*(2.0*(1.0-probability)).ln()).clamp(-50.0,50.0).exp()/5.0;
                fm+=(self.sells*(2.0*probability).ln()+self.buys*(2.0*(1.0-probability)).ln()).clamp(-50.0,50.0).exp()/5.0;
            }
            let weight=self.cfg.get("forecast_flow",1.0);
            let upflow=fp.powf(weight);let downflow=fm.powf(weight);
            lp*=upflow;lm*=downflow;mp*=upflow;mm*=downflow;
            let forecast=(mp-mm)/(self.cfg.get("forecast_prior",6.0)+lp+lm)*m.slots_left as f64/6.0;
            let cost=self.cfg.get("forecast",1.5)*(self.half+0.00075);
            if forecast>cost {self.trend_pos=1.0;}else if forecast< -cost {self.trend_pos=-1.0;}
        }
        let directional=if self.signal!=0.0 {self.signal}else{self.trend_pos};
        let dd=(self.peak-self.equity).max(0.0);
        if dd>self.cfg.get("stop_dd",100.0).min(m.max_drawdown*0.45) {self.stopped=true;}
        let cap=m.cap.min(((m.max_drawdown-dd-35.0)/(p*0.045)).max(0.0));
        let neutral_cap=self.cfg.get("inventory_cap",6.0).min(cap);
        let drift_target=(self.drift*self.cfg.get("drift_gain",100000.0)).clamp(-self.cfg.get("trend_cap",8.0),self.cfg.get("trend_cap",8.0));
        let target=if directional!=0.0 {directional*cap*self.cfg.get("signal_fraction",0.95)}else{drift_target};
        let mut active=false;
        o.cancel_all();
        if self.stopped {
            let mut remaining=q.abs();
            while remaining>=self.lot {
                let size=remaining.min(8.0);
                if q>0.0 {o.sell(size,p*0.992);}else{o.buy(size,p*1.008);}
                remaining-=size;
            }
            self.last_q=q;self.last_price=p;self.last_slot=m.slot;return Ok(());
        }
        // Only cross the spread when the public flow implies a discrete jump.
        if self.cfg.get("active",1.0)>0.0 && (directional!=0.0 || jumped || (self.signal_until>0 && m.slot>=self.signal_until && q.abs()>neutral_cap+1.0)) && (target-q).abs()>1.0 {
            let goal=if directional!=0.0 {target}else{q.clamp(-neutral_cap,neutral_cap)};
            let size=(goal-q).abs().min(self.cfg.get("active_slice",19.0));
            if goal>q {o.buy(size,p*(1.0+self.cfg.get("active_slip_bps",45.0)/1e4));}else{o.sell(size,p*(1.0-self.cfg.get("active_slip_bps",45.0)/1e4));}
            active=true;
        }
        if !active {
            let floor=self.cfg.get("min_half_bps",2.5)/1e4;
            let shift=(target-q)*self.cfg.get("inventory_skew_bps",0.8)/1e4+self.fill_flow.clamp(-4.0,4.0)*self.cfg.get("fill_skew_bps",0.4)/1e4;
            let bid_dist=((p-bb-self.tick)/p).clamp(floor,self.half);
            let ask_dist=((ba-p-self.tick)/p).clamp(floor,self.half);
            let bid=(p*(1.0-bid_dist+shift.min(0.0))).min(p*(1.0-floor));
            let ask=(p*(1.0+ask_dist+shift.max(0.0))).max(p*(1.0+floor));
            let size=self.cfg.get("quote_size",2.5);
            let position_limit=if directional!=0.0 {cap}else{neutral_cap.max(drift_target.abs()+2.0)};
            let bs=((position_limit-q).min(size).max(0.0)/self.lot).floor()*self.lot;
            let az=((position_limit+q).min(size).max(0.0)/self.lot).floor()*self.lot;
            let bid_ticks=(bid/self.tick+1e-8).floor();
            let ask_ticks=(ask/self.tick-1e-8).ceil();
            let bid=bid_ticks*self.tick+self.tick*1e-7;
            let ask=ask_ticks*self.tick-self.tick*1e-7;
            if bs>=self.lot && directional>=0.0 && self.danger_dir>=0.0 {o.bid(bid,bs);}
            if az>=self.lot && directional<=0.0 && self.danger_dir<=0.0 {o.ask(ask,az);}
            self.last_bid=bid_ticks*self.tick;
            self.last_ask=ask_ticks*self.tick;
        }
        if m.k%10==0 || active || jumped {eprintln!("STATE,{},{},{:.4},{:.3},{:.3},{:.2},{:.7},{:.2},{:.1},{:.4},{:.4},{}",m.k,m.slot,p,q,self.equity-self.initial_equity,dd,self.drift,score,directional,bb,ba,self.own_id);}
        self.last_active=active;
        self.last_q=q;self.last_price=p;self.last_slot=m.slot;
        Ok(())
    }
}
fn main()->BotResult {mtx_kit::run(Maker::default())}

#[cfg(test)]
mod tests {
 use super::*;
 #[test]
 fn exported_forest_matches_training_predictions() {
 let f=Forest::load();
 let x:Vec<f64>=vec![2.0, 655.0, 0.0030441400595009327, 0.0, 200.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 100.0, 100.0];
 assert!((f.predict(&x)-0.004582352635716356).abs()<1e-10);
 let x:Vec<f64>=vec![502.0, 155.0, 0.7640791535377502, 2.0, 104.0, 0.30478087067604065, 76.0, 2.7007143216906115e-05, 0.0074820686131715775, 3.649177961051464e-05, 0.5659164786338806, 1.0, 1.0, 2.0, 1.0, 2.0, 2.0, 3.0, 2.0, 7.0, 3.0, 10.0, 4.0, 11.0, 5.0, 11.0, 8.0, 4.0, 2.0];
 assert!((f.predict(&x)-0.13125456057130444).abs()<1e-10);
 let x:Vec<f64>=vec![434.0, 89.0, 0.8298279047012329, 1.0, 112.0, 0.5460829734802246, 69.0, 2.690769179025665e-05, 0.004212765488773584, 5.3357820434030145e-05, 0.844440758228302, 4.0, 1.0, 7.0, 2.0, 10.0, 4.0, 17.0, 4.0, 19.0, 5.0, 20.0, 7.0, 25.0, 12.0, 29.0, 15.0, 2.0, 4.0];
 assert!((f.predict(&x)-0.7012749462384088).abs()<1e-10);
 }
 #[test]
 fn drawdown_stop_flattens_both_sides_with_oracle_limits() {
   for q in [-20.0,20.0] {
     let mut maker=Maker{stopped:true,lot:0.001,tick:0.01,..Maker::default()};
     let market=Market{oracle:143.0,mark:200.0,cap:20.0,max_drawdown:250.0,position:Position{size:q,entry_usd:143.0},..Market::default()};
     let intents=mtx_kit::step_offline(&mut maker,&market).unwrap();
     let (actions,notes)=mtx_kit::plan(&intents,&market);
     assert!(notes.is_empty());
     let mut total=0.0;
     for action in actions {
       if let Action::Market{side,size,limit}=action {
         assert!(size<=8.0); total+=size;
         if q>0.0 {assert_eq!(side,Side::Ask);assert_eq!(limit,143.0*0.992);}
         else {assert_eq!(side,Side::Bid);assert_eq!(limit,143.0*1.008);}
       }
     }
     assert_eq!(total,20.0);
   }
 }

}
