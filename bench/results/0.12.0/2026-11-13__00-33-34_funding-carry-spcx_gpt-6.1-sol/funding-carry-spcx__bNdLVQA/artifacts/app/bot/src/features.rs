#[derive(Default)]
struct Signals {
    ix: Vec<f64>,
    bk: Vec<f64>,
    gaps: Vec<f64>,
    ei: [f64; 6],
    eb: [f64; 6],
    es: f64,
    op: f64,
    base: f64,
    age: f64,
    des: f64,
    dop: f64,
    dbase: f64,
    logodds: f64,
    bigage: f64,
    bigsize: f64,
    bigbase: f64,
    numup: f64,
    numdown: f64,
    lastchange: usize,
}
impl Signals {
    fn update(&mut self, ix: f64, bk: f64, g: f64, per: f64, cap: f64, inter: f64) -> [f64; 55] {
        let t = self.ix.len();
        if t == 0 {
            self.ei = [ix; 6];
            self.eb = [bk; 6];
            self.base = ix;
            self.dbase = ix;
            self.bigbase = ix;
            self.bigage = 100.;
        }
        let d = if g.abs() < 0.0002 {
            if ix >= self.ei[3] { 1. } else { -1. }
        } else {
            g.signum()
        };
        if self.es == 0. && g.abs() > 0.0015 {
            self.es = g.signum();
            self.op = g.abs();
            self.base = if t > 0 { self.ix[t - 1] } else { ix };
            self.age = 0.;
        }
        if self.es != 0. {
            self.age += 0.5;
            self.op = self.op.max(g * self.es);
            if g * self.es < 0.00025 {
                self.es = 0.;
            }
        }
        self.ix.push(ix);
        self.bk.push(bk);
        self.gaps.push(g);
        let mut x = [0.; 55];
        let mut z = 0;
        x[z] = g * d * 1e4;
        z += 1;
        x[z] = g.abs().min(cap) * 1e4 / per;
        z += 1;
        x[z] = per;
        z += 1;
        x[z] = cap * 1e4;
        z += 1;
        x[z] = inter;
        z += 1;
        for (j, tau) in [1., 2., 4., 8., 16., 32.].iter().enumerate() {
            let a = 1. - f64::exp(-0.5 / tau);
            self.ei[j] += a * (ix - self.ei[j]);
            self.eb[j] += a * (bk - self.eb[j]);
            x[z] = (ix / self.ei[j] - 1.) * d * 1e4;
            z += 1;
            x[z] = (bk / self.eb[j] - 1.) * d * 1e4;
            z += 1;
        }
        for lag in [2, 4, 8, 16, 32, 64] {
            let k = t.saturating_sub(lag);
            x[z] = (ix / self.ix[k] - 1.) * d * 1e4;
            z += 1;
            x[z] = (bk / self.bk[k] - 1.) * d * 1e4;
            z += 1;
            x[z] = self.gaps[k] * d * 1e4;
            z += 1;
        }
        x[z] = if self.es != 0. { self.age } else { 0. };
        z += 1;
        x[z] = if self.es != 0. { self.op * 1e4 } else { 0. };
        z += 1;
        x[z] = if self.es != 0. {
            (ix / self.base - 1.) * d * 1e4
        } else {
            0.
        };
        z += 1;
        x[z] = self.es * d;
        z += 1;
        x[z] = if self.es != 0. {
            g.abs() / self.op.max(0.0001)
        } else {
            0.
        };
        z += 1;
        let mut v = 0.;
        let mut c: f64 = 0.;
        for j in (t.saturating_sub(120).max(1))..=t {
            let r = self.ix[j] / self.ix[j - 1] - 1.;
            if r != 0. {
                v += (r * r).min(0.003 * 0.003);
                c += 1.;
            }
        }
        x[z] = (v / c.max(1.)).sqrt() * 1e4;
        x[54] = d;
        if self.des == 0. && g.abs() > 0.0015 {
            self.des = g.signum();
            self.dop = g.abs();
            self.dbase = if t > 0 { self.ix[t - 1] } else { ix };
        }
        if self.des != 0. {
            self.dop = self.dop.max(g * self.des);
            if g * self.des < 0.00025 {
                let informed = (ix / self.dbase - 1.) * self.des > (self.dop * 0.6).max(0.0015);
                self.logodds = (self.logodds + if informed { 1.45 } else { -1.45 }).clamp(-5., 5.);
                self.des = 0.;
            }
        }
        if t > 0 && ix != self.ix[t - 1] {
            let delta = ix / self.ix[t - 1] - 1.;
            let scale = x[40].max(6.);
            if delta.abs() * 1e4 > (scale * 2.).max(12.) {
                self.bigage = 0.;
                self.bigsize = delta;
                self.bigbase = self.ix[t - 1];
            } else {
                self.bigage += 0.5;
            }
            self.numup = self.numup * 0.75 + if delta > 0. { 1. } else { 0. };
            self.numdown = self.numdown * 0.75 + if delta < 0. { 1. } else { 0. };
            self.lastchange = t;
        } else {
            self.bigage += 0.5;
        }
        x[41] = 1. / (1. + (-self.logodds).exp());
        x[42] = self.bigage;
        x[43] = self.bigsize * d * 1e4;
        x[44] = (ix / self.bigbase - 1.) * d * 1e4;
        x[45] = (self.numup - self.numdown) * d;
        x[46] = (t - self.lastchange) as f64 * 0.5;
        for (jj, look) in [12, 24, 48].iter().enumerate() {
            let lo = (t + 1).saturating_sub(*look);
            let nn = (t - lo + 1) as f64;
            let (mut sx, mut sy, mut sxx, mut sxy) = (0., 0., 0., 0.);
            for j in lo..=t {
                let xx = (j - lo) as f64;
                sx += xx;
                sy += self.ix[j];
                sxx += xx * xx;
                sxy += xx * self.ix[j];
            }
            let slope = (nn * sxy - sx * sy) / (nn * sxx - sx * sx).max(1e-8);
            let intercept = (sy - slope * sx) / nn;
            x[47 + jj * 2] = slope / ix * d * 1e4 * 2.;
            x[48 + jj * 2] = (ix - (intercept + slope * (nn - 1.))) / ix * d * 1e4;
        }
        x[53] = (t as f64 * 0.5).min(120.);
        x
    }
}
