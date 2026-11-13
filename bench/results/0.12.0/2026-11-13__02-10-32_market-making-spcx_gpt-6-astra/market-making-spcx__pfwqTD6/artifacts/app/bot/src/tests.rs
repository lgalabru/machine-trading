use super::*;

fn bot() -> Maker {
    Maker {
        tick: 0.01,
        lot: 0.001,
        prev_price: 143.05,
        start_price: 143.05,
        width: 0.0015,
        equity: 10_000.0,
        peak: 10_000.0,
        ..Maker::default()
    }
}

fn market(position: f64) -> Market {
    Market {
        k: 50,
        slot: 100,
        oracle: 143.05,
        mark: 143.05,
        best_bid: Some(142.90),
        best_ask: Some(143.20),
        position: mtx_kit::Position {
            size: position,
            entry_usd: 143.05,
        },
        cap: 20.0,
        max_drawdown: 250.0,
        slots_left: 400,
        ..Market::default()
    }
}

#[test]
fn invalid_oracle_only_cancels() {
    let mut b = bot();
    let mut m = market(0.0);
    m.oracle = f64::NAN;
    let intents = step_offline(&mut b, &m).unwrap();
    assert_eq!(intents.len(), 1);
    assert!(matches!(intents[0], Intent::CancelAll));
}

#[test]
fn every_resting_side_fits_internal_and_mandate_caps() {
    for pos in [-19.8, -19.7, -2.0, 0.0, 2.0, 19.7, 19.8] {
        let mut b = bot();
        for intent in step_offline(&mut b, &market(pos)).unwrap() {
            if let Intent::Limit { side, price, size } = intent {
                assert!(price.is_finite() && price > 0.0 && size > 0.0);
                match side {
                    Side::Bid => {
                        assert!(pos + size <= 19.8 + 1e-9);
                        assert!(price <= 143.05 * (1.0 - 0.00015));
                    }
                    Side::Ask => {
                        assert!(pos - size >= -19.8 - 1e-9);
                        assert!(price >= 143.05 * (1.0 + 0.00015));
                    }
                }
            }
        }
    }
}

#[test]
fn drawdown_stop_is_latched_and_requests_flat_inventory() {
    let mut b = bot();
    b.equity = 9869.0;
    let intents = step_offline(&mut b, &market(7.0)).unwrap();
    assert!(b.stopped);
    assert!(
        intents
            .iter()
            .any(|i| matches!(i, Intent::Target { position, .. } if *position == 0.0))
    );
    assert!(!intents.iter().any(|i| matches!(i, Intent::Limit { .. })));
    b.equity = 10_001.0;
    let intents = step_offline(&mut b, &market(2.0)).unwrap();
    assert!(b.stopped);
    assert!(!intents.iter().any(|i| matches!(i, Intent::Limit { .. })));
}

#[test]
fn drawdown_reserve_reduces_exposure_before_hard_stop() {
    let mut b = bot();
    b.equity = 9890.0;
    let intents = step_offline(&mut b, &market(19.8)).unwrap();
    let reserve_cap = (225.0 - 110.0) / (143.05 * 0.05);
    assert!(intents.iter().any(
        |i| matches!(i, Intent::Target { position, .. } if (*position-reserve_cap).abs() < 1e-9)
    ));
    assert!(!b.stopped);
}

fn burst_bot() -> Maker {
    let mut b = bot();
    b.flow_count = 20.0;
    for slot in 94..=100 {
        b.recent.push_back((slot, 0.6));
    }
    b
}

#[test]
fn sustained_flow_can_lean_with_burst_within_position_cap() {
    let mut b = burst_bot();
    let intents = step_offline(&mut b, &market(-2.0)).unwrap();
    assert!(
        intents
            .iter()
            .any(|i| matches!(i, Intent::Target { position, .. } if *position == 19.8))
    );
}

#[test]
fn late_session_and_jump_cooldown_suppress_new_burst_entries() {
    let mut b = burst_bot();
    let mut m = market(0.0);
    m.slots_left = 50;
    let intents = step_offline(&mut b, &m).unwrap();
    assert!(!intents.iter().any(|i| matches!(i, Intent::Target { .. })));

    let mut b = burst_bot();
    b.last_jump = 90;
    let intents = step_offline(&mut b, &market(0.0)).unwrap();
    assert!(!intents.iter().any(|i| matches!(i, Intent::Target { .. })));
}

#[test]
fn expired_burst_exits_speculative_inventory() {
    let mut b = bot();
    b.was_active = true;
    b.burst_until = 99;
    let intents = step_offline(&mut b, &market(19.8)).unwrap();
    assert!(
        intents
            .iter()
            .any(|i| matches!(i, Intent::Target { position, .. } if *position == 0.0))
    );
}
