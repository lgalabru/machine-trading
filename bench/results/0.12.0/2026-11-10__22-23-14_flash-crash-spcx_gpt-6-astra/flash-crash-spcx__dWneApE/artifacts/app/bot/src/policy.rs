//! Receding-horizon inventory planning over the forecast price curve.
//! Compare short, flat and long inventory, paying a cost for every change.
//! A small inventory penalty suppresses marginal trades. Re-plan each turn;
//! the risk guard subsequently limits exposure using actual account equity.
pub fn target(forecast: [f64; 4], position: f64, cap: f64) -> f64 {
    let q = [
        0.0,
        forecast[0],
        forecast[1],
        forecast[2] * 0.5,
        forecast[3] * 0.5,
    ];
    // Forecasts are percentage returns: 0.09 is nine basis points.
    let cost = 0.09;
    let mut value = [0.0; 3];
    for k in (1..=3).rev() {
        let mut next = [f64::NEG_INFINITY; 3];
        for previous in 0..3 {
            for current in 0..3 {
                let score = (current as f64 - 1.0) * (q[k + 1] - q[k]) + value[current]
                    - (current as f64 - previous as f64).abs() * cost;
                next[previous] = next[previous].max(score);
            }
        }
        value = next;
    }
    let mut best = f64::NEG_INFINITY;
    let mut target = 0.0;
    for current in 0..3 {
        let sign = current as f64 - 1.0;
        let score = sign * q[1] + value[current]
            - (sign - position / cap).abs() * cost
            - if current != 1 { 0.01 } else { 0.0 };
        if score > best {
            best = score;
            target = sign * cap;
        }
    }
    target
}

#[cfg(test)]
mod tests {
    #[test]
    fn flat_and_weak_forecasts_do_not_open_positions() {
        assert_eq!(super::target([0.0; 4], 0.0, 70.0), 0.0);
        assert_eq!(super::target([0.02; 4], 0.0, 70.0), 0.0);
    }
    #[test]
    fn strong_forecasts_are_symmetric_and_capped() {
        assert_eq!(super::target([1.0, 2.0, 3.0, 0.0], 0.0, 70.0), 70.0);
        assert_eq!(super::target([-1.0, -2.0, -3.0, 0.0], 70.0, 70.0), -70.0);
        assert_eq!(super::target([1.0, 2.0, 3.0, 0.0], 0.0, 20.0), 20.0);
    }
}
