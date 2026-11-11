mod flow;
mod risk;
use mtx_kit::prelude::*;
use tracing::info;

#[derive(Default)]
struct Maker {
    tick: f64,
    prev: f64,
    n: f64,
    sum: f64,
    sumsq: f64,
    peak: f64,
    stopped: bool,
    flow: flow::Flow,
    burst: f64,
    burst_until: u64,
    last_jump: u64,
    start_slot: u64,
    initial_price: f64,
}
impl Strategy for Maker {
    async fn start(&mut self, s: &Session) -> BotResult {
        if s.env.symbol != "SPCX" {
            return Err("SPCX only".into());
        }
        let cfg = s.trader.phoenix.market(&s.env.symbol)?;
        self.tick = s.trader.phoenix.ticks_to_usd(cfg, 1);
        self.peak = 10000.0;
        self.flow.start(s.hello.start_slot);
        self.start_slot = s.hello.start_slot;
        Ok(())
    }
    async fn step(&mut self, m: &Market, o: &mut Orders<'_>) -> BotResult {
        let p = m.oracle;
        let pos = m.position.size;
        if !p.is_finite() || p <= 0.0 {
            o.cancel_all();
            return Ok(());
        }
        if self.initial_price == 0.0 {
            self.initial_price = p;
        }
        let t = o.trader().unwrap();
        self.flow.read(t, m.slot);
        let margin = t
            .phoenix
            .hawkeye(t.authority())
            .view_margin_for_trader(t.account)
            .await?
            .value;
        let equity = (margin.collateral_quote_lots + margin.unrealized_pnl_quote_lots) as f64 / 1e6
            + pos * (p - m.mark);
        self.peak = self.peak.max(equity);
        let dd = (self.peak - equity).max(0.0);
        if dd > m.max_drawdown * 0.55 {
            self.stopped = true;
        }
        // Cancel before reading the book, so our old quotes cannot set its spread.
        t.cancel_all()?;
        if self.stopped || m.last {
            if self.stopped {
                o.target_position_with(
                    0.0,
                    Execution {
                        min_trade: 0.001,
                        slice: 5.0,
                        slippage_bps: 100.0,
                        ..Execution::default()
                    },
                );
            }
            return Ok(());
        }
        if self.prev > 0.0 && p != self.prev {
            let r = (p / self.prev - 1.0) * 10000.0;
            if r.abs() < 50.0 {
                self.n += 1.0;
                self.sum += r;
                self.sumsq += r * r;
            } else {
                self.flow.clear();
                self.burst = 0.0;
                self.burst_until = 0;
                self.last_jump = m.slot;
            }
        }
        self.prev = p;
        let (dir, score) = self.flow.signal(m.slot);
        if score >= 0.4
            && m.slot - self.start_slot >= 30
            && m.slots_left >= 60
            && (self.last_jump == 0 || m.slot - self.last_jump >= 30)
        {
            self.burst = dir;
            self.burst_until = m.slot + 30;
        }
        if m.slot >= self.burst_until || m.slots_left < 60 {
            self.burst = 0.0;
        }
        let var = ((self.sumsq + 400.0) / (self.n + 20.0)).max(4.0);
        let mut lup = 0.0;
        let mut ldown = 0.0;
        for mu in [0.6, 1.2, 1.8] {
            lup += (mu * self.sum / var - self.n * mu * mu / (2.0 * var))
                .clamp(-30.0, 30.0)
                .exp()
                / 3.0;
            ldown += (-mu * self.sum / var - self.n * mu * mu / (2.0 * var))
                .clamp(-30.0, 30.0)
                .exp()
                / 3.0;
        }
        let fup = (self.flow.buys * 1.16_f64.ln() + self.flow.sells * 0.84_f64.ln()) * 0.6;
        let fdown = (self.flow.sells * 1.16_f64.ln() + self.flow.buys * 0.84_f64.ln()) * 0.6;
        let up = lup * fup.clamp(-20.0, 20.0).exp() / 6.0;
        let down = ldown * fdown.clamp(-20.0, 20.0).exp() / 6.0;
        let trend = if self.last_jump == 0 {
            (up - down) / (1.0 + up + down)
        } else {
            0.0
        };
        let cap = risk::cap(m.cap, m.max_drawdown, dd, self.initial_price);
        let target = if self.burst != 0.0 {
            self.burst * cap
        } else {
            trend * cap
        };
        // One target per turn. A risk reduction cannot accidentally reverse
        // an information-driven trade already queued on the order sheet.
        let execution_target = if self.burst != 0.0 && (target - pos) * self.burst > 2.0 {
            Some(target)
        } else if pos.abs() > cap {
            Some(pos.clamp(-cap, cap))
        } else {
            None
        };
        let effective = execution_target.unwrap_or(pos);
        if let Some(dest) = execution_target {
            o.target_position_with(
                dest,
                Execution {
                    min_trade: 0.001,
                    slice: 5.0,
                    slippage_bps: if pos.abs() > cap { 100.0 } else { 45.0 },
                    ..Execution::default()
                },
            );
        }
        let bbo = t.inner.bbo().await?;
        let bb = bbo
            .best_bid_ticks()
            .map(|v| v as f64 * self.tick)
            .unwrap_or(p * 0.998);
        let ba = bbo
            .best_ask_ticks()
            .map(|v| v as f64 * self.tick)
            .unwrap_or(p * 1.002);
        let inv = effective - target;
        let skew = inv * 0.00018 * p;
        let floor = p * if inv.abs() > 3.0 { 0.0002 } else { 0.00035 };
        let mut bid = (bb + self.tick).min(p - floor - skew.max(0.0));
        let mut ask = (ba - self.tick).max(p + floor + (-skew).max(0.0));
        if self.burst > 0.0 {
            ask = ask.max(p * 1.01);
        }
        if self.burst < 0.0 {
            bid = bid.min(p * 0.99);
        }
        // Use the original position as well as the intended one: partial IOC fills remain safe.
        let (bs, az) = risk::quote_room(pos, effective, cap, 2.0);
        if bs >= 0.001 {
            o.bid(bid, bs);
        }
        if az >= 0.001 {
            o.ask(ask, az);
        }
        info!(
            k = m.k,
            p,
            pos,
            equity,
            dd,
            bid,
            ask,
            target,
            score,
            burst = self.burst,
            trend,
            buys = self.flow.buys,
            sells = self.flow.sells,
            "quote"
        );
        Ok(())
    }
}
fn main() -> BotResult {
    mtx_kit::run(Maker::default())
}
