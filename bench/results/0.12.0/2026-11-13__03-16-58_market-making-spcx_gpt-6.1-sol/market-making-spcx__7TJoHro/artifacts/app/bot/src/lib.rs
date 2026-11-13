mod model;
mod trend;
use mtx_kit::{log, prelude::*, venue::{PhoenixVenue, phoenix_rise::{ix::PhoenixInstruction, accounts::trader::Trader}}};
use serde_json::{json,Value};
use std::collections::VecDeque;

#[derive(Default)]
struct Maker {
    tick: f64,
    lot_scale: f64,
    authority: String,
    market: String,
    last_position: f64,
    last_oracle: f64,
    drift: f64,
    history: VecDeque<(u64,f64)>,
    signal: f64,
    signal_until: u64,
    last_jump: u64,
    total_count: f64,
    trader: String,
    asset_id: u32,
    peak_equity: f64,
    stopped: bool,
    risk_cap: f64,
    risk_move: f64,
    dd_limit: f64,
    total_buys: f64,
    total_sells: f64,
    jump_count: u32,
    price_history: VecDeque<f64>,
    return_sum: f64,
    return_squared: f64,
    return_count: f64,
    competitor_bps: f64,
}

impl Maker {
    fn execute(o: &mut Orders, delta: f64, p: f64) {
        let mut left=delta.abs();
        for _ in 0..8 {
            let size=left.min(8.0);
            if size<0.0005 {break;}
            if delta>0.0 {o.buy(size,p*1.004);}else{o.sell(size,p*0.996);}
            left-=size;
        }
    }

    fn flow(&mut self,m:&Market) -> (usize,usize,f64) {
        let mut buys=0;let mut sells=0;let mut volume=0.0;
        let answer=mtx_kit::rpc("getBlock",&json!([m.slot.saturating_sub(2),{"encoding":"json","transactionDetails":"full","rewards":false,"maxSupportedTransactionVersion":0,"commitment":"processed"}]).to_string());
        if let Ok(s)=answer {
            if let Ok(v)=serde_json::from_str::<Value>(&s) {
                for tx in v["transactions"].as_array().into_iter().flatten() {
                    let msg=&tx["transaction"]["message"];
                    let Some(keys)=msg["accountKeys"].as_array() else {continue};
                    if keys.first().and_then(|v|v.as_str())==Some(&self.authority) {continue;}
                    if !keys.iter().any(|v|v.as_str()==Some(&self.market)) {continue;}
                    for ix in msg["instructions"].as_array().into_iter().flatten() {
                        let Some(encoded)=ix["data"].as_str() else {continue};
                        let Ok(data)=bs58::decode(encoded).into_vec() else {continue};
                        if data.len()<19 || data[..8]!=PhoenixInstruction::PlaceMarketOrder.discriminant() || data[8]!=2 {continue;}
                        let offset=11+if data[10]==1 {8} else {0};
                        if data.len()<offset+8 {continue;}
                        let lots=u64::from_le_bytes(data[offset..offset+8].try_into().unwrap());
                        let sign=if data[9]==0 {1.0} else {-1.0};
                        let size=sign*lots as f64/self.lot_scale;
                        if sign>0.0 {buys+=1;}else{sells+=1;}
                        volume+=size;
                        self.history.push_back((m.k,size));
                    }
                }
            }
        }
        while self.history.front().map_or(false,|x|m.k.saturating_sub(x.0)>20) {self.history.pop_front();}
        self.total_count+=(buys+sells) as f64;
        self.total_buys+=buys as f64;self.total_sells+=sells as f64;
        (buys,sells,volume)
    }
}

impl Strategy for Maker {
    fn start(&mut self, session: &Session) -> BotResult {
        let venue = PhoenixVenue::from_json(&session.venue)?;
        let market=venue.market(&session.symbol)?;
        self.tick = venue.ticks_to_usd(market, 1);
        self.market=market.market_pubkey.clone();
        self.lot_scale=10f64.powi(market.base_lots_decimals as i32);
        self.authority=session.authority.clone();
        self.last_oracle = session.start_oracle;
        self.trader=venue.trader_account(&mtx_kit::venue::key(&session.authority)?).to_string();
        self.asset_id=market.asset_id;
        self.peak_equity=10000.0;
        self.risk_cap=session.cap.min(19.9);
        self.risk_move=session.start_mark.max(session.start_oracle)*0.045;
        self.dd_limit=session.max_drawdown;
        Ok(())
    }
    fn step(&mut self, m: &Market, o: &mut Orders) -> BotResult {
        let p = m.oracle;
        if !p.is_finite() || p <= 0.0 { o.cancel_all(); return Ok(()); }
        let q = m.position.size;
        let fill = q - self.last_position;
        let (buys,sells,volume)=self.flow(m);
        let ret=p/self.last_oracle-1.0;
        let jump=ret.abs()>0.008;
        if jump {self.jump_count+=1;self.last_jump=m.k;self.signal=0.0;self.signal_until=0;self.history.clear();}
        self.drift=0.98*self.drift+0.02*ret.clamp(-0.001,0.001);
        if !jump && ret!=0.0 {self.return_sum+=ret;self.return_squared+=ret*ret;self.return_count+=1.0;}
        self.price_history.push_back(p);
        while self.price_history.len()>41 {self.price_history.pop_front();}
        if m.k==0 {
            self.competitor_bps=match (m.best_bid,m.best_ask) { (Some(b),Some(a)) => ((a-b)/p*5000.0).clamp(8.0,30.0), _=>18.0 };
        }

        let base=((self.total_count+12.0)/(2.0*(m.k as f64+20.0))).clamp(0.15,0.6);
        let mut probs=[0.0;2];
        let mut trend_probs=[0.0;2];
        let mut raw_lr=[0.0f64;2];
        for side in 0..2 {
            let mut x=[0.0f64;33];let mut llrs=[0.0f64;3];
            for (idx,window) in [1u64,2,3,4,6,9,12,15,20].iter().enumerate() {
                let mut b=0.0;let mut a=0.0;
                for &(k,s) in &self.history {if m.k.saturating_sub(k)<*window {if s>0.0 {b+=1.0;}else{a+=1.0;}}}
                let (own,opp)=if side==0 {(b,a)}else{(a,b)};
                x[idx*2]=own;x[idx*2+1]=opp;
                if *window>=2 && m.k+1>=*window && (self.last_jump==0 || m.k-self.last_jump>=*window) {
                    for (i,extra) in [0.5f64,1.0,2.0].iter().enumerate() {
                        llrs[i]=llrs[i].max(own*(1.0+extra/base).ln()-extra*(*window as f64));
                        if i==1 && *window<=15 {raw_lr[side]=raw_lr[side].max(own*(1.0+extra/base).ln()-extra*(*window as f64));}
                    }
                }
            }
            let (own,opp,sign)=if side==0 {(self.total_buys,self.total_sells,1.0)}else{(self.total_sells,self.total_buys,-1.0)};
            x[18]=(own+6.0)/(m.k as f64+20.0);x[19]=(opp+6.0)/(m.k as f64+20.0);x[20]=base;
            x[21]=llrs[0];x[22]=llrs[1];x[23]=llrs[2];
            x[24]=if self.last_jump==0 {60.0}else{(m.k-self.last_jump).min(60) as f64};
            x[25]=self.jump_count as f64;x[26]=m.k as f64;x[27]=m.steps_left.min(150) as f64;
            for (idx,w) in [6usize,15,40].iter().enumerate() {
                let old=self.price_history[self.price_history.len().saturating_sub(w+1)];
                x[28+idx]=sign*(p/old-1.0)*10000.0;
            }
            x[31]=sign*self.return_sum/self.return_count.max(1.0)*10000.0;
            x[32]=(self.return_squared/self.return_count.max(1.0)).sqrt()*10000.0;
            probs[side]=model::predict(&x);
            if self.jump_count==0 {trend_probs[side]=trend::predict(&x);}
        }
        let threshold=0.4*(self.competitor_bps/18.0).sqrt();
        let strength=[(probs[0]/threshold).max(raw_lr[0]/4.5),(probs[1]/threshold).max(raw_lr[1]/4.5)];
        let direction=if strength[0]>strength[1] {1.0}else{-1.0};
        let maxscore=strength[0].max(strength[1]);
        if maxscore>=1.0 && m.k>14 && m.steps_left>30 && self.jump_count<3 && (self.last_jump==0 || m.k-self.last_jump>29) {
            self.signal=direction;
            self.signal_until=m.k+14;
        }
        if m.k>self.signal_until {self.signal=0.0;}
        let mut equity=None;
        if let Ok(Some(data))=mtx_kit::rpc_account(&self.trader) {
            if let Ok(t)=Trader::try_from_account_bytes(&data) {
                let mut value=t.header().trader_state.quote_lot_collateral.as_inner() as f64/1e6;
                for (asset,pos) in t.positions() {
                    if asset as u32==self.asset_id {
                        let pos=pos.to_position();
                        value+=pos.virtual_quote_lot_position.as_inner() as f64/1e6;
                        value+=pos.base_lot_position.as_inner() as f64/self.lot_scale*p;
                    }
                }
                equity=Some(value);
                self.peak_equity=self.peak_equity.max(value);
                let dd=(self.peak_equity-value).max(0.0);
                let remaining=(self.dd_limit-dd-45.0).max(0.0);
                self.risk_cap=((remaining/self.risk_move).min(m.cap.min(19.9))*1000.0).floor()/1000.0;
                if dd>self.dd_limit-50.0 {self.stopped=true;}
            }
        }
        if self.stopped {self.signal=0.0;}
        let weak_lean=if m.k>14 && m.steps_left>30 && self.jump_count<3 && (self.last_jump==0 || m.k-self.last_jump>29) {((probs[0]-probs[1])*40.0).clamp(-4.0,4.0)}else{0.0};
        let target=if self.signal!=0.0 {self.signal*self.risk_cap} else {((trend_probs[0]-trend_probs[1])*10.0+weak_lean).clamp(-8.0,8.0).clamp(-self.risk_cap,self.risk_cap)};
        o.cancel_all();
        let mut traded=false;
        if self.stopped || jump || (self.signal==0.0 && q.abs()>11.5) {
            Self::execute(o,-q,p);
            traded=true;
        } else if self.signal!=0.0 && (q-target).abs()>0.05 {
            Self::execute(o,target-q,p);
            traded=true;
        } else if q.abs()>self.risk_cap+0.001 {
            Self::execute(o,q.signum()*self.risk_cap-q,p);
            traded=true;
        }
        let skew=((q-target)*0.00015).clamp(-0.002,0.002);
        let half=p*0.00030;
        let bid0=m.best_bid.unwrap_or(p*0.999)+self.tick;
        let ask0=m.best_ask.unwrap_or(p*1.001)-self.tick;
        let bid=(bid0.min(p-half)-p*skew).min(p-self.tick);
        let ask=(ask0.max(p+half)-p*skew).max(p+self.tick);
        let bs=2.0f64.min((target+3.0-q).max(0.0)).min((self.risk_cap-q).max(0.0));
        let az=2.0f64.min((3.0+q-target).max(0.0)).min((self.risk_cap+q).max(0.0));
        if !traded && self.signal==0.0 && !self.stopped {
            if bs>=0.001 {o.bid((bid/self.tick).floor()*self.tick,bs);}
            if az>=0.001 {o.ask((ask/self.tick).ceil()*self.tick,az);}
        }
        if buys+sells>0 || fill.abs()>0.01 || jump {log(&format!("k={} p={:.2} q={:.3} pnl={:.2} b={} s={} vol={:.3} score={:.2} sig={} target={:.2} jump={} equity={:?} trend={:.3}/{:.3} riskcap={:.3}",m.k,p,q,m.pnl,buys,sells,volume,maxscore,self.signal,target,jump,equity,trend_probs[0],trend_probs[1],self.risk_cap));}
        self.last_position=q;self.last_oracle=p;
        Ok(())
    }
}
mtx_kit::export!(Maker);
