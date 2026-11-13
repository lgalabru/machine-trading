mod model;
use mtx_kit::{prelude::*, venue::{PhoenixVenue, phoenix_rise::ix::PhoenixInstruction, phoenix_rise::accounts::owned::Trader}};
use serde_json::{Value, json};
use std::collections::VecDeque;

#[derive(Default)]
struct Maker {
    tick: f64, lot_scale:f64, authority:String, market_addr:String,
    trader_addr:String, equity_peak:f64, stopped:bool,
    last_slot:u64, last_oracle:f64, last_pos:f64,
    flow:VecDeque<(u64,f64)>, count:f64, elapsed:f64,
    drift:f64, variance:f64, moves:u64,
    bid:f64, ask:f64, half:f64,
    event_dir:f64,event_until:u64,cooldown:u64,last_jump:u64,seen_jumps:u64,
}
impl Maker {
    fn read_flow(&mut self,m:&Market) {
        let first = self.last_slot.max(m.slot.saturating_sub(4));
        for slot in first..m.slot {
            let Ok(s)=mtx_kit::rpc("getBlock", &json!([slot,{"encoding":"json","transactionDetails":"full","rewards":false,"maxSupportedTransactionVersion":0,"commitment":"processed"}]).to_string()) else {continue};
            let Ok(v)=serde_json::from_str::<Value>(&s) else {continue};
            let Some(txs)=v["transactions"].as_array() else {continue};
            for tx in txs {
                let msg=&tx["transaction"]["message"];
                let Some(keys)=msg["accountKeys"].as_array() else {continue};
                if keys.first().and_then(Value::as_str)==Some(self.authority.as_str()) {continue;}
                if !keys.iter().any(|x|x.as_str()==Some(self.market_addr.as_str())) {continue;}
                let Some(ixs)=msg["instructions"].as_array() else {continue};
                for ix in ixs {
                    let Some(data)=ix["data"].as_str() else {continue};
                    if data.len()>300 {continue;}
                    let Ok(d)=bs58::decode(data).into_vec() else {continue};
                    if d.len()<19 || d[..8]!=PhoenixInstruction::PlaceMarketOrder.discriminant() || d[8]!=2 {continue;}
                    let sign=if d[9]==0 {1.0}else{-1.0};
                    let offset=11+if d[10]==1 {8}else{0};
                    if d.len()<offset+8 {continue;}
                    let size=u64::from_le_bytes(d[offset..offset+8].try_into().unwrap()) as f64/self.lot_scale;
                    self.flow.push_back((m.slot,sign)); self.count+=1.0;
                    mtx_kit::log(&format!("FLOW {} {} {:.3}",m.k,sign,size));
                }
            }
        }
        self.last_slot=m.slot;
        while self.flow.front().is_some_and(|x|m.slot-x.0>=100) { self.flow.pop_front(); }
    }

}
impl Strategy for Maker {
    fn start(&mut self,s:&Session)->BotResult {
        let v=PhoenixVenue::from_json(&s.venue)?;let cfg=v.market(&s.symbol)?;
        self.tick=v.ticks_to_usd(cfg,1); self.lot_scale=10f64.powi(cfg.base_lots_decimals as i32);
        self.market_addr=cfg.market_pubkey.clone();self.authority=s.authority.clone();
        self.trader_addr=v.trader_account(&mtx_kit::venue::key(&s.authority)?).to_string();
        self.equity_peak=10000.0;
        self.last_jump=s.start_slot.saturating_sub(1000);self.last_slot=s.start_slot;self.last_oracle=s.start_oracle;self.half=0.001;
        Ok(())
    }
    fn step(&mut self,m:&Market,o:&mut Orders)->BotResult {
        let p=m.oracle;
        if !p.is_finite()||p<=0.0 {o.cancel_all();return Ok(());}
        self.elapsed+=2.0;self.read_flow(m);
        let mut equity=10000.0+m.pnl+m.position.size*(p-m.mark);
        if let Ok(Some(data))=mtx_kit::rpc_account(&self.trader_addr) {
            if let Ok(t)=Trader::try_from_account_bytes(&data) {
                equity=t.state.quote_lot_collateral.as_inner() as f64/1e6;
                for (_,pos) in t.positions.entries {equity+=pos.base_lot_position.as_inner() as f64/self.lot_scale*p+pos.virtual_quote_lot_position.as_inner() as f64/1e6;}
            }
        }
        self.equity_peak=self.equity_peak.max(equity);
        let dd=self.equity_peak-equity;
        if dd>110.0 {self.stopped=true;}
        if self.stopped {o.cancel_all();o.target_position(0.0);return Ok(());}
        let change=p/self.last_oracle-1.0;
        let jumped=change.abs()>0.006;
        if change.abs()>0.006 {
            self.event_dir=0.0;self.event_until=0;self.cooldown=m.slot+30;self.flow.clear();self.last_jump=m.slot;self.seen_jumps+=1;
        } else if change.abs()>1e-8 {
            self.moves+=1;
            self.drift=0.97*self.drift+0.03*change;
            self.variance=0.97*self.variance+0.03*change*change;
        }
        let windows=[4,8,12,16,24,32,48];
        let mut probs=[0.0;2];
        for (i,side) in [1.0,-1.0].iter().enumerate() {
            let mut x=[0.0;20];
            for (j,&w) in windows.iter().enumerate() {
                for &(st,sgn) in &self.flow {if m.slot-st<w {if sgn==*side {x[j]+=1.0;}else{x[j+7]+=1.0;}}}
            }
            x[14]=(self.count/self.elapsed.max(50.0)/2.0).clamp(0.09,0.25);
            x[15]=self.elapsed.min(120.0);x[16]=(m.slot-self.last_jump).min(120) as f64;
            x[17]=m.slots_left.min(120) as f64;x[18]=self.drift*side*10000.0;x[19]=self.seen_jumps as f64;
            probs[i]=model::predict(&x);
        }
        let (score,dir)=if probs[0]>probs[1] {(probs[0],1.0)}else{(probs[1],-1.0)};
        if m.slot>self.cooldown && m.slots_left>20 && score>0.3 {
            self.event_dir=dir;self.event_until=m.slot+48;
        }
        if m.slot>self.event_until {self.event_dir=0.0;}
        let trend=(self.drift*40000.0).clamp(-5.0,5.0);
        let target=if self.event_dir!=0.0 {self.event_dir*18.0}else{trend};
        o.cancel_all();
        if (self.event_dir!=0.0 || jumped || m.position.size.abs()>8.0 && m.slot>self.event_until) && (target-m.position.size).abs()>0.5 {
            if target>m.position.size {o.buy(target-m.position.size,p*1.005);}else{o.sell(m.position.size-target,p*0.995);}
        }
        let mut b=m.best_bid.unwrap_or(p*(1.0-self.half));
        let mut a=m.best_ask.unwrap_or(p*(1.0+self.half));
        if m.k==0 {self.half=((a-b)/p/2.0).clamp(0.0003,0.003);}
        if (b-self.bid).abs()<self.tick*0.5 {b=p*(1.0-self.half);}
        if (a-self.ask).abs()<self.tick*0.5 {a=p*(1.0+self.half);}
        let center=p-(m.position.size-target)*p*0.00025;
        let bid=(b+self.tick).min(center-p*0.0003).min(p*1.002);
        let ask=(a-self.tick).max(center+p*0.0003).max(p*0.998);
        self.bid=(bid/self.tick).floor()*self.tick;self.ask=(ask/self.tick).ceil()*self.tick;
        let effective=if self.event_dir!=0.0 || jumped || m.position.size.abs()>8.0 {target}else{m.position.size};
        let cap=if self.event_dir!=0.0 {18.0}else{7.0};
        let bs=(cap-effective).clamp(0.0,2.0);let az=(cap+effective).clamp(0.0,2.0);
        if bs>0.01 {o.bid(self.bid,bs);}
        if az>0.01 {o.ask(self.ask,az);}
        if m.k%10==0||self.event_dir!=0.0 {mtx_kit::log(&format!("STATE k={} p={:.3} pos={:.3} pnl={:.2} score={:.2} event={} target={:.2} flow={} equity={:.2} dd={:.2}",m.k,p,m.position.size,m.pnl,score,self.event_dir,target,self.count,equity,dd));}
        self.last_pos=m.position.size;self.last_oracle=p;
        Ok(())
    }
}
mtx_kit::export!(Maker);
