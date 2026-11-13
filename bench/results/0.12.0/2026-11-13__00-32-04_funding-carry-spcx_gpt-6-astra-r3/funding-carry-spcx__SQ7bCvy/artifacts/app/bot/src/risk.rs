//! Position sizing reserves cash for an adverse price jump and execution costs.

pub fn position_cap(price: f64, drawdown: f64, mandate_cap: f64, loss_limit: f64) -> f64 {
    if !price.is_finite() || price <= 0.0 || !drawdown.is_finite() || loss_limit <= 0.0 {
        return 0.0;
    }
    ((0.9 * loss_limit - drawdown).max(0.0) / (price * 0.06))
        .min(mandate_cap.min(70.0).max(0.0))
}

#[cfg(test)]
mod tests {
    use super::position_cap;

    #[test]
    fn adverse_jump_stays_inside_reserved_loss_budget() {
        // Include prices well outside the training paths and both position signs.
        for price in [60.0, 100.0, 143.11, 200.0, 500.0] {
            for dd in (0..=900).step_by(5) {
                let cap = position_cap(price, dd as f64, 70.0, 1000.0);
                for direction in [-1.0_f64, 1.0] {
                    let position = direction * cap;
                    let shock_loss = position.abs() * price * 0.06;
                    let execution_and_funding_reserve = position.abs() * 1.10;
                    assert!(dd as f64 + shock_loss + execution_and_funding_reserve < 1000.0);
                    assert!(position.abs() <= 70.0);
                }
            }
        }
    }

    #[test]
    fn respects_smaller_mandates_and_invalid_price() {
        assert_eq!(position_cap(143.0, 0.0, 20.0, 1000.0), 20.0);
        assert_eq!(position_cap(143.0, 901.0, 70.0, 1000.0), 0.0);
        assert_eq!(position_cap(f64::NAN, 0.0, 70.0, 1000.0), 0.0);
        assert_eq!(position_cap(0.0, 0.0, 70.0, 1000.0), 0.0);
    }
}
