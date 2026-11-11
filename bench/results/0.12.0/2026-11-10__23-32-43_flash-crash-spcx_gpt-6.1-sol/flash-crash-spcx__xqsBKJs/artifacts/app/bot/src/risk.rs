//! Exposure is bounded by both the mandate and the cash left for the next jump.
const JUMP_RESERVE: f64 = 0.045;
const CASH_RESERVE_FRACTION: f64 = 0.14;
pub fn position_limit(price: f64, drawdown: f64, limit: f64, cap: f64) -> f64 {
    if !price.is_finite() || price <= 0. || !drawdown.is_finite() || limit <= 0. || cap <= 0. {
        return 0.;
    }
    ((limit - drawdown - limit * CASH_RESERVE_FRACTION) / (price * JUMP_RESERVE)).clamp(0., cap)
}
pub fn should_halt(drawdown: f64, limit: f64) -> bool {
    drawdown > limit * 0.65
}

/// Even a worst-price emergency fill must leave cash inside the loss limit.
pub fn exit_slippage_bps(price: f64, position: f64, drawdown: f64, limit: f64) -> f64 {
    let notional = price * position.abs();
    if !notional.is_finite() || notional <= 0. {
        return 100.;
    }
    (((limit - drawdown - 50.).max(0.) / notional) * 10_000. - 5.).clamp(1., 500.)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn repeated_maximum_adverse_jumps_leave_the_account_inside_its_limit() {
        for direction in [-1., 1.] {
            for starting_dd in (0..650).step_by(13) {
                let mut price = 143.;
                let mut equity = 10_000. - starting_dd as f64;
                let peak = 10_000.;
                let mut position =
                    -direction * position_limit(price, starting_dd as f64, 1000., 70.).trunc();
                let mut halted = false;
                equity -= position.abs() * price * 0.0015;
                for _ in 0..20 {
                    let next = price * (1. + direction * 0.038);
                    equity += position * (next - price);
                    price = next;
                    let dd = peak - equity;
                    assert!(
                        dd < 1000.,
                        "drawdown {dd}, initial {starting_dd}, direction {direction}"
                    );
                    halted |= should_halt(dd, 1000.);
                    let target = if halted {
                        0.
                    } else {
                        -direction * position_limit(price, dd, 1000., 70.).trunc()
                    };
                    assert!(target.abs() <= 70.);
                    equity -= (target - position).abs() * price * 0.0015;
                    position = target;
                    assert!(peak - equity < 1000.);
                }
                assert_eq!(position, 0.);
            }
        }
    }
    #[test]
    fn emergency_fill_prices_respect_remaining_cash() {
        for price in [100., 143., 200.] {
            for position in 1..=70 {
                for dd in (650..950).step_by(10) {
                    let dd = dd as f64;
                    let slip = exit_slippage_bps(price, position as f64, dd, 1000.);
                    let cost = price * position as f64 * (slip / 10_000. + 0.0005);
                    assert!(dd + cost < 1000.);
                }
            }
        }
    }
    #[test]
    fn invalid_prices_and_exhausted_risk_have_zero_room() {
        assert_eq!(position_limit(f64::NAN, 0., 1000., 70.), 0.);
        assert_eq!(position_limit(143., 900., 1000., 70.), 0.);
        assert_eq!(position_limit(0., 0., 1000., 70.), 0.);
    }
}
