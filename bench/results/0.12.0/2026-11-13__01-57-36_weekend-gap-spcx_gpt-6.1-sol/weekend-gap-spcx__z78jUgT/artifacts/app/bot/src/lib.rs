//! Weekend SPCX strategy. Only public on-chain observations enter the causal model.
mod engine;
use engine::{Engine,Config};
use mtx_kit::{log,prelude::*,rpc_account,venue::{PhoenixVenue,phoenix_rise::{accounts::trader::Trader,ix::PhoenixInstruction}}};
use serde_json::{Value,json};

#[derive(Default)]
struct Weekend {
    start:u64,reopen:u64,authority:String,address:String,trader:String,
    decimals:f64,tick_value:f64,asset_id:u64,seen:std::collections::HashSet<String>,
    accounts:Vec<String>,engine:Option<Engine>,previous_mid:f64,peak_equity:f64,include_start:bool,
}
impl Weekend {
    fn trades(&mut self,m:&Market,mid:f64) {
        let Ok(raw)=rpc("getSignaturesForAddress",&json!([self.address,{"commitment":"confirmed","limit":64}]).to_string()) else{return};
        let Ok(v)=serde_json::from_str::<Value>(&raw) else{return};
        let Some(arr)=v.as_array().or_else(||v["result"].as_array()) else{return};
        let mut fresh=Vec::new();
        for s in arr {
            let sig=s["signature"].as_str().unwrap_or("");
            let slot=s["slot"].as_u64().unwrap_or(0);
            if slot<self.start || (slot==self.start && !self.include_start) || sig.is_empty() {continue;}
            if self.seen.insert(sig.into()) {fresh.push((sig.to_string(),slot));}
        }
        for (sig,slot) in fresh.into_iter().rev() {
            let Ok(raw)=rpc("getTransaction",&json!([sig,{"encoding":"json","commitment":"confirmed","maxSupportedTransactionVersion":0}]).to_string()) else {continue};
            let Ok(mut v)=serde_json::from_str::<Value>(&raw) else{continue};
            if v.get("result").is_some(){v=v["result"].take();}
            if !v["meta"]["err"].is_null(){continue;}
            let msg=&v["transaction"]["message"];
            let Some(keys)=msg["accountKeys"].as_array() else{continue};
            let keystr=|i:usize|keys.get(i).and_then(|x|x.as_str().or_else(||x["pubkey"].as_str())).unwrap_or("").to_string();
            let signer=keystr(0);
            if signer==self.authority {continue;}
            let Some(ixs)=msg["instructions"].as_array() else{continue};
            for ix in ixs {
                let Some(data)=ix["data"].as_str().and_then(|s|bs58::decode(s).into_vec().ok()) else{continue};
                if data.len()<19 || data[..8]!=PhoenixInstruction::PlaceMarketOrder.discriminant() || data[8]!=2 || data[9]>1 {continue;}
                let offset=11+if data[10]==1 {8} else {0};
                if data.len()<offset+8 {continue;}
                let size=u64::from_le_bytes(data[offset..offset+8].try_into().unwrap()) as f64/self.decimals;
                let sign=if data[9]==0 {1.0} else {-1.0};
                if size<=0.0 || size>4.1 {continue;}
                let idx=if let Some(i)=self.accounts.iter().position(|a|*a==signer){i}else{
                    if self.accounts.len()>=8 {continue;}
                    self.accounts.push(signer.clone());self.accounts.len()-1
                };
                let aggression=if data[10]==1 && self.previous_mid>0.0 {
                    let limit=u64::from_le_bytes(data[11..19].try_into().unwrap()) as f64*self.tick_value;
                    Some(sign*(limit/self.previous_mid-1.0))
                }else{None};
                if let Some(e)=self.engine.as_mut(){e.trade(idx,slot.saturating_sub(self.start),size*sign,mid,aggression);}
                log(&format!("TRADE t={} id={} q={:.3} aggr={:.2} px={:.4}",slot.saturating_sub(self.start),idx,size*sign,aggression.unwrap_or(0.0)*10000.0,mid));
            }
        }
        let _=m;
    }
    fn drawdown(&mut self,m:&Market)->f64 {
        let Ok(Some(data))=rpc_account(&self.trader) else{return m.drawdown+100.0;};
        let Ok(t)=Trader::try_from_account_bytes(&data) else{return m.drawdown+100.0;};
        let mut equity=t.header.trader_state.quote_lot_collateral.as_inner() as f64/1e6;
        for (asset,p) in t.positions(){
            if asset==self.asset_id {
                equity+=p.virtual_quote_lot_position().as_inner() as f64/1e6;
                equity+=p.base_lot_position().as_inner() as f64/self.decimals*m.mark;
            }
        }
        if equity.is_finite(){self.peak_equity=self.peak_equity.max(equity);(self.peak_equity-equity).max(0.0)}else{m.drawdown+100.0}
    }
}
impl Strategy for Weekend {
    fn start(&mut self,s:&Session)->BotResult {
        self.start=s.start_slot;
        self.reopen=s.var("MTX_REOPEN_SLOT").and_then(|v|v.parse().ok()).unwrap_or(s.end_slot);
        self.authority=s.authority.clone();self.peak_equity=10000.0;
        let venue=PhoenixVenue::from_json(&s.venue)?;
        let market=venue.market(&s.symbol)?;
        self.decimals=10f64.powi(market.base_lots_decimals as i32);
        self.tick_value=market.tick_size as f64*10f64.powi(market.base_lots_decimals as i32-6);
        self.asset_id=market.asset_id as u64;
        self.trader=venue.trader_account(&mtx_kit::venue::key(&s.authority)?).to_string();
        self.address=mtx_kit::phoenix_market_address(&s.venue,&s.symbol).unwrap_or_default();
        self.engine=Some(Engine::new(s.start_mark,self.reopen.saturating_sub(self.start),Config::default()));
        // Seed the cursor before the first turn, so orders executed at the initial slot
        // are distinguishable from the snapshot's historical transactions.
        if let Ok(raw)=rpc("getSignaturesForAddress",&json!([self.address,{"commitment":"confirmed","limit":64}]).to_string()) {
            if let Ok(v)=serde_json::from_str::<Value>(&raw) {
                if let Some(arr)=v.as_array().or_else(||v["result"].as_array()) {
                    for x in arr {if let Some(sig)=x["signature"].as_str(){self.seen.insert(sig.to_string());}}
                    self.include_start=true;
                }
            }
        }
        log(&format!("START slot={} reopen={} p0={}",self.start,self.reopen,s.start_mark));
        Ok(())
    }
    fn step(&mut self,m:&Market,o:&mut Orders)->BotResult {
        let mid=match (m.best_bid,m.best_ask){(Some(b),Some(a))=>(a+b)*0.5,_=>m.mark};
        if !mid.is_finite()||mid<=0.0{return Ok(());}
        let t=m.slot.saturating_sub(self.start);
        if self.previous_mid==0.0 {
            if let (Some(b),Some(a),Some(e))=(m.best_bid,m.best_ask,self.engine.as_mut()){
                e.halfspread=((a-b)*0.5/mid-0.00012).clamp(0.00075,0.00165);
                log(&format!("BOOK bid={} ask={} hs={}",b,a,e.halfspread));
            }
        }
        if m.slot<self.reopen {self.trades(m,mid);}
        let dd=self.drawdown(m);
        let exit_risk=(m.position.size*(m.mark-mid)).max(0.0)+m.position.size.abs()*mid*0.002;
        let dd=dd+exit_risk;
        let Some(e)=self.engine.as_mut()else{return Ok(());};
        let target=e.update(t,mid,m.mark,m.position.size,dd).clamp(-m.cap,m.cap);
        if (target-m.position.size).abs()>=1.0 {
            o.target_position_with(target,Execution{slice:5.0,max_per_step:140.0,slippage_bps:1600.0,min_trade:1.0});
        }
        if t%12<2 || t+4>=e.reopen {
            log(&format!("STATE t={} mid={:.4} mark={:.4} pos={:.1} target={:.1} edge={:.5} open={:.5} temp={:.5} dd={:.2} impact={:.5} half={:.0} probs={:?}",t,mid,m.mark,m.position.size,target,e.expected_change,e.opening_edge,e.expected_temp,dd,e.expected_impact,e.expected_half,e.info_probs));
        }
        self.previous_mid=mid;
        Ok(())
    }
}
mtx_kit::export!(Weekend);
