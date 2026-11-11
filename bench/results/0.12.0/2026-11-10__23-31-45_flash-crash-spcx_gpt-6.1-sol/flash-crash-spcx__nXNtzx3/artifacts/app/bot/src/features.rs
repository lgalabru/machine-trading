//! Causal features: only observed oracle prices and the session clock.
pub const FEATURE_COUNT: usize = 144;
pub fn extract(x: &[f64], remaining: u64) -> (Vec<f64>, f64) {
    let i = x.len() - 1;
    let p = x[i];
    let delta: Vec<f64> = (0..=i)
        .map(|j| if j == 0 { 0.0 } else { x[j] - x[j - 1] })
        .collect();
    let mut direction = 1.0;
    for &d in &delta {
        if d.abs() >= 30.0 {
            direction = d.signum();
        }
    }
    let mut f = Vec::with_capacity(FEATURE_COUNT);
    f.extend([i as f64, remaining as f64, (p - x[0]) * direction]);
    for lag in [1, 2, 3, 4, 6, 8, 12, 16, 24, 32, 48, 64, 96, 128, 192, 256] {
        f.push((p - x[i.saturating_sub(lag)]) * direction);
    }
    for w in [4, 8, 16, 32, 64, 128, 256, 512] {
        let a = (i + 1).saturating_sub(w);
        let mut hi = x[a];
        let mut lo = x[a];
        let mut ih = a;
        let mut il = a;
        let mut sum = 0.0;
        let mut tv = 0.0;
        for j in a..=i {
            if x[j] > hi {
                hi = x[j];
                ih = j;
            }
            if x[j] < lo {
                lo = x[j];
                il = j;
            }
            sum += x[j];
            if j > a {
                tv += (x[j] - x[j - 1]).abs();
            }
        }
        f.extend([
            (hi - p) * direction,
            (lo - p) * direction,
            (sum / (i + 1 - a) as f64 - p) * direction,
            tv,
            (i - if direction > 0.0 { ih } else { il }) as f64,
            (i - if direction > 0.0 { il } else { ih }) as f64,
        ]);
    }
    for threshold in [15.0, 30.0, 60.0, 120.0, 250.0] {
        let ev: Vec<usize> = (0..=i)
            .rev()
            .filter(|&j| delta[j].abs() >= threshold)
            .take(4)
            .collect();
        for j in 0..4 {
            if let Some(&e) = ev.get(j) {
                f.extend([
                    delta[e] * direction,
                    (i - e) as f64,
                    (p - x[e.saturating_sub(1)]) * direction,
                ]);
            } else {
                f.extend([0.0, 512.0, 0.0]);
            }
        }
    }
    // Time-local, same-direction impulses are one evolving move.
    let mut clusters: Vec<(usize, usize, f64, f64)> = Vec::new();
    let mut cd = 0.0;
    let mut last: Option<usize> = None;
    for j in 0..=i {
        if delta[j].abs() >= 25.0 {
            let dr = delta[j].signum();
            if dr != cd || last.is_none_or(|l| j - l > 12) {
                clusters.push((j, j, x[j.saturating_sub(1)], x[j]));
                cd = dr;
            } else if let Some(c) = clusters.last_mut() {
                c.1 = j;
                c.3 = x[j];
            }
            last = Some(j);
        }
    }
    for j in 0..3 {
        if j < clusters.len() {
            let c = clusters[clusters.len() - 1 - j];
            f.extend([
                (c.3 - c.2) * direction,
                (i - c.0) as f64,
                (i - c.1) as f64,
                (p - c.3) * direction,
            ]);
        } else {
            f.extend([0.0, 512.0, 512.0, 0.0]);
        }
    }
    let mut big = 0;
    for j in 0..=i {
        if delta[j].abs() > delta[big].abs() {
            big = j;
        }
    }
    let hi = x[big..].iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let lo = x[big..].iter().copied().fold(f64::INFINITY, f64::min);
    f.extend([
        delta[big] * direction,
        (i - big) as f64,
        (p - x[big.saturating_sub(1)]) * direction,
        (hi - p) * direction,
        (lo - p) * direction,
    ]);
    debug_assert_eq!(f.len(), FEATURE_COUNT);
    (f, direction)
}
