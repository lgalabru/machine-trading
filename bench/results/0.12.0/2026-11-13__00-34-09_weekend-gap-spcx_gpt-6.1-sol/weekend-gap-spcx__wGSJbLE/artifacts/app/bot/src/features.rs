use std::collections::HashMap;
const HALVES: [f64; 3] = [60., 120., 240.];
#[derive(Default, Clone)]
pub struct Trade {
    pub t: u64,
    pub z: f64,
    pub limit: f64,
}
#[derive(Default)]
pub struct State {
    pub reopen: u64,
    pub accounts: HashMap<String, Vec<Trade>>,
    prices: Vec<(u64, f64)>,
    obs: Vec<(f64, f64)>,
    pub impact: f64,
    impact_n: f64,
}
impl State {
    pub fn new(reopen: u64) -> Self {
        Self {
            reopen,
            impact: 0.0014,
            impact_n: 4.,
            ..Self::default()
        }
    }
    pub fn update(&mut self, t: u64, y: f64, trades: Vec<(String, f64, f64)>) {
        let prev = self.prices.last().map(|x| x.1).unwrap_or(0.);
        let z = trades.iter().map(|x| x.1).sum::<f64>();
        self.obs.push((y - prev, z));
        if self.obs.len() >= 3 {
            let r = &self.obs[self.obs.len() - 3..];
            let z = r.iter().map(|x| x.1).sum::<f64>();
            let dy = r.iter().map(|x| x.0).sum::<f64>();
            if z.abs() > 0.4 && dy / z > 0.0004 && dy / z < 0.003 {
                let w = z.abs().min(2.) / 3.;
                self.impact = (self.impact * self.impact_n + dy / z * w) / (self.impact_n + w);
                self.impact_n += w;
            }
        }
        self.prices.push((t, y));
        for (key, z, limit) in trades {
            self.accounts
                .entry(key)
                .or_default()
                .push(Trade { t, z, limit });
        }
    }
    pub fn features(&self, t: u64, y: f64, spread: f64) -> (Vec<f32>, f64) {
        let mut acc: Vec<_> = self.accounts.iter().collect();
        acc.sort_by(|(_, a), (_, b)| {
            let na = a.iter().map(|x| x.z).sum::<f64>().abs();
            let nb = b.iter().map(|x| x.z).sum::<f64>().abs();
            nb.total_cmp(&na)
                .then(b.len().cmp(&a.len()))
                .then(a[0].t.cmp(&b[0].t))
                .then_with(|| {
                    for (x, y) in a.iter().zip(b.iter()) {
                        let cmp =
                            x.t.cmp(&y.t)
                                .then(x.z.total_cmp(&y.z))
                                .then(x.limit.total_cmp(&y.limit));
                        if !cmp.is_eq() {
                            return cmp;
                        }
                    }
                    std::cmp::Ordering::Equal
                })
        });
        let orient = if acc
            .first()
            .map(|(_, a)| a.iter().map(|x| x.z).sum::<f64>())
            .unwrap_or(0.)
            >= 0.
        {
            1.
        } else {
            -1.
        };
        let mut out = vec![
            t as f64 / self.reopen as f64,
            (self.reopen as f64 - t as f64) / 100.,
            spread,
            y * orient * 100.,
            self.impact * 100.,
        ];
        for lag in [6, 12, 24, 48, 96, 192] {
            let j = self
                .prices
                .partition_point(|x| x.0 as i64 <= t as i64 - lag);
            let prev = self.prices[j.saturating_sub(1)].1;
            out.push((y - prev) * orient * 100.);
        }
        let all: Vec<_> = acc.iter().flat_map(|(_, a)| a.iter()).collect();
        let decayed = |a: &[&Trade], h: f64| {
            a.iter()
                .map(|x| x.z * (-((t - x.t) as f64) * std::f64::consts::LN_2 / h).exp())
                .sum::<f64>()
        };
        let totaldec: Vec<f64> = HALVES.iter().map(|&h| decayed(&all, h)).collect();
        for &dec in &totaldec {
            out.extend([dec * orient, (y - self.impact * dec) * orient * 100.]);
        }
        for window in [20, 60, 120] {
            let tr: Vec<_> = all.iter().filter(|x| t - x.t <= window).collect();
            out.extend([
                tr.iter().map(|x| x.z).sum::<f64>() * orient,
                tr.iter().map(|x| x.z.abs()).sum(),
                tr.len() as f64,
            ]);
        }
        for k in 0..4 {
            let Some((_, a)) = acc.get(k) else {
                out.extend([0.; 27]);
                continue;
            };
            let n = a.len();
            let net = a.iter().map(|x| x.z).sum::<f64>();
            let gross = a.iter().map(|x| x.z.abs()).sum::<f64>();
            let gaps: Vec<_> = a.windows(2).map(|w| (w[1].t - w[0].t) as f64).collect();
            let gap = if gaps.is_empty() {
                30.
            } else {
                gaps.iter().sum::<f64>() / gaps.len() as f64
            };
            let cv = if gaps.is_empty() {
                1.
            } else {
                (gaps.iter().map(|x| (x - gap).powi(2)).sum::<f64>() / gaps.len() as f64).sqrt()
                    / gap.max(1.)
            };
            out.extend([
                n as f64,
                net * orient,
                gross,
                net / gross.max(0.01) * orient,
                (t - a[0].t) as f64 / 100.,
                (t - a[n - 1].t) as f64 / 30.,
                gap / 30.,
                cv,
                gross / n as f64,
                a.iter().map(|x| x.limit).sum::<f64>() / n as f64,
                a.iter().map(|x| x.limit).fold(f64::INFINITY, f64::min),
                a.iter().map(|x| x.limit).fold(f64::NEG_INFINITY, f64::max),
            ]);
            for window in [20, 60, 120] {
                let tr: Vec<_> = a.iter().filter(|x| t - x.t <= window).collect();
                out.extend([
                    tr.iter().map(|x| x.z).sum::<f64>() * orient,
                    tr.len() as f64,
                ]);
            }
            let ar: Vec<_> = a.iter().collect();
            for (i, &h) in HALVES.iter().enumerate() {
                let dec = decayed(&ar, h);
                out.extend([
                    dec * orient,
                    (net - dec) * orient,
                    (y - self.impact * (totaldec[i] + net - dec)) * orient * 100.,
                ]);
            }
        }
        // Quiet book relaxation, excluding the bars around taker orders.
        for window in [30usize, 60, 120] {
            let mut vals = Vec::new();
            let begin = self.obs.len().saturating_sub(window / 2).max(3);
            for k in begin..self.obs.len() {
                if self.obs[k - 3..=k].iter().all(|x| x.1.abs() < 1e-9)
                    && self.obs[k].0.abs() < 0.0008
                {
                    vals.push(self.obs[k].0);
                }
            }
            vals.sort_by(f64::total_cmp);
            let n = vals.len();
            let trim = if n > 0 { &vals[n / 5..n - n / 5] } else { &[] };
            out.extend([
                n as f64 / (window as f64 / 2.),
                vals.iter().sum::<f64>() / n.max(1) as f64 * 5000. * orient,
                trim.iter().sum::<f64>() / trim.len().max(1) as f64 * 5000. * orient,
                vals.get(n / 2).copied().unwrap_or(0.) * 5000. * orient,
            ]);
        }
        for k in 0..4 {
            let Some((_, a)) = acc.get(k) else {
                out.extend([0.; 4]);
                continue;
            };
            let n = a.len();
            let gap = if n > 1 {
                (a[n - 1].t - a[0].t) as f64 / (n - 1) as f64
            } else {
                30.
            };
            let lastgap = if n >= 4 {
                (a[n - 1].t - a[n - 4].t) as f64 / 3.
            } else {
                gap
            };
            out.extend([
                (t - a[n - 1].t) as f64 / gap.max(1.),
                lastgap / gap.max(1.),
                a.iter().map(|x| x.z).sum::<f64>() * self.impact * orient * 100.,
                a[n.saturating_sub(3)..].iter().map(|x| x.z).sum::<f64>() * orient,
            ]);
        }
        (out.into_iter().map(|x| x as f32).collect(), orient)
    }
}
