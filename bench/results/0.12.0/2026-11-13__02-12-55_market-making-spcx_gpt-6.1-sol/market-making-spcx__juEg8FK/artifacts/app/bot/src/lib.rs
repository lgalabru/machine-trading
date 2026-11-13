//! Oracle-centered SPCX maker with inventory control and public-chain flow signals.
//! The component only reads RPC state and trades through the SDK order sheet.

use base64::Engine;
use mtx_kit::{log, prelude::*};
use mtx_kit::venue::{
    PhoenixVenue,
    phoenix_rise::{
        accounts::{
            trader::{Trader, TraderHeader},
            orderbook::OrderbookHeader,
            discriminants::PhoenixAccount,
        },
        ix::PHOENIX_PROGRAM_ID,
    },
};

#[derive(Default)]
struct Participant {
    address: String,
    key: [u8; 32],
    position: f64,
    sequence: u64,
    maker: bool,
}

#[derive(Default)]
struct Maker {
    tick: f64,
    lots: f64,
    asset: u64,
    book: String,
    own: String,
    participants: Vec<Participant>,
    seq: u64,
    start_slot: u64,
    prev_oracle: f64,
    prev_pos: f64,
    prev_bid: f64,
    prev_ask: f64,
    drift: f64,
    flow: f64,
    half: f64,
    tape: Vec<(u64, f64, f64)>,
    signal: f64,
    until: u64,
    last_jump: u64,
    peak: f64,
    stopped: bool,
    jumps: u32,
    start_price: f64,
    n: f64,
    sum: f64,
    sum2: f64,
    jumped: bool,
    trend: f64,
}

fn decode(v:&serde_json::Value)->Option<Vec<u8>> {
    base64::engine::general_purpose::STANDARD.decode(v["data"][0].as_str()?).ok()
}
fn position(data:&[u8],asset:u64,lots:f64)->Option<(f64,f64,f64,u64)> {
    let t=Trader::try_from_account_bytes(data).ok()?;
    let mut pos=0.;
    let mut quote=0.;
    for (id,p) in t.positions(){
        if id==asset {
            pos=p.base_lot_position().as_inner() as f64/lots;
            quote=p.virtual_quote_lot_position().as_inner() as f64/1e6;
        }
    }
    Some((pos,quote,t.header().trader_state.quote_lot_collateral.as_inner() as f64/1e6,t.header().sequence_number.sequence_number))
}
impl Maker {
    // FIFO trade events identify makers. Trader position changes also cover
    // taker fills against spline liquidity, which FIFO events alone miss.
    fn read(&mut self,m:&Market)->(f64,f64,f64) {
        let args=serde_json::json!([self.book,{
            "encoding":"base64","dataSlice":{
                "offset":0,"length":5504
            }
        }]);
        if let Ok(raw)=mtx_kit::rpc("getAccountInfo",&args.to_string()) {
            if let Ok(j)=serde_json::from_str::<serde_json::Value>(&raw) {
                if let Some(data)=decode(&j["value"]) {
                    if data.len()==5504 {
                        let h=bytemuck::pod_read_unaligned::<OrderbookHeader>(&data);
                        for t in h.trade_list.iter(){
                            if t.trade_sequence_number>=self.seq && t.slot>=self.start_slot {
                                for who in &mut self.participants {
                                    if t.maker==who.key {
                                        who.maker=true;
                                    }
                                }
                            }
                        }
                        self.seq=h.trade_sequence_number.sequence_number;
                    }
                }
            }
        }
        let mut keys=vec![self.own.as_str()];
        keys.extend(self.participants.iter().map(|p|p.address.as_str()));
        let mut buys=0.;
        let mut sells=0.;
        let mut value=10000.;
        if let Ok(accounts)=mtx_kit::rpc_accounts(&keys){
            for (i,raw) in accounts.iter().enumerate(){
                if let Some(raw)=raw {
                    if let Some((p,q,c,seq))=position(raw,self.asset,self.lots) {
                        if i==0 {
                            value=c+q+p*m.oracle;
                            // Maker fills can await settlement into the trader account.
                            let un=m.position.size-p;
                            if un>0. {
                                value+=un*(m.oracle-self.prev_bid)-un*self.prev_bid*0.00005;
                            }
                            if un<0. {
                                value+=(-un)*(self.prev_ask-m.oracle)-(-un)*self.prev_ask*0.00005;
                            }
                        }
                        else {
                            let who=&mut self.participants[i-1];
                            let delta=p-who.position;
                            if !who.maker && delta.abs()>0.0001 {
                                let count=(seq.saturating_sub(who.sequence) as f64).clamp(1.0,4.0);
                                if delta>0.0 {
                                    buys+=count;
                                }
                                else{
                                    sells+=count;
                                }
                                log(&format!("F {} {} {:.3} {:.1}",m.k,i,delta,count));
                            }
                            who.position=p;
                            who.sequence=seq;
                        }
                    }
                }
            }
        }
        if buys+sells>0.0{
            self.tape.push((m.slot,buys,sells));
        }
        (buys,sells,value-10000.)
    }
}
impl Strategy for Maker {
    fn start(&mut self,s:&Session)->BotResult {
        let v=PhoenixVenue::from_json(&s.venue)?;
        let c=v.market(&s.symbol)?;
        self.tick=v.ticks_to_usd(c,1);
        self.lots=10f64.powi(c.base_lots_decimals as i32);
        self.asset=c.asset_id as u64;
        self.book=c.market_pubkey.clone();
        self.own=v.trader_account(&s.authority.parse()?).to_string();
        self.start_price=s.start_oracle;
        self.start_slot=s.start_slot;
        self.prev_oracle=s.start_oracle;
        self.half=12.;
        // Discover recently activated participants from public account headers.
        // Accounts subsequently observed making markets are removed from flow.
        let args=serde_json::json!([PHOENIX_PROGRAM_ID.to_string(),{
            "encoding":"base64","dataSlice":{
                "offset":0,"length":224
            },"filters":[{
                "memcmp":{
                    "offset":0,"bytes":bs58::encode(PhoenixAccount::Trader.discriminant()).into_string()
                }
            }]
        }]);
        if let Ok(raw)=mtx_kit::rpc("getProgramAccounts",&args.to_string()){
            if let Ok(j)=serde_json::from_str::<serde_json::Value>(&raw){
                if let Some(a)=j.as_array(){
                    for r in a {
                        if let (Some(address),Some(data))=(r["pubkey"].as_str(),decode(&r["account"])) {
                            if data.len()==224 && address!=self.own {
                                let h=bytemuck::pod_read_unaligned::<TraderHeader>(&data);
                                if h.last_deposit_slot>s.start_slot.saturating_sub(200) && h.trader_state.global_position_sequence_number==0 {
                                    self.participants.push(Participant{
                                        address:address.to_string(),key:h.key,sequence:h.sequence_number.sequence_number,..Default::default()
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }
        log(&format!("start {} tick {} participants {}",s.start_oracle,self.tick,self.participants.len()));
        Ok(())
    }
    fn step(&mut self,m:&Market,o:&mut Orders)->BotResult {
        let p=m.oracle;
        let pos=m.position.size;
        if !p.is_finite() || p<=0. {
            o.cancel_all();
            return Ok(());
        }
        let (buys,sells,equity)=self.read(m);
        self.peak=self.peak.max(equity);
        let dd=self.peak-equity;
        if dd>200.0 {
            self.stopped=true;
        }
        let ret=(p/self.prev_oracle-1.)*10000.;
        let fill=pos-self.prev_pos;
        self.flow=self.flow*0.86-fill;
        if ret.abs()<30. && ret.abs()>0.001 {
            self.drift=self.drift*0.94+ret*0.06;
        }
        let r=(p-self.prev_oracle)/self.start_price*10000.;
        if r.abs()>0.001 && r.abs()<40. {
            self.n+=1.;
            self.sum+=r;
            self.sum2+=r*r;
        }
        if ret.abs()>80. {
            self.jumps+=1;
            self.jumped=true;
            self.flow=0.;
            self.signal=0.;
            self.until=0;
            self.last_jump=m.slot;
            self.tape.clear();
        }
        // A short count window separates concentrated bursts from ordinary flow.
        self.tape.retain(|t|m.slot.saturating_sub(t.0)<12);
        let up=self.tape.iter().map(|t|t.1).sum::<f64>();
        let dn=self.tape.iter().map(|t|t.2).sum::<f64>();
        let su=up-0.3*dn-1.92;
        let sd=dn-0.3*up-1.92;
        if m.slot>self.last_jump+36 && m.slot>self.start_slot+34 && su.max(sd)>5.0 && m.slots_left>60 && self.jumps<3 {
            self.signal=if su>sd {
                1.
            }
            else{
                -1.
            };
            self.until=m.slot+30;
        }
        if m.slot>self.until || m.slots_left<=60 {
            self.signal=0.;
        }
        if self.signal>0. && sd>3.0 {
            self.signal=0.;
        }
        if self.signal<0. && su>3.0 {
            self.signal=0.;
        }
        // Integrate a small Gaussian mixture over unknown walk volatility and
        // persistent drift. A realized jump rules out the trending regime.
        let mut plus=0.;
        let mut minus=0.;
        let mut total=0.;
        let mut mean=0.;
        if !self.jumped && self.n>=12. {
            let mut terms=Vec::new();
            let mut maxlog:f64=-1e30;
            for sigma in [2.0_f64,3.,4.,5.,6.,7.,8.] {
                for mu in [0.,0.6,1.2,1.8,-0.6,-1.2,-1.8] {
                    let prior:f64=if mu==0. {
                        0.75
                    }
                    else {
                        0.25/6.
                    };
                    let ll=prior.ln()-self.n*sigma.ln()-(self.sum2-2.*mu*self.sum+self.n*mu*mu)/(2.*sigma*sigma);
                    maxlog=maxlog.max(ll);
                    terms.push((mu,ll));
                }
            }
            for (mu,ll) in terms {
                let w=(ll-maxlog).exp();
                total+=w;
                mean+=mu*w;
                if mu>0. {
                    plus+=w;
                }
                if mu<0. {
                    minus+=w;
                }
            }
            plus/=total;
            minus/=total;
            mean/=total;
        }
        let target_trend=if !self.jumped {
            (mean*24.).clamp(-13.5,13.5)
        }
        else {
            0.
        };
        self.trend=self.trend*0.9+target_trend*0.1;
        o.cancel_all();
        let mut future=pos;
        if self.stopped {
            self.signal=0.;
            if pos.abs()>0.05 {
                o.target_position_with(0.,Execution{
                    max_per_step:40.,slice:4.,slippage_bps:100.,min_trade:0.01
                });
            }
            return Ok(());
        }
        // Reserve loss capacity for another jump and transaction costs. Exposure
        // contracts with drawdown; the persistent stop cancels and flattens.
        let risk_cap=((m.max_drawdown-dd-35.)/(p*0.05)).clamp(0.,18.);
        self.trend=self.trend.clamp(-risk_cap*0.75,risk_cap*0.75);
        let active=self.signal*risk_cap;
        if self.signal!=0. && (active-pos).abs()>0.1 {
            o.target_position_with(active,Execution{
                max_per_step:40.,slice:4.,slippage_bps:100.,min_trade:0.05
            });
            future=active;
        }
        else if self.signal==0. && (pos-self.trend).abs()>5. {
            future=self.trend+pos.signum();
            o.target_position_with(future,Execution{
                max_per_step:40.,slice:4.,slippage_bps:100.,min_trade:0.05
            });
        }
        // Compete for noise flow while moving quotes away from excess inventory.
        let bb=m.best_bid.unwrap_or(p*0.998);
        let ba=m.best_ask.unwrap_or(p*1.002);
        if m.k==0 {
            self.half=(((p-bb).max(ba-p))/p*10000.).clamp(8.,30.);
        }
        let target=self.trend+(self.drift*1.5+self.flow*0.8).clamp(-2.,2.);
        let skew=(future-target)*1.1;
        let spread=(self.half*0.7).clamp(4.,16.);
        let bid=(p*(1.-(spread+skew)/10000.)).max(bb+self.tick).min(p*(1.-(2.5+skew.max(0.))/10000.));
        let ask=(p*(1.+(spread-skew)/10000.)).min(ba-self.tick).max(p*(1.+(2.5+(-skew).max(0.))/10000.));
        if self.signal==0. {
            let bs=(self.trend+2.5-future).min(risk_cap-future).clamp(0.,2.);
            let az=(2.5-self.trend+future).min(risk_cap+future).clamp(0.,2.);
            if bs>0.01{
                o.bid(bid,bs);
            }
            if az>0.01{
                o.ask(ask,az);
            }
        }
        // Tiny inside orders also keep low-limit taker activity observable.
        o.bid((p/self.tick).floor()*self.tick-self.tick,0.001);
        o.ask((p/self.tick).ceil()*self.tick+self.tick,0.001);
        log(&format!("TREND {} {:.3} {:.3} {:.3} {:.3} {}",m.k,self.trend,mean,plus,minus,self.n));
        log(&format!("K {} {} {:.4} {:.3} {:.1} {:.1} signal {} score {:.2} {:.2} equity {:.2} dd {:.2}",m.k,m.slot-self.start_slot,p,pos,buys,sells,self.signal,su,sd,equity,dd));
        self.prev_pos=pos;
        self.prev_oracle=p;
        self.prev_bid=bid;
        self.prev_ask=ask;
        Ok(())
    }
}
mtx_kit::export!(Maker);
