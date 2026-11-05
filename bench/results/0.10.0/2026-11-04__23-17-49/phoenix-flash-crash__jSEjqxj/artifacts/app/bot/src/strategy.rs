//! The trading logic, free of any chain access so the backtester can run it.
//!
//! Each step the bot feeds the index price in; the strategy answers with the
//! position it wants (in SPCX, signed, within the cap). Prices are only ever
//! compared as ratios, so nothing here depends on SPCX's price level.
//!
//! The world, as the scenario family describes it: a quiet market that
//! wobbles around a flat level, one shock of up to three legs that each land
//! at once or ramp over seconds, a hold of 8-35 s, then an aftermath that
//! mostly reverts. Quiet stretches may carry a fizzle: a 0.4-2% jump that
//! gives itself back within 1.5-10 s. Phoenix moves its index at most 4% a
//! step, so a larger jump lands in equal parts on consecutive steps.
//!
//! The strategy follows episodes: a jump off a quiet base starts one.
//! - ride: while the episode keeps making progress (a ramp, or the next part
//!   of a split jump), hold the move's way.
//! - fizzle fade: once it settles, a jump the size of a fizzle is faded until
//!   it gives itself back, or until a shock leg's hold could end.
//! - shock fade: a shock that has stopped growing, large enough (or in its
//!   third leg), is faded until it retraces.

#[derive(Clone, Debug)]
pub struct Params {
    pub max_pos: f64,
    /// One-step move (fraction) that signals a split jump.
    pub split_th: f64,
    /// A move off the base (fraction) that starts an episode.
    pub ep_th: f64,
    /// Band (fraction) the base's recent history must sit in to be quiet.
    pub still: f64,
    /// Steps of history the base is the median of.
    pub base_n: usize,
    /// Progress (fraction) that counts as the move going on.
    pub prog_th: f64,
    /// Steps without progress after which the move has settled.
    pub settle: usize,
    pub ride_size: f64,
    /// Smallest episode size worth riding.
    pub ride_min: f64,
    /// Fizzle fade: episode sizes, size, and how long (steps after the
    /// move's last progress) to give it.
    pub fiz_lo: f64,
    pub fiz_hi: f64,
    pub fiz_size: f64,
    /// Fizzle fade size once this quiet stretch has had its fizzle (so a
    /// new jump is more likely a leg), and in the first steps (before any
    /// shock can start).
    pub fiz_size2: f64,
    pub fiz_size_early: f64,
    pub early_steps: usize,
    /// Ride size on a move's first step, before it has shown a ramp.
    pub ride_size1: f64,
    /// Stop fading the shock once an aftermath has held (1) or not (0).
    pub aft_block: f64,
    pub fiz_max: usize,
    /// Count as given back within this fraction of the base.
    pub fiz_back: f64,
    /// Shock fade: smallest shock (fraction of the start level), size from
    /// fade_size_min at fade_min up to the cap at fade_full.
    pub fade_min: f64,
    pub fade_full: f64,
    pub fade_size_min: f64,
    /// Steps the shock must have stopped growing before fading it.
    pub fade_wait: usize,
    /// Close the fade once the shock has retraced this share of itself.
    pub fade_tp: f64,
    /// Smallest shock to fade once three legs have been seen.
    pub fade3_min: f64,
    /// Tracker: a move off the plateau (fraction) that may be a leg; within
    /// tr_back of the plateau it has given itself back; it must hold
    /// tr_confirm steps after its last progress to be more than a fizzle,
    /// unless it is past tr_big, which no fizzle reaches.
    pub tr_move: f64,
    pub tr_back: f64,
    pub tr_confirm: usize,
    pub tr_big: f64,
    /// Fizzle fade sizes by stretch: before the shock, in a pause (with the
    /// shock, against it), and after the aftermath.
    pub fiz_pre: f64,
    pub fiz_with: f64,
    pub fiz_against: f64,
    pub fiz_post: f64,
    /// Steps after the shock's last leg to keep a fade waiting for the
    /// aftermath.
    pub fade_timeout: usize,
    /// Once a split jump's last part has landed, stop riding it (1) or not (0).
    pub split_settle: f64,
    /// Drawdown (USD) the position may risk reaching on the worst step.
    pub dd_limit: f64,
    /// Worst one-step move to budget for (fraction): Phoenix's 4% cap plus slack.
    pub worst_step: f64,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            max_pos: 70.0,
            split_th: 0.0195,
            ep_th: 0.0017488,
            still: 0.004773,
            base_n: 2,
            prog_th: 0.0019072,
            settle: 3,
            ride_size: 70.0,
            ride_min: 0.0021057,
            fiz_lo: 0.0069846,
            fiz_hi: 0.021977,
            fiz_size: 70.0,
            fiz_size2: 70.0,
            fiz_size_early: 50.0,
            early_steps: 0,
            ride_size1: 70.0,
            aft_block: 0.0,
            fiz_max: 22,
            fiz_back: -0.001468,
            fade_min: 0.02459,
            fade_full: 0.12015,
            fade_size_min: 70.0,
            fade_wait: 7,
            fade_tp: 0.1404,
            fade3_min: 0.0074026,
            tr_move: 0.0049666,
            tr_back: 0.0025,
            tr_confirm: 22,
            tr_big: 0.021,
            fiz_pre: 70.0,
            fiz_with: 65.658,
            fiz_against: 70.0,
            fiz_post: 70.0,
            fade_timeout: 196,
            split_settle: 0.0,
            dd_limit: 850.0,
            worst_step: 0.045,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Phase {
    /// No episode: the price sits around its base.
    Quiet,
    /// The price jumped off the base and is still moving its way.
    Moving,
    /// The move stopped; waiting to see if it gives itself back.
    Settled,
}

/// A move off the tracker's plateau, not yet known to be more than a fizzle.
#[derive(Clone, Copy, Debug)]
struct Cand {
    dir: f64,
    /// Extreme level (relative to the start) and the step it was reached.
    ext: f64,
    ext_at: usize,
}

pub struct Strategy {
    pub p: Params,
    hist: Vec<f64>,
    start: f64,
    phase: Phase,
    /// The episode: its base, direction, first step, extreme, and the step
    /// the extreme was last pushed.
    base: f64,
    dir: f64,
    ep_at: usize,
    ext: f64,
    ext_at: usize,
    /// Split jump: the part seen and how many so far.
    split_m: f64,
    split_n: u32,
    /// Legs of the shock seen: settled episodes the shock's way.
    pub legs: u32,
    shock_dir: f64,
    /// This quiet stretch already had its fizzle.
    fizzled: bool,
    /// An aftermath (a move against the shock that held) has happened.
    aftermath: bool,
    /// Steps of progress the episode has made after its first.
    progs: u32,
    /// Shock fade: on, the extreme it fades, and whether it already paid.
    fade_dir: f64,
    fade_ext: f64,
    fade_done: bool,
    /// Tracker: the confirmed level (relative to the start), the pending
    /// move, legs, the shock's direction, whether the aftermath came, and the
    /// step the last leg ended.
    plateau: f64,
    cand: Option<Cand>,
    pub t_legs: u32,
    t_dir: f64,
    pub t_aft: bool,
    leg_end: usize,
    peak: f64,
    /// Running extremes of the level and when they were set, and the last
    /// two jumps (step, signed size), for the features.
    cap: f64,
    lmax: (f64, usize),
    lmin: (f64, usize),
    jumps: Vec<(usize, f64)>,
    last_tgt: f64,
    /// What set the last position: 0 none, 1 shock fade, 2 fizzle fade,
    /// 3 ride, 4 split (for analysis).
    pub reason: u8,
}

fn median(v: &[f64]) -> f64 {
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap());
    s[s.len() / 2]
}

impl Strategy {
    pub fn new(p: Params) -> Self {
        Self {
            p,
            hist: Vec::new(),
            start: 0.0,
            phase: Phase::Quiet,
            base: 0.0,
            dir: 0.0,
            ep_at: 0,
            ext: 0.0,
            ext_at: 0,
            split_m: 0.0,
            split_n: 0,
            legs: 0,
            shock_dir: 0.0,
            fizzled: false,
            aftermath: false,
            progs: 0,
            fade_dir: 0.0,
            fade_ext: 0.0,
            fade_done: false,
            plateau: 0.0,
            cand: None,
            t_legs: 0,
            t_dir: 0.0,
            t_aft: false,
            leg_end: 0,
            peak: f64::NEG_INFINITY,
            cap: 0.0,
            lmax: (0.0, 0),
            lmin: (0.0, 0),
            jumps: Vec::new(),
            last_tgt: 0.0,
            reason: 0,
        }
    }

    /// The position to hold after seeing index `px`, with the account worth
    /// `value` (USD, any offset).
    pub fn step(&mut self, px: f64, value: f64) -> f64 {
        self.peak = self.peak.max(value);
        let dd = self.peak - value;
        let room = ((self.p.dd_limit - dd) / (px * self.p.worst_step)).max(0.0);
        let cap = self.p.max_pos.min(room);
        self.cap = cap;
        let t = self.signal(px);
        self.last_tgt = t;
        t.clamp(-cap, cap)
    }

    /// The largest position the drawdown budget allows after the last step.
    pub fn cap(&self) -> f64 {
        self.cap
    }

    /// What the model sees after a step: the price's recent shape and the
    /// rules' view of it. `remaining` is the steps left in the session.
    pub fn features(&self, remaining: f64) -> Vec<f32> {
        let n = self.hist.len();
        let i = n - 1;
        let px = self.hist[i];
        let l = px / self.start - 1.0;
        let r = |k: usize| px / self.hist[i.saturating_sub(k)] - 1.0;
        let med = |k: usize| {
            let a = &self.hist[n.saturating_sub(k)..];
            px / median(a) - 1.0
        };
        let cap = |x: usize| x.min(200) as f64;
        let (c_on, c_size, c_age) = match self.cand {
            Some(c) => (1.0, c.ext - self.plateau, cap(i - c.ext_at)),
            None => (0.0, 0.0, 0.0),
        };
        let ep_on = self.phase != Phase::Quiet;
        let j1 = self.jumps.last().copied();
        let j2 = if self.jumps.len() >= 2 { Some(self.jumps[self.jumps.len() - 2]) } else { None };
        let vol: f64 = (1..=10.min(i)).map(|k| (self.hist[i + 1 - k] / self.hist[i - k] - 1.0).abs()).sum();
        let f = vec![
            i as f64,
            remaining,
            l,
            r(1),
            r(2),
            r(3),
            r(5),
            r(8),
            r(13),
            r(21),
            r(34),
            self.lmax.0,
            self.lmin.0,
            cap(i - self.lmax.1),
            cap(i - self.lmin.1),
            med(10),
            med(30),
            match self.phase { Phase::Quiet => 0.0, Phase::Moving => 1.0, Phase::Settled => 2.0 },
            if ep_on { self.ext / self.base - 1.0 } else { 0.0 },
            if ep_on { cap(i - self.ext_at) } else { 0.0 },
            if ep_on { self.progs as f64 } else { 0.0 },
            if ep_on { px / self.base - 1.0 } else { 0.0 },
            self.t_legs as f64,
            self.t_dir,
            if self.t_aft { 1.0 } else { 0.0 },
            self.plateau,
            if self.t_legs > 0 { cap(i - self.leg_end) } else { 200.0 },
            c_on,
            c_size,
            c_age,
            self.fade_dir,
            self.fade_ext,
            self.split_n as f64,
            self.last_tgt / self.p.max_pos,
            j1.map_or(200.0, |j| cap(i - j.0)),
            j1.map_or(0.0, |j| j.1),
            j2.map_or(200.0, |j| cap(i - j.0)),
            j2.map_or(0.0, |j| j.1),
            self.jumps.len() as f64,
            vol,
        ];
        f.into_iter().map(|x| x as f32).collect()
    }

    fn quiet_base(&self) -> Option<f64> {
        let n = self.hist.len();
        let k = self.p.base_n.min(n - 1);
        if k < 2 {
            return None;
        }
        let b = &self.hist[n - 1 - k..n - 1];
        let m = median(b);
        b.iter()
            .all(|x| (x / m - 1.0).abs() <= self.p.still)
            .then_some(m)
    }

    /// The shock tracker's view of level `l` (relative to the start) at step `i`.
    fn track(&mut self, i: usize, l: f64) {
        let p = &self.p;
        match self.cand {
            None => {
                if (l - self.plateau).abs() >= p.tr_move {
                    self.cand = Some(Cand {
                        dir: (l - self.plateau).signum(),
                        ext: l,
                        ext_at: i,
                    });
                } else if i >= 4 {
                    // The plateau follows the quiet level's slow wobble.
                    let n = self.hist.len();
                    let k = n.min(9);
                    let s: Vec<f64> = self.hist[n - k..]
                        .iter()
                        .map(|x| x / self.start - 1.0)
                        .collect();
                    let m = median(&s);
                    if (m - self.plateau).abs() < p.tr_move {
                        self.plateau = m;
                    }
                }
            }
            Some(mut c) => {
                if (l - c.ext) * c.dir > 0.0 {
                    c.ext = l;
                    c.ext_at = i;
                }
                let off = (l - self.plateau) * c.dir;
                let size = (c.ext - self.plateau).abs();
                let held = i - c.ext_at;
                if off < p.tr_back {
                    // Given back: a fizzle, or wobble.
                    self.cand = None;
                } else if held >= p.tr_confirm || (size >= p.tr_big && held >= p.settle) {
                    if !self.t_aft {
                        if self.t_legs == 0 {
                            self.t_legs = 1;
                            self.t_dir = c.dir;
                        } else if c.dir == self.t_dir {
                            self.t_legs += 1;
                            if self.t_legs > 3 {
                                // A fourth leg is an extension: the aftermath.
                                self.t_aft = true;
                            }
                        } else {
                            self.t_aft = true;
                        }
                    }
                    self.leg_end = c.ext_at;
                    self.plateau = l;
                    self.cand = None;
                } else {
                    self.cand = Some(c);
                }
            }
        }
    }

    fn signal(&mut self, px: f64) -> f64 {
        let p = self.p.clone();
        if self.hist.is_empty() {
            self.start = px;
        }
        self.hist.push(px);
        let n = self.hist.len();
        let i = n - 1;
        if n < 2 {
            return 0.0;
        }
        let prev = self.hist[n - 2];
        let m = px / prev - 1.0;
        let lv = px / self.start - 1.0;
        if lv > self.lmax.0 {
            self.lmax = (lv, i);
        }
        if lv < self.lmin.0 {
            self.lmin = (lv, i);
        }
        if m.abs() >= 0.003 {
            self.jumps.push((i, m));
        }
        self.track(i, px / self.start - 1.0);

        // Split: equal parts on consecutive steps.
        let mut split_more = false;
        if m.abs() >= p.split_th {
            if self.split_n > 0 && (m / self.split_m - 1.0).abs() < 0.08 {
                self.split_n += 1;
            } else {
                self.split_n = 1;
            }
            self.split_m = m;
            // After two parts, a third needs parts over 8/3 %.
            split_more = self.split_n == 1 || (self.split_n == 2 && m.abs() > 0.0262);
        } else {
            self.split_n = 0;
        }

        // Episodes.
        if self.phase != Phase::Quiet {
            if (px / self.ext - 1.0) * self.dir > p.prog_th {
                self.ext = px;
                self.ext_at = i;
                self.phase = Phase::Moving;
                self.progs += 1;
            } else if self.phase == Phase::Moving && i - self.ext_at >= p.settle {
                self.phase = Phase::Settled;
            }
            let size = (self.ext / self.base - 1.0).abs();
            let back = (px / self.base - 1.0) * self.dir;
            if back < p.fiz_back {
                // Given back (or reversed through the base).
                if size >= p.fiz_lo && size <= p.fiz_hi {
                    self.fizzled = true;
                }
                self.phase = Phase::Quiet;
            } else if self.phase == Phase::Settled && i - self.ext_at == p.fiz_max {
                // Held past a fizzle's likely life: a leg, if the shock's way.
                let shock = px / self.start - 1.0;
                if size >= 0.006 && shock * self.dir > 0.0 {
                    if self.shock_dir != self.dir {
                        self.shock_dir = self.dir;
                        self.legs = 0;
                    }
                    self.legs += 1;
                } else if shock * self.dir < 0.0 && self.shock_dir != 0.0 && self.legs > 0 {
                    // Back toward the start after legs: the aftermath.
                    self.aftermath = true;
                }
                self.fizzled = false;
                // The held level is the new base for whatever comes next.
                self.phase = Phase::Quiet;
            }
        }
        if p.split_settle > 0.5 && self.split_n >= 2 && !split_more && self.phase == Phase::Moving {
            self.phase = Phase::Settled;
        }
        if self.phase == Phase::Quiet {
            if let Some(b) = self.quiet_base() {
                let j = px / b - 1.0;
                if j.abs() >= p.ep_th {
                    self.phase = Phase::Moving;
                    self.base = b;
                    self.dir = j.signum();
                    self.ep_at = i;
                    self.ext = px;
                    self.ext_at = i;
                    self.progs = 0;
                }
            }
        }

        // Shock fade.
        let shock = px / self.start - 1.0;
        if shock * self.fade_ext < 0.0 || shock.abs() > self.fade_ext.abs() {
            if shock.abs() > self.fade_ext.abs() + 0.005 || shock * self.fade_ext < 0.0 {
                self.fade_done = false;
            }
            self.fade_ext = shock;
        }
        if self.fade_dir != 0.0 {
            let retraced = 1.0 - shock / self.fade_ext;
            if retraced >= p.fade_tp {
                self.fade_dir = 0.0;
                self.fade_done = true;
            }
        }
        let still_for = i - self.ext_at;
        let min_s = if self.t_legs >= 3 { p.fade3_min } else { p.fade_min };
        let timed_out = self.t_legs >= 1 && i - self.leg_end > p.fade_timeout;
        if timed_out && self.fade_dir != 0.0 {
            self.fade_dir = 0.0;
            self.fade_done = true;
        }
        if self.fade_dir == 0.0
            && !self.fade_done
            && !(self.t_aft && p.aft_block > 0.5)
            && !timed_out
            && self.fade_ext.abs() >= min_s
            && 1.0 - shock / self.fade_ext < p.fade_tp
            && self.phase != Phase::Moving
            && still_for >= p.fade_wait
        {
            self.fade_dir = -self.fade_ext.signum();
        }

        // The position.
        let mut tgt = 0.0;
        self.reason = 0;
        if self.fade_dir != 0.0 {
            self.reason = 1;
            let f = ((self.fade_ext.abs() - p.fade_min) / (p.fade_full - p.fade_min).max(1e-9))
                .clamp(0.0, 1.0);
            tgt = self.fade_dir * (p.fade_size_min + f * (p.max_pos - p.fade_size_min));
        }
        if self.phase == Phase::Settled && tgt == 0.0 {
            let size = (self.ext / self.base - 1.0).abs();
            if size >= p.fiz_lo && size <= p.fiz_hi {
                let q = if i < p.early_steps {
                    p.fiz_size_early
                } else if self.t_legs == 0 {
                    p.fiz_pre
                } else if self.t_aft {
                    p.fiz_post
                } else if self.dir == self.t_dir {
                    p.fiz_with
                } else {
                    p.fiz_against
                };
                tgt = -self.dir * q;
                self.reason = 2;
            }
        }
        if self.phase == Phase::Moving {
            let size = (self.ext / self.base - 1.0).abs();
            if size >= p.ride_min {
                tgt = self.dir * if self.progs == 0 { p.ride_size1 } else { p.ride_size };
                self.reason = 3;
            }
        }
        if split_more {
            tgt = m.signum() * p.max_pos;
            self.reason = 4;
        }
        tgt.clamp(-p.max_pos, p.max_pos)
    }
}
