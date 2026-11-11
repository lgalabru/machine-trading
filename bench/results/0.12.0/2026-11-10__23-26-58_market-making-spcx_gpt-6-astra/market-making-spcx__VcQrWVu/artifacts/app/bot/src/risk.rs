//! Risk bounds use oracle equity and a stress move on the initial price.
//! Resting orders reserve room at both endpoints of a possibly partial IOC.

pub fn cap(mandate: f64, max_drawdown: f64, drawdown: f64, initial_price: f64) -> f64 {
    if !(initial_price > 0.0 && initial_price.is_finite()) {
        return 0.0;
    }
    19.5_f64
        .min((mandate - 0.01).max(0.0))
        .min(((max_drawdown - drawdown - 25.0) / (initial_price * 0.045)).max(0.0))
}
pub fn quote_room(position: f64, intended: f64, cap: f64, size: f64) -> (f64, f64) {
    (
        (cap - position.max(intended)).clamp(0.0, size),
        (cap + position.min(intended)).clamp(0.0, size),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn either_quote_and_any_partial_ioc_stay_within_the_cap() {
        for a in -40..=40 {
            for b in -40..=40 {
                let position = a as f64 / 2.0;
                let intended = b as f64 / 2.0;
                let (bid, ask) = quote_room(position, intended, 20.0, 2.0);
                for fill in 0..=10 {
                    let p = position + (intended - position) * fill as f64 / 10.0;
                    assert!(p + bid <= 20.0 + 1e-9);
                    assert!(p - ask >= -20.0 - 1e-9);
                }
            }
        }
    }
    #[test]
    fn stress_loss_keeps_a_fee_and_exit_buffer() {
        for dd in 0..=225 {
            let c = cap(20.0, 250.0, dd as f64, 143.11);
            assert!(c < 20.0);
            assert!(dd as f64 + c * 143.11 * 0.045 <= 225.0 + 1e-9);
        }
        assert_eq!(cap(20.0, 250.0, 251.0, 143.11), 0.0);
        assert_eq!(cap(20.0, 250.0, 0.0, f64::NAN), 0.0);
    }
}
