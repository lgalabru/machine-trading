mod features;
mod predict;
use mtx_kit::{log, prelude::*, rpc_accounts};
use mtx_kit::venue::{PhoenixVenue, key, phoenix_rise::accounts::owned::{PerpAssetMapOwned, Trader}};

#[derive(Default)]
struct Carry {
    map: String, trader: String, asset_id: u64, lots: f64,
    period: f64, funding: f64, peak: f64, halted: bool,
    base: f64, fcap:f64, features:features::Features,target:f64,
}
impl Strategy for Carry {
    fn start(&mut self, s: &Session) -> BotResult {
        let v = PhoenixVenue::from_json(&s.venue)?;
        let m = v.market(&s.symbol)?;
        self.asset_id = m.asset_id as u64;
        self.lots = 10f64.powi(m.base_lots_decimals as i32);
        self.map = v.perp_asset_map()?.to_string();
        self.trader = v.trader_account(&key(&s.authority)?).to_string();
        self.period = 30.; self.peak = 10000.; self.base=s.start_mark;self.fcap=1.;
        Ok(())
    }
    fn step(&mut self, m: &Market, o: &mut Orders) -> BotResult {
        // The desk values open inventory at the final book; trading on the
        // last turn only spends spread and fees.
        if m.last { return Ok(()); }
        let mid = match (m.best_bid,m.best_ask) { (Some(b),Some(a)) => (b+a)*0.5, _=>m.mark };
        let mut equity = 10000.+m.pnl;
        if let Ok(a) = rpc_accounts(&[&self.map,&self.trader]) {
            if let Some(Some(data)) = a.first() {
                if let Ok(map) = PerpAssetMapOwned::try_from_account_bytes(data) {
                    if let Some(md) = map.get_by_symbol("SPCX") {
                        self.funding = md.funding_accumulator.cumulative_funding_rate.as_inner() as f64;
                        self.period = md.funding_accumulator.funding_period_seconds.as_inner() as f64;
                        self.fcap=md.funding_accumulator.max_funding_rate.as_inner() as f64*self.lots/1e6*self.period/(md.funding_accumulator.funding_interval_seconds.as_inner() as f64)/self.base*100.;
                    }
                }
            }
            if let Some(Some(data)) = a.get(1) {
                if let Ok(t) = Trader::try_from_account_bytes(data) {
                    equity = t.state.quote_lot_collateral.as_inner() as f64 / 1e6;
                    for (id,p) in &t.positions.entries {
                        if *id == self.asset_id {
                            let lots = p.base_lot_position.as_inner() as f64;
                            equity += lots / self.lots * mid + p.virtual_quote_lot_position.as_inner() as f64/1e6;
                            equity -= lots * (self.funding-p.cumulative_funding_snapshot.as_inner() as f64)/1e6;
                        }
                    }
                }
            }
        }
        self.peak = self.peak.max(equity);
        if self.peak-equity > m.max_drawdown*0.60 { self.halted=true; }
        let gap = mid/m.oracle-1.;
        let (x,sign)=self.features.update((m.oracle/self.base-1.)*100.,(mid/self.base-1.)*100.,self.period,self.fcap);
        let prediction=(predict::predict(&x,0)*0.5+predict::predict(&x,1)*0.5)*sign
            * ((m.slots_left as f64 * 0.25)/6.).min(1.);
        if prediction>0.15 {self.target=m.cap;} else if prediction < -0.15 {self.target=-m.cap;}
        else if self.target>0. && prediction < -0.10 {self.target=0.;}
        else if self.target<0. && prediction > 0.10 {self.target=0.;}
        let dd=self.peak-equity;
        let risk_cap=m.cap.min((m.max_drawdown-dd-150.).max(0.)/(0.06*mid));
        let target = if self.halted {0.} else {self.target.clamp(-risk_cap,risk_cap)};
        // Pace ordinary orders, but act immediately on strong forecasts or
        // the risk stop. Price limits use the current book, not a stale mark.
        let pace=if self.halted || prediction.abs()>0.30 {140.} else {35.};
        let delta=(target-m.position.size).clamp(-pace,pace);
        let mut remaining=delta.abs();
        while remaining>=0.1 {
            let size=remaining.min(10.);
            if delta>0. {o.buy(size,m.best_ask.unwrap_or(mid)*1.005);}
            else {o.sell(size,m.best_bid.unwrap_or(mid)*0.995);}
            remaining-=size;
        }
        log(&format!("k={} mid={:.4} oracle={:.4} index={:.4} mark={:.4} gap={:.5} pos={:.3} target={:.1} equity={:.3} period={} cfr={} pred={:.5} sends={:?}",m.k,mid,m.oracle,m.index,m.mark,gap,m.position.size,target,equity,self.period,self.funding,prediction,m.sends));
        Ok(())
    }
}
mtx_kit::export!(Carry);

#[cfg(test)]
mod tests {
    use super::*;
    use mtx_kit::{Intent, Side, Position, step_offline};

    fn strategy() -> Carry {
        Carry { base:143.11, period:30., fcap:1., lots:1000., peak:10000., ..Carry::default() }
    }
    fn market(position:f64) -> Market {
        Market { mark:143.0, oracle:143.0, index:143.0,
            best_bid:Some(142.99), best_ask:Some(143.01),
            cap:70., max_drawdown:1000., slots_left:400,
            position:Position {size:position,entry_usd:143.0}, ..Market::default() }
    }
    #[test]
    fn final_turn_preserves_inventory_without_spending_fees() {
        let mut bot=strategy(); bot.target=-70.;
        let mut m=market(70.);m.last=true;
        assert!(step_offline(&mut bot,&m).unwrap().is_empty());
    }
    #[test]
    fn drawdown_stop_flattens_and_remains_latched() {
        let mut bot=strategy();let mut m=market(70.);m.pnl=-650.;
        let orders=step_offline(&mut bot,&m).unwrap();
        let mut sold=0.;
        for order in orders {
            match order {Intent::Market {side:Side::Ask,size,..}=>{assert!(size<=10.);sold+=size;},_=>panic!("risk stop must only sell")}
        }
        assert_eq!(sold,70.);assert!(bot.halted);
        m.pnl=100.;m.position.size=0.;
        assert!(step_offline(&mut bot,&m).unwrap().is_empty());
        assert!(bot.halted);
    }
    #[test]
    fn current_book_limits_and_remaining_cap_control_execution() {
        let mut bot=strategy();bot.target=70.;let mut m=market(65.);m.mark=100.;
        let orders=step_offline(&mut bot,&m).unwrap();
        let mut bought=0.;
        for order in orders {
            match order {Intent::Market {side:Side::Bid,size,limit}=>{assert!(limit>=143.01 && limit<145.);bought+=size;},_=>panic!("expected a capped buy")}
        }
        assert_eq!(bought,5.);
    }
}
