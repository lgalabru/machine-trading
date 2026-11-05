//! The trading policy: a phase-aware mean-reversion state machine.
//!
//! The market moves in shocks (legs of 0.6-8%), pauses, and aftermaths, with
//! fizzles (0.4-2% moves that fully retrace within 10 s) in quiet stretches.
//! The policy fades moves away from a quiet reference level, sized to a
//! worst-case drawdown budget, rides moves that are still in progress, and
//! steps aside while a move runs against an open fade.
//!
//! Everything is in percent of the session's starting mark; one step is two
//! slots (0.5 s of market time).

pub const MAX_POS: f64 = 70.0;

#[derive(Debug, Clone)]
pub struct Params {
    /// Deviation from the reference that counts as a move (%).
    pub theta: f64,
    /// Worst adverse single-step move the sizing budget must survive (%).
    pub worst: f64,
    /// Dollars kept back from the drawdown budget.
    pub safety: f64,
    /// Fraction of a move given back that counts as a revert.
    pub rho: f64,
    /// Steps without a change after which a fade is abandoned.
    pub quiet_steps: u64,
    /// Steps without a change before a level counts as landed.
    pub stable_k: u64,
    /// Step-to-step change below which the level counts as unchanged (%).
    pub noise: f64,
    /// Leg-fade size scaling: fraction = a + b * size.
    pub a: f64,
    pub b: f64,
    /// Further move beyond the extreme that counts as a new leg (%).
    pub leg_min: f64,
    /// Smallest change in target worth a trade (shares).
    pub hyst: f64,
    /// Steps after a small move during which it may still be a fizzle.
    pub fizzle_steps: u64,
    /// Largest move that can be a fizzle (%).
    pub fizzle_max: f64,
    /// Size for fizzle fades (shares).
    pub sf: f64,
    /// Sizes for leg fades by leg count.
    pub s1: f64,
    pub s2: f64,
    pub s3: f64,
    /// Size for riding a move in progress (shares); 0 disables.
    pub ride: f64,
    /// Steps from the session start before which no shock can occur.
    pub early_steps: u64,
    /// 0: hold a fade through an adverse ramp; 1: go flat while it runs;
    /// 2: ride it at `ride` size while it runs.
    pub adverse_ramp: u8,
    /// Shares over the budget cap tolerated before a forced cut (and cut below it by as much).
    pub cut_margin: f64,
    /// Step move (%) from which a move may be one chunk of a larger move that
    /// Phoenix lands in 4% pieces on consecutive steps; ridden at `chunk_ride`.
    pub chunk_min: f64,
    /// A following step move at least this fraction of the last chunk continues the ride.
    pub chunk_cont: f64,
    /// Size for riding chunked moves (shares); 0 disables.
    pub chunk_ride: f64,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            theta: 0.35,
            worst: 9.0,
            safety: 60.0,
            rho: 0.5,
            quiet_steps: 50,
            stable_k: 4,
            noise: 0.2,
            a: 0.3,
            b: 0.1,
            leg_min: 0.5,
            hyst: 4.0,
            fizzle_steps: 22,
            fizzle_max: 2.1,
            sf: 70.0,
            s1: 50.0,
            s2: 50.0,
            s3: 70.0,
            ride: 70.0,
            early_steps: 20,
            adverse_ramp: 2,
            cut_margin: 1.5,
            chunk_min: 2.0,
            chunk_cont: 0.7,
            chunk_ride: 70.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    PreShock,
    Shock,
    PostAftermath,
}

#[derive(Debug, Clone)]
pub struct State {
    /// Quiet reference level (%).
    pub r: f64,
    /// Extreme deviation from `r` of the current move.
    pub ext: f64,
    pub active: bool,
    pub last_change: u64,
    pub prev_level: Option<f64>,
    pub legs: u32,
    pub t_land: u64,
    pub target: f64,
    pub riding: f64,
    pub phase: Phase,
    /// Direction of a chunked move being ridden (0 when none) and its last chunk size.
    pub chunk_dir: i8,
    pub chunk_size: f64,
}

impl State {
    pub fn new() -> Self {
        Self {
            r: 0.0,
            ext: 0.0,
            active: false,
            last_change: 0,
            prev_level: None,
            legs: 0,
            t_land: 0,
            target: 0.0,
            riding: 0.0,
            phase: Phase::PreShock,
            chunk_dir: 0,
            chunk_size: 0.0,
        }
    }
}

impl Default for State {
    fn default() -> Self {
        Self::new()
    }
}

fn copysign(mag: f64, sign: f64) -> f64 {
    if sign < 0.0 { -mag } else { mag }
}

fn leg_size(p: &Params, s: f64, legs: u32) -> f64 {
    let base = match legs {
        0 | 1 => p.s1,
        2 => p.s2,
        _ => p.s3,
    };
    MAX_POS.min(base * (p.a + p.b * s).clamp(0.0, 1.0))
}

fn fade_size(st: &State, p: &Params, k: u64, s: f64) -> f64 {
    if st.phase == Phase::PostAftermath || (st.phase == Phase::PreShock && st.t_land < p.early_steps) {
        return p.sf;
    }
    if s <= p.fizzle_max && (k - st.t_land) < p.fizzle_steps {
        return p.sf;
    }
    leg_size(p, s, st.legs)
}

/// One decision. `level` is the market's level in percent of the starting
/// mark, `pos` the current position in shares, `budget` the drawdown room
/// left in dollars, `mark` the price in dollars. Returns the target position.
pub fn step(st: &mut State, p: &Params, k: u64, level: f64, pos: f64, budget: f64, mark: f64) -> f64 {
    let c = st.prev_level.map_or(0.0, |prev| level - prev);
    st.prev_level = Some(level);
    let moved_now = c.abs() > p.noise;
    if moved_now {
        st.last_change = k;
    }
    let cap = ((budget - p.safety) / (p.worst / 100.0 * mark)).max(0.0);

    // Chunked moves: Phoenix lands a move larger than 4% as equal pieces on
    // consecutive steps, so a big step move is likely followed by another.
    if st.chunk_dir != 0 {
        if c.abs() >= p.chunk_cont * st.chunk_size && (c > 0.0) == (st.chunk_dir > 0) {
            st.chunk_size = c.abs();
            st.target = f64::from(st.chunk_dir) * p.chunk_ride.min(cap);
            return st.target;
        }
        st.chunk_dir = 0;
    } else if c.abs() >= p.chunk_min && p.chunk_ride > 0.0 {
        st.chunk_dir = if c > 0.0 { 1 } else { -1 };
        st.chunk_size = c.abs();
        st.target = f64::from(st.chunk_dir) * p.chunk_ride.min(cap);
        return st.target;
    }

    let dev = level - st.r;
    let stable = (k - st.last_change) >= p.stable_k;

    if !st.active {
        if dev.abs() >= p.theta {
            if stable {
                st.active = true;
                st.ext = dev;
                st.legs = 1;
                st.t_land = st.last_change;
                st.riding = 0.0;
                if st.phase == Phase::PreShock && dev.abs() > p.fizzle_max {
                    st.phase = Phase::Shock;
                }
                let size = fade_size(st, p, k, dev.abs());
                st.target = -copysign(size.min(cap), dev);
                return st.target;
            } else if p.ride > 0.0 {
                st.riding = copysign(p.ride.min(cap), dev);
                st.target = st.riding;
                return st.target;
            }
        }
        if st.riding != 0.0 && stable {
            st.riding = 0.0;
        }
        st.target = 0.0;
        return 0.0;
    }

    // Active fade.
    if dev.abs() > st.ext.abs() {
        if dev.abs() - st.ext.abs() >= p.leg_min && stable {
            st.legs += 1;
            st.t_land = st.last_change;
            if st.phase == Phase::PreShock {
                st.phase = Phase::Shock;
            }
        }
        if stable || dev.abs() - st.ext.abs() >= p.leg_min {
            st.ext = dev;
        }
    }
    let ext = st.ext;
    if st.phase == Phase::PreShock
        && ext.abs() <= p.fizzle_max
        && (k - st.t_land) >= p.fizzle_steps
        && st.t_land >= p.early_steps
    {
        st.phase = Phase::Shock;
    }
    let retrace_abs = if ext > 0.0 { ext - dev } else { dev - ext };
    let retrace = if ext != 0.0 { retrace_abs / ext.abs() } else { 0.0 };
    let mut done = false;
    let mut aftermath = false;
    if stable
        && (retrace_abs >= p.fizzle_max + 0.1
            || (retrace >= p.rho && (k - st.last_change) >= p.fizzle_steps))
    {
        done = true;
        aftermath = st.phase == Phase::Shock;
    }
    if (k - st.last_change) >= p.quiet_steps {
        done = true;
        aftermath = st.phase == Phase::Shock;
    }
    if dev == 0.0 || (dev > 0.0) != (ext > 0.0) {
        done = true;
    }
    if done {
        st.active = false;
        st.r = level;
        st.ext = 0.0;
        st.legs = 0;
        st.target = 0.0;
        if aftermath {
            st.phase = Phase::PostAftermath;
        }
        return 0.0;
    }
    let adverse_moving = moved_now && (c > 0.0) == (ext > 0.0);
    if !stable && adverse_moving && p.adverse_ramp > 0 && st.phase != Phase::PostAftermath {
        st.target = if p.adverse_ramp == 1 { 0.0 } else { copysign(p.ride.min(cap), ext) };
        return st.target;
    }
    let size = fade_size(st, p, k, ext.abs());
    let desired = -copysign(size.min(cap), ext);
    let cur = st.target;
    if pos.abs() > cap + p.cut_margin {
        // Over the budget cap: trim the position we hold, whichever way it points.
        st.target = copysign((cap - p.cut_margin).max(0.0), pos);
        return st.target;
    }
    if ((desired - cur).abs() > p.hyst || (cur != 0.0 && (cur > 0.0) != (desired > 0.0))) && stable {
        st.target = desired;
        return desired;
    }
    cur
}
