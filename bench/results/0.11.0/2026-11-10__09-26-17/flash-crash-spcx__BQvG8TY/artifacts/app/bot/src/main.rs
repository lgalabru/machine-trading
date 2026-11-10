//! Shock trader for SPCX: ride a move while it runs, fade a shock once it
//! stalls, and size every position so a further jump can't breach the
//! mandate's drawdown limit.

use mtx_kit::prelude::*;
use tracing::info;

// Levels are in percent of the session's starting mark.
const ENTRY: f64 = 1.5; // fade a deviation at least this large...
const STALL: u64 = 4; // ...once it has made no new extreme for this many steps
const NEWEXT: f64 = 0.05; // a new extreme must beat the old by this much
const MAXGB: f64 = 0.2; // don't fade a shock that has already given back this share
const EXITGB: f64 = 0.8; // close a fade once the shock gives back this share
const MOM: f64 = 0.3; // ride a one-step move at least this large
const RSTALL: u64 = 3; // close a ride after this many steps without a new extreme
const RSTOP: f64 = 0.5; // or once it comes back this far from its best
const DD_BUFFER: f64 = 450.0; // USD kept in reserve below the drawdown limit
const JUMP: f64 = 0.045; // the largest one-step move to size against
const STOP: f64 = 2.0; // close a fade that goes this much further against it, and fade no more

#[derive(Default)]
struct Shock {
    m0: f64,
    prev: f64,
    k: u64,
    ext: f64,
    ext_k: u64,
    ride: f64,
    ride_lvl: f64,
    entry_ext: f64,
    target: f64,
    stopped: bool,
}

impl Strategy for Shock {
    async fn step(&mut self, market: &Market, orders: &mut Orders<'_>) -> BotResult {
        if !(market.mark > 0.0) {
            return Ok(());
        }
        if self.m0 == 0.0 {
            self.m0 = market.mark;
            self.prev = 0.0;
        }
        let k = self.k;
        self.k += 1;
        let dv = (market.mark / self.m0 - 1.0) * 100.0;
        let size = market.cap;

        if dv.abs() > self.ext.abs() + NEWEXT || dv * self.ext < 0.0 || dv.abs() < ENTRY * 0.5 {
            self.ext = dv;
            self.ext_k = k;
        }
        let mv = dv - self.prev;
        self.prev = dv;
        let flat = self.target == 0.0;
        let mut tgt = self.target;
        if self.ride != 0.0 {
            if k - self.ext_k >= RSTALL || (dv - self.ride_lvl) * self.ride <= -RSTOP {
                tgt = 0.0;
                self.ride = 0.0;
            } else if (dv - self.ride_lvl) * self.ride > 0.0 {
                self.ride_lvl = dv;
            }
        } else if flat && mv.abs() >= MOM && k == self.ext_k {
            self.ride = mv.signum();
            self.ride_lvl = dv;
            tgt = self.ride * size;
        } else if flat {
            if !self.stopped
                && dv.abs() >= ENTRY
                && k - self.ext_k >= STALL
                && dv.abs() >= self.ext.abs() * (1.0 - MAXGB)
            {
                tgt = -dv.signum() * size;
                self.entry_ext = self.ext;
            }
        } else {
            let gb = if dv * self.entry_ext > 0.0 {
                (self.entry_ext.abs() - dv.abs()) / self.entry_ext.abs()
            } else {
                2.0
            };
            if dv * self.entry_ext > 0.0 && dv.abs() - self.entry_ext.abs() >= STOP {
                tgt = 0.0;
                self.stopped = true;
            } else if gb >= EXITGB {
                tgt = 0.0;
                self.ext = dv;
                self.ext_k = k;
            }
        }
        self.target = tgt;

        // Size so that one more jump against the position stays inside the limit.
        let room = market.max_drawdown - market.drawdown - DD_BUFFER;
        let lim = (room / (market.mark * JUMP)).clamp(0.0, market.cap);
        let mut want = tgt.clamp(-lim, lim);
        if market.last {
            want = market.position.size;
        }
        info!(
            step = market.k,
            mark = market.mark,
            dv,
            target = want,
            position = market.position.size,
            pnl = market.pnl,
            dd = market.drawdown,
            "step"
        );
        if (want - market.position.size).abs() >= 1.0 {
            orders.target_position_with(
                want,
                Execution {
                    slippage_bps: 300.0,
                    ..Execution::default()
                },
            );
        }
        Ok(())
    }
}

fn main() -> BotResult {
    mtx_kit::run(Shock::default())
}
