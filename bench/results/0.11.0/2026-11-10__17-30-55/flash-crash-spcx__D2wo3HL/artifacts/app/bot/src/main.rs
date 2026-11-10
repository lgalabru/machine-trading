//! SPCX shock bot: rides a move while it is landing, then fades it once it
//! stops, sized by displacement from a slow anchor, under a drawdown-aware
//! position cap.

use mtx_kit::prelude::*;
use tracing::info;

#[derive(Default)]
struct ShockBot {
    anchor: f64,
    prev_mark: f64,
    /// Current fade target (signed shares), 0 when none.
    fade: f64,
    fade_peak_disp: f64,
    /// Signed ride target while a move is landing, 0 when none.
    ride: f64,
    quiet_steps: u32,
    /// Steps of quiet needed before the anchor may re-settle and new fades open.
    cooldown: u32,
}

const RISK_BUDGET: f64 = 750.0;
const WORST_STEP: f64 = 0.08;
const D0: f64 = 0.015;
const D1: f64 = 0.035;
const MOVE_STEP: f64 = 0.0030;
const QUIET_BEFORE_FADE: u32 = 3;
const RIDE_STEP: f64 = 0.006;
const RIDE_SIZE: f64 = 70.0;
const TAKE_PROFIT: f64 = 0.40;
const COOLDOWN_STEPS: u32 = 6;

impl ShockBot {
    fn size_for(&self, disp: f64, cap: f64) -> f64 {
        let x = ((disp.abs() - D0) / (D1 - D0)).clamp(0.0, 1.0);
        cap * x
    }
    fn fade_target(&self, disp: f64, cap: f64) -> f64 {
        let want = self.size_for(disp, cap);
        if disp > 0.0 { -want } else { want }
    }
}

impl Strategy for ShockBot {
    async fn start(&mut self, session: &Session) -> BotResult {
        info!(steps = session.hello.steps, cap = session.env.max_position, "started");
        Ok(())
    }

    async fn step(&mut self, m: &Market, orders: &mut Orders<'_>) -> BotResult {
        let mark = m.mark;
        if self.anchor == 0.0 {
            self.anchor = mark;
            self.prev_mark = mark;
        }
        let m1 = mark / self.prev_mark - 1.0;
        self.prev_mark = mark;
        let moving = m1.abs() >= MOVE_STEP;
        if moving {
            self.quiet_steps = 0;
        } else {
            self.quiet_steps += 1;
        }
        let disp = mark / self.anchor - 1.0;

        // Anchor: drift slowly toward the mark when flat and near it; after a
        // cooldown of quiet steps with no position, settle where the market is.
        if self.fade == 0.0 && self.ride == 0.0 {
            if disp.abs() < 0.004 {
                self.anchor += (mark - self.anchor) * 0.03;
            }
            if self.cooldown > 0 && self.quiet_steps >= COOLDOWN_STEPS {
                self.cooldown = 0;
                self.anchor = mark;
            }
        }

        let dd_room = (RISK_BUDGET - m.drawdown).max(0.0);
        let risk_cap = (dd_room / (WORST_STEP * mark)).min(m.cap);

        let mut target;
        if m1.abs() >= RIDE_STEP && self.cooldown == 0 {
            // A move is landing: ride it, dropping any fade.
            self.ride = if m1 > 0.0 { RIDE_SIZE } else { -RIDE_SIZE };
            self.fade = 0.0;
            self.fade_peak_disp = 0.0;
            target = self.ride;
        } else if self.ride != 0.0 {
            if moving && (m1 > 0.0) != (self.ride > 0.0) {
                // Turned against us: drop the ride, no fade yet.
                self.ride = 0.0;
                target = 0.0;
            } else if moving || self.quiet_steps < QUIET_BEFORE_FADE {
                target = self.ride; // still going our way, or a gap between parts
            } else {
                // The move stopped (or turned): switch to the fade.
                self.ride = 0.0;
                if disp.abs() >= D0 {
                    self.fade = self.fade_target(disp, m.cap);
                    self.fade_peak_disp = disp.abs();
                }
                target = self.fade;
            }
        } else if self.fade != 0.0 {
            self.fade_peak_disp = self.fade_peak_disp.max(disp.abs());
            let same_side = (disp < 0.0 && self.fade > 0.0) || (disp > 0.0 && self.fade < 0.0);
            if !same_side || disp.abs() <= self.fade_peak_disp * TAKE_PROFIT {
                self.fade = 0.0;
                self.fade_peak_disp = 0.0;
                self.cooldown = 1;
                self.quiet_steps = 0;
                target = 0.0;
            } else if !moving {
                self.fade = self.fade_target(disp, m.cap);
                target = self.fade;
            } else {
                target = self.fade;
            }
        } else if !moving && self.quiet_steps >= QUIET_BEFORE_FADE && disp.abs() >= D0 && self.cooldown == 0 {
            self.fade = self.fade_target(disp, m.cap);
            self.fade_peak_disp = disp.abs();
            target = self.fade;
        } else {
            target = 0.0;
        }

        let capped = target.clamp(-risk_cap, risk_cap);
        info!(
            k = m.k, mark, pos = m.position.size, pnl = format!("{:.0}", m.pnl),
            dd = format!("{:.0}", m.drawdown), disp = format!("{:.3}%", disp * 100.0),
            m1 = format!("{:.3}%", m1 * 100.0), target = format!("{:.1}", capped),
            ride = self.ride, fade = format!("{:.1}", self.fade), cd = self.cooldown, "step"
        );
        if (capped - m.position.size).abs() >= 1.0 {
            orders.target_position_with(
                capped,
                Execution { slice: 10.0, slippage_bps: 100.0, ..Execution::default() },
            );
        }
        Ok(())
    }
}

fn main() -> BotResult {
    mtx_kit::run(ShockBot::default())
}
