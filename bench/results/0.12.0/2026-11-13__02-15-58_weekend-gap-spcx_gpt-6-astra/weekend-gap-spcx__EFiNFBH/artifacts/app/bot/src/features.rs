//! Causal, permutation-invariant flow features. Values are percentage points.
//! Each row contains price history, per-signer flow and timing, impact-model
//! residuals, and robust estimates of the persistent price component.
#[derive(Clone, Default)]
pub struct Actor {
    pub key: String,
    pub net: f64,
    pub volume: f64,
    pub count: f64,
    pub signed_count: f64,
    pub limits: f64,
    pub first: f64,
    pub last: f64,
    pub gaps: Vec<f64>,
    pub history: Vec<(f64, f64)>,
    pub decay: [f64; 3],
}
pub struct Features {
    pub actors: Vec<Actor>,
    pub prices: Vec<f64>,
    pub xy: [[f64; 9]; 3],
    pub xx: [[f64; 9]; 3],
    pub yy: f64,
}
impl Default for Features {
    fn default() -> Self {
        Self {
            actors: (0..8)
                .map(|_| Actor {
                    first: -1.0,
                    last: -1.0,
                    ..Actor::default()
                })
                .collect(),
            prices: vec![0.0],
            xy: [[0.; 9]; 3],
            xx: [[0.; 9]; 3],
            yy: 0.0,
        }
    }
}
impl Features {
    pub fn update(&mut self, t: f64, p: f64, flows: &[(usize, f64, f64)]) {
        let mut q = [0.0; 8];
        let mut v = [0.0; 8];
        let mut c = [0.0; 8];
        let mut lim = [0.0; 8];
        for &(a, size, limit) in flows {
            if a < 8 {
                q[a] += size;
                v[a] += size.abs();
                c[a] += 1.;
                lim[a] += limit;
            }
        }
        for a in 0..8 {
            let ac = &mut self.actors[a];
            ac.net += q[a];
            ac.volume += v[a];
            ac.count += c[a];
            ac.signed_count += if q[a] == 0. { 0. } else { q[a].signum() };
            ac.limits += lim[a];
            if c[a] > 0. {
                if ac.first < 0. {
                    ac.first = t;
                }
                if ac.last >= 0. {
                    ac.gaps.push(t - ac.last);
                }
                ac.last = t;
            }
            ac.history.push((ac.net, ac.volume));
            for (h, half) in [60., 120., 240.].iter().enumerate() {
                ac.decay[h] = ac.decay[h] * 2f64.powf(-2. / half) + q[a];
            }
        }
        self.prices.push(p);
        self.yy += p * p;
        for h in 0..3 {
            let total: f64 = self.actors.iter().map(|a| a.decay[h]).sum();
            for k in 0..9 {
                let b = if k == 0 {
                    total
                } else {
                    total + self.actors[k - 1].net - self.actors[k - 1].decay[h]
                };
                self.xy[h][k] += b * p;
                self.xx[h][k] += b * b;
            }
        }
    }
    fn fit(&self, k: usize, h: usize, p: f64, sign: f64) -> [f64; 3] {
        let slope = (self.xy[h][k] + 0.14 * 15.) / (self.xx[h][k] + 15.);
        let err = ((self.yy - 2. * slope * self.xy[h][k] + slope * slope * self.xx[h][k])
            / self.prices.len() as f64)
            .max(0.)
            .sqrt();
        let total: f64 = self.actors.iter().map(|a| a.decay[h]).sum();
        let b = if k == 0 {
            total
        } else {
            total + self.actors[k - 1].net - self.actors[k - 1].decay[h]
        };
        [slope, err, sign * (p - slope * b)]
    }
    pub fn vector(&self, t: f64, reopen: f64) -> (Vec<f64>, f64) {
        let i = self.prices.len() - 1;
        let p = self.prices[i];
        let mut rank: Vec<usize> = (0..8).collect();
        rank.sort_by(|&a, &b| {
            let a = &self.actors[a];
            let b = &self.actors[b];
            let sa = a.signed_count.abs() / (a.count + 2.).sqrt();
            let sb = b.signed_count.abs() / (b.count + 2.).sqrt();
            sb.total_cmp(&sa).then(b.volume.total_cmp(&a.volume))
        });
        let sign = if self.actors[rank[0]].net >= 0. {
            1.
        } else {
            -1.
        };
        let mut f = vec![t, reopen - t, t / reopen, sign * p];
        for lag in [8, 16, 32, 64, 128] {
            f.push(sign * (p - self.prices[i.saturating_sub(lag / 2)]));
        }
        for lag in [32, 64, 128] {
            let vals = &self.prices[i.saturating_sub(lag / 2)..];
            let max = vals
                .iter()
                .map(|x| sign * x)
                .fold(f64::NEG_INFINITY, f64::max);
            let min = vals.iter().map(|x| sign * x).fold(f64::INFINITY, f64::min);
            f.extend([max - sign * p, sign * p - min]);
        }
        f.extend([
            sign * self.actors.iter().map(|a| a.net).sum::<f64>(),
            self.actors.iter().map(|a| a.volume).sum(),
            self.actors.iter().map(|a| a.count).sum(),
        ]);
        // Robust estimates of the non-decaying component from quiet intervals.
        for window in [64, 128, 10000] {
            let mut vals: [Vec<f64>; 3] = [vec![], vec![], vec![]];
            for k in 4.max(i.saturating_sub(window / 2))..=i {
                let vol: f64 = self.actors.iter().map(|a| a.history[k - 1].1).sum();
                let oldvol: f64 = if k == 4 {
                    0.
                } else {
                    self.actors.iter().map(|a| a.history[k - 5].1).sum()
                };
                let delta = self.prices[k] - self.prices[k - 3];
                if vol == oldvol && delta.abs() < 0.055 {
                    let level = (self.prices[k] + self.prices[k - 3]) * 0.5;
                    for (h, half) in [60., 120., 240.].iter().enumerate() {
                        vals[h].push(level + delta / 6. * half / 2f64.ln());
                    }
                }
            }
            let n = vals[0].len();
            f.push(n as f64);
            if n >= 3 {
                for v in &mut vals {
                    v.sort_by(f64::total_cmp);
                    let quant = |q: f64| {
                        let x = (n - 1) as f64 * q;
                        let k = x.floor() as usize;
                        v[k] * (1. - (x - k as f64)) + v[(k + 1).min(n - 1)] * (x - k as f64)
                    };
                    f.extend([sign * quant(0.5), quant(0.8) - quant(0.2)]);
                }
            } else {
                f.extend([0.; 6]);
            }
        }
        for h in 0..3 {
            f.extend(self.fit(0, h, p, sign));
        }
        for &a in &rank[..8] {
            let ac = &self.actors[a];
            f.extend([
                sign * ac.net,
                ac.volume,
                ac.count,
                sign * ac.signed_count,
                if ac.first >= 0. { t - ac.first } else { -1. },
                if ac.last >= 0. { t - ac.last } else { -1. },
                ac.limits / ac.count.max(1.),
            ]);
            let gs = &ac.gaps[ac.gaps.len().saturating_sub(6)..];
            let mean = gs.iter().sum::<f64>() / gs.len().max(1) as f64;
            let sd = (gs.iter().map(|g| (g - mean).powi(2)).sum::<f64>() / gs.len().max(1) as f64)
                .sqrt();
            f.extend([mean, sd]);
            for lag in [16, 32, 64, 128] {
                let j = i.saturating_sub(lag / 2);
                let (net, vol) = if j == 0 { (0., 0.) } else { ac.history[j - 1] };
                f.extend([sign * (ac.net - net), ac.volume - vol]);
            }
            for h in 0..3 {
                f.extend(self.fit(a + 1, h, p, sign));
            }
        }
        for x in &mut f {
            *x = (*x as f32) as f64;
        }
        (f, sign)
    }
}
/// Evaluate an embedded numeric tree ensemble. The binary header holds the
/// baseline and root offsets; each 24-byte node holds a value/threshold,
/// child offsets, and feature index (65535 denotes a leaf).
pub fn predict(data: &[u8], f: &[f64]) -> f64 {
    fn u32at(d: &[u8], i: usize) -> usize {
        u32::from_le_bytes(d[i..i + 4].try_into().unwrap()) as usize
    }
    fn f64at(d: &[u8], i: usize) -> f64 {
        f64::from_le_bytes(d[i..i + 8].try_into().unwrap())
    }
    let mut v = f64at(data, 0);
    let count = u32at(data, 8);
    let base = 12 + 4 * count;
    for tree in 0..count {
        let mut n = u32at(data, 12 + 4 * tree);
        loop {
            let off = base + 24 * n;
            let feature = u16::from_le_bytes(data[off + 16..off + 18].try_into().unwrap()) as usize;
            let x = f64at(data, off);
            if feature == 65535 {
                v += x;
                break;
            }
            n = u32at(data, off + if f[feature] <= x { 8 } else { 12 });
        }
    }
    v
}
