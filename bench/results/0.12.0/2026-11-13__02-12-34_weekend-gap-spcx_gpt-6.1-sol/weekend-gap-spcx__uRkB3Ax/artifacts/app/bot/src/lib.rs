use mtx_kit::{log, prelude::*, venue::PhoenixVenue};
use serde_json::{Value, json};
use std::collections::HashSet;
mod model;
use model::Model;

#[derive(Default, Clone)]
struct Flow { key: String, net: f64, gross: f64, n: usize, last: u64 }
#[derive(Default)]
struct Weekend {
    reopen: u64, start: u64, p0: f64, address: String, authority: String,
    last_sig: String, seen: HashSet<String>, flows: Vec<Flow>,
    last_mid: f64, ema: f64, model:Model, step_flows:Vec<(usize,f64)>, trader:String, peak:f64, stopped:bool, last_trade:u64,
}
impl Weekend {
    fn read_flow(&mut self, slot: u64) {
        let mut opts=json!({"commitment":"confirmed","limit":40});
        if !self.last_sig.is_empty() { opts["until"]=json!(self.last_sig); }
        let Ok(raw)=rpc("getSignaturesForAddress", &json!([self.address,opts]).to_string()) else {return};
        let Ok(sigs)=serde_json::from_str::<Value>(&raw) else {return};
        let Some(sigs)=sigs.as_array() else {return};
        if let Some(s)=sigs.first().and_then(|s|s["signature"].as_str()) { self.last_sig=s.to_owned(); }
        for s in sigs.iter().rev() {
            let Some(sig)=s["signature"].as_str() else {continue};
            if !s["err"].is_null() || !self.seen.insert(sig.to_owned()) {continue}
            let Ok(raw)=rpc("getTransaction",&json!([sig,{"encoding":"json","commitment":"confirmed","maxSupportedTransactionVersion":0}]).to_string()) else {continue};
            let Ok(tx)=serde_json::from_str::<Value>(&raw) else {continue};
            let keys=&tx["transaction"]["message"]["accountKeys"];
            let Some(signer)=keys[0].as_str() else {continue};
            if signer==self.authority {continue}
            let Some(ixs)=tx["transaction"]["message"]["instructions"].as_array() else {continue};
            for ix in ixs {
                let Some(data)=ix["data"].as_str().and_then(|s|bs58::decode(s).into_vec().ok()) else {continue};
                use mtx_kit::venue::phoenix_rise::ix::PhoenixInstruction;
                if data.len()<20 || data[..8]!=PhoenixInstruction::PlaceMarketOrder.discriminant() || data[8]!=2 {continue}
                let side=if data[9]==0 {1.0} else {-1.0};
                let pos=11+if data[10]==1 {8} else {0};
                let Some(bytes)=data.get(pos..pos+8) else {continue};
                let size=u64::from_le_bytes(bytes.try_into().unwrap()) as f64/1000.0;
                let price=if data[10]==1 {u64::from_le_bytes(data[11..19].try_into().unwrap()) as f64*0.01} else {0.0};
                let idx=self.flows.iter().position(|f|f.key==signer).unwrap_or_else(||{self.flows.push(Flow{key:signer.to_owned(),..Flow::default()});self.flows.len()-1});
                let f=&mut self.flows[idx]; f.net+=side*size;f.gross+=size;f.n+=1;f.last=slot;
                if idx<8 {self.step_flows.push((idx,side*size)); self.model.flow(slot-self.start,idx,side*size);}
                log(&format!("FLOW {} {} {} {} {:.3} {:.2}",slot-self.start,idx,signer,side,size,price));
            }
        }
    }
}
impl Strategy for Weekend {
    fn start(&mut self,s:&Session)->BotResult {
        self.reopen=s.var("MTX_REOPEN_SLOT").and_then(|x|x.parse().ok()).unwrap_or(s.end_slot);
        self.start=s.start_slot;self.p0=s.start_mark;self.authority=s.authority.clone();
        let v=PhoenixVenue::from_json(&s.venue)?;
        self.address=v.market("SPCX")?.market_pubkey.clone();
        self.trader=v.trader_account(&mtx_kit::venue::key(&s.authority)?).to_string();
        log(&format!("START {} {} {}",self.start,self.reopen,self.p0));
        // Discard the snapshot's old transactions.
        if let Ok(raw)=rpc("getSignaturesForAddress",&json!([self.address,{"commitment":"confirmed","limit":1}]).to_string()) {
            if let Ok(x)=serde_json::from_str::<Value>(&raw) {self.last_sig=x[0]["signature"].as_str().unwrap_or("").to_owned();}
        }
        Ok(())
    }
    fn step(&mut self,m:&Market,o:&mut Orders)->BotResult {
        self.step_flows.clear();
        if m.slot<self.reopen {self.read_flow(m.slot);}
        let mid=match (m.best_bid,m.best_ask){(Some(b),Some(a))=>(a+b)*0.5,_=>m.mark};
        let t=m.slot-self.start; let left=self.reopen.saturating_sub(m.slot);
        let price=mid/self.p0-1.;
        let mut equity=m.pnl;
        if let Ok(Some(bytes))=mtx_kit::rpc_account(&self.trader) {
            if let Ok(tr)=mtx_kit::venue::phoenix_rise::accounts::trader::Trader::try_from_account_bytes(&bytes) {
                equity=tr.header().state().quote_lot_collateral.as_inner() as f64/1e6-10000.+m.position.size*(m.mark-m.position.entry_usd);
            }
        }
        self.peak=self.peak.max(equity);let dd=(self.peak-equity).max(0.);
        if dd>550. {self.stopped=true;}
        let mut target=m.position.size;
        let mut signal=0.;let mut terminal=0.;let mut pred=0.;
        if left>0&&!self.stopped {
            self.model.update(t,price,&self.step_flows,t.saturating_sub(self.last_trade)<=4||m.best_bid.zip(m.best_ask).is_some_and(|(b,a)|b>=a));
            (signal,terminal,pred)=self.model.signal(t,left,price);
            let desired=70.*(signal/0.001).clamp(-1.,1.);
            if (desired-target).abs()>10. && (signal-target/70.*0.001).abs()>0.0025 {target=desired;}
            if left<20 {
                let budget=(850.-dd+m.position.size*(mid-m.mark)).max(0.);
                let long=budget/(mid-self.p0*0.845).max(self.p0*0.04);
                let short=budget/(self.p0*1.155-mid).max(self.p0*0.04);
                target=target.clamp(-short.min(70.),long.min(70.));
            }
        } else if self.stopped {target=0.;}
        // Keeping an existing position after the open avoids paying another spread.
        target=target.clamp(-69.9,69.9);
        let diff=target-m.position.size;
        if diff.abs()>3.0 {
            let mut amount=diff.abs();while amount>0.01 {let q=amount.min(5.);if diff>0. {o.buy(q,mid*1.006);}else{o.sell(q,mid*0.994);}amount-=q;}
            self.last_trade=t;
        }
        if t%10==0||!self.step_flows.is_empty() {
            log(&format!("M {} {:.3} {:.3} {:.2} {:.2} {:.2} {:.2} SIG {:.4} {:.4} {:.4} PR {:?}",t,mid,m.mark,m.position.size,equity,dd,target,signal,terminal,pred,self.model.probs));
        }
        self.last_mid=mid;
        Ok(())
    }
}
mtx_kit::export!(Weekend);
