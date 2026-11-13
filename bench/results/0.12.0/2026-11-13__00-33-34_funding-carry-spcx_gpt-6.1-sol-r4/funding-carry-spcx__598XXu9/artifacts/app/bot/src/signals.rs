#[derive(Default)]
pub struct Signals {
    c: Vec<f64>,
    g: Vec<f64>,
    episode: usize,
    esign: f64,
    anchor: f64,
    peak: f64,
    lastchange: usize,
    lastdc: f64,
    lastdg: f64,
    secondc: f64,
    secondg: f64,
    noise: f64,
    low: f64,
    high: f64,
    ninf: f64,
    ncrowd: f64,
    halfsum: f64,
    lagsum: f64,
    halfcount: f64,
    lagcount: f64,
    peakstep: usize,
    half_time: f64,
    lag_time: f64,
    maxgain: f64,
}
impl Signals {
    pub fn observe(&mut self, index: f64, mid: f64, period: f64, cap: f64) -> ([f32; 64], f64) {
        let c = 100. * index.ln();
        let g = 100. * (mid / index).ln();
        let k = self.c.len();
        if k == 0 {
            self.anchor = c;
            self.noise = 0.03;
            self.low = c;
            self.high = c;
        }
        if k > 0 && ((c - self.c[k - 1]).abs() > 1e-6 || (g - self.g[k - 1]).abs() > 1e-6) {
            self.secondc = self.lastdc;
            self.secondg = self.lastdg;
            self.lastdc = c - self.c[k - 1];
            self.lastdg = g - self.g[k - 1];
            self.lastchange = k;
            self.noise = 0.96 * self.noise + 0.04 * self.lastdc.abs().min(0.5);
        }
        let ending = self.esign != 0. && (g.abs() < 0.025 || self.esign * g < 0.);
        if ending {
            let gain = (c - self.anchor) * self.esign;
            if gain > self.peak * 0.8 {
                self.ninf += 1.;
                if self.lag_time > 0. {
                    self.lagsum += self.lag_time;
                    self.lagcount += 1.;
                }
            } else {
                self.ncrowd += 1.;
                if self.half_time > 0. {
                    self.halfsum += self.half_time;
                    self.halfcount += 1.;
                }
            }
            self.esign = 0.;
        }
        if g.abs() > 0.08 && self.esign == 0. {
            self.episode = k;
            self.esign = g.signum();
            self.anchor = if k > 0 { self.c[k - 1] } else { c };
            self.peak = g.abs();
            self.peakstep = k;
            self.half_time = 0.;
            self.lag_time = 0.;
            self.maxgain = 0.;
        }
        if self.esign != 0. {
            if g.abs() > self.peak {
                self.peak = g.abs();
                self.peakstep = k;
                self.half_time = 0.;
            }
            if g.abs() < self.peak * 0.5 && self.half_time == 0. {
                self.half_time = (k - self.peakstep) as f64 * 0.5;
            }
            let gain = (c - self.anchor) * self.esign;
            self.maxgain = self.maxgain.max(gain);
            if gain > self.peak * 0.3 && self.lag_time == 0. {
                self.lag_time = (k - self.episode) as f64 * 0.5;
            }
        }
        let mut s = if g >= 0. { 1. } else { -1. };
        if g.abs() < 0.025 {
            s = if c >= self.c.get(k.saturating_sub(16)).copied().unwrap_or(c) {
                1.
            } else {
                -1.
            };
        }
        self.c.push(c);
        self.g.push(g);
        self.low = self.low.min(c);
        self.high = self.high.max(c);
        let mut x = [0f32; 64];
        let mut j = 0;
        macro_rules! push {
            ($v:expr) => {{
                x[j] = $v as f32;
                j += 1;
            }};
        }
        push!(g * s);
        push!(period);
        push!(cap);
        push!((g / period).clamp(-cap, cap) * s);
        for lag in [1, 2, 4, 8, 16, 32, 64, 128] {
            let a = k.saturating_sub(lag);
            push!((c - self.c[a]) * s);
            push!((g - self.g[a]) * s);
        }
        for win in [4, 8, 16, 32, 64, 128] {
            let a = (k + 1).saturating_sub(win);
            let mut mc = 0.;
            let mut mg = 0.;
            let mut lo = c;
            let mut hi = c;
            for q in a..=k {
                mc += self.c[q];
                mg += self.g[q];
                lo = lo.min(self.c[q]);
                hi = hi.max(self.c[q]);
            }
            mc /= (k - a + 1) as f64;
            mg /= (k - a + 1) as f64;
            push!((c - mc) * s);
            push!((g - mg) * s);
            push!((c - lo) * s);
            push!((c - hi) * s);
        }
        push!(if self.esign != 0. {
            (k - self.episode) as f64 * 0.5
        } else {
            -1.
        });
        push!(if self.esign != 0. {
            (c - self.anchor) * s
        } else {
            0.
        });
        push!(if self.esign != 0. { self.peak } else { 0. });
        push!((k - self.lastchange) as f64 * 0.5);
        push!(self.lastdc * s);
        push!(self.lastdg * s);
        push!(self.secondc * s);
        push!(self.secondg * s);
        push!(self.noise);
        push!(k as f64 * 0.5);
        if k > 0 {
            push!((c - self.c[0]) * s);
            push!(self.high - self.low);
        }
        let _ = j;
        x[56] = self.ninf as f32;
        x[57] = self.ncrowd as f32;
        x[58] = if self.halfcount > 0. {
            self.halfsum / self.halfcount
        } else {
            -1.
        } as f32;
        x[59] = if self.lagcount > 0. {
            self.lagsum / self.lagcount
        } else {
            -1.
        } as f32;
        x[60] = if self.esign != 0. { self.maxgain } else { 0. } as f32;
        x[61] = if self.esign != 0. {
            (k - self.peakstep) as f64 * 0.5
        } else {
            -1.
        } as f32;
        x[62] = if self.esign != 0. {
            self.half_time
        } else {
            -1.
        } as f32;
        x[63] = if self.esign != 0. { self.lag_time } else { -1. } as f32;
        for v in &mut x {
            *v = ((*v as f64 * 1e6).round() * 1e-6) as f32;
        }
        (x, s)
    }
}
