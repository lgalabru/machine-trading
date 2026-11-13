use super::*;

fn strategy() -> Maker {
    Maker {
        tick: 0.01,
        lots: 1000.0,
        prev_price: 143.0,
        start_slot: 100,
        prev_slot: 198,
        peak_equity: 10000.0,
        ..Maker::default()
    }
}

fn market(position: f64, drawdown: f64) -> Market {
    Market {
        k: 50,
        slot: 200,
        oracle: 143.0,
        mark: 143.0,
        best_bid: Some(142.8),
        best_ask: Some(143.2),
        position: Position {
            size: position,
            entry_usd: 143.0,
        },
        cap: 20.0,
        max_drawdown: 250.0,
        drawdown,
        slots_left: 400,
        steps_left: 200,
        ..Market::default()
    }
}

fn exposure(intents: &[Intent], position: f64) -> (f64, f64) {
    let (mut low, mut high) = (position, position);
    for intent in intents {
        match intent {
            Intent::Market { side, size, limit } => {
                assert!(limit.is_finite() && *limit > 0.0);
                assert!(*size > 0.0 && *size <= 10.0);
                if *side == Side::Bid {
                    high += size;
                } else {
                    low -= size;
                }
            }
            Intent::Limit { side, size, price } => {
                assert!(price.is_finite() && *price > 0.0);
                assert!(*size > 0.0);
                if *side == Side::Bid {
                    high += size;
                } else {
                    low -= size;
                }
            }
            Intent::CancelAll => {}
            other => panic!("Unexpected order: {other:?}"),
        }
    }
    (low, high)
}

#[test]
fn invalid_oracle_cancels_without_new_exposure() {
    for oracle in [0.0, -1.0, f64::NAN, f64::INFINITY] {
        let mut m = market(4.0, 0.0);
        m.oracle = oracle;
        let orders = step_offline(&mut strategy(), &m).unwrap();
        assert_eq!(orders, vec![Intent::CancelAll]);
    }
}

#[test]
fn drawdown_stop_flattens_and_stays_latched() {
    for position in [-18.0, 18.0] {
        let mut bot = strategy();
        let orders = step_offline(&mut bot, &market(position, 146.0)).unwrap();
        let (low, high) = exposure(&orders, position);
        if position > 0.0 {
            assert!(low.abs() < 1e-9);
        } else {
            assert!(high.abs() < 1e-9);
        }
        assert!(orders.iter().all(|i| !matches!(i, Intent::Limit { .. })));
        // Even after the position is flat and the reported drawdown clears,
        // the strategy must not restart risk-taking during this session.
        let next = step_offline(&mut bot, &market(0.0, 0.0)).unwrap();
        assert_eq!(next, vec![Intent::CancelAll]);
    }
}

#[test]
fn reduced_risk_budget_forces_inventory_reduction() {
    let orders = step_offline(&mut strategy(), &market(18.0, 120.0)).unwrap();
    assert!(matches!(orders.first(), Some(Intent::CancelAll)));
    let (low, high) = exposure(&orders, 18.0);
    assert!(low < 15.0 && low > 0.0);
    assert_eq!(high, 18.0);
    assert!(orders.iter().all(|i| !matches!(i, Intent::Limit { .. })));
}

#[test]
fn all_fill_combinations_stay_inside_mandate() {
    for position in [-18.0, -5.0, 0.0, 5.0, 18.0] {
        for direction in [-1.0, 1.0] {
            let mut bot = strategy();
            bot.burst = direction;
            bot.burst_until = 240;
            bot.prev_pos = position;
            let orders = step_offline(&mut bot, &market(position, 0.0)).unwrap();
            assert!(matches!(orders.first(), Some(Intent::CancelAll)));
            let (low, high) = exposure(&orders, position);
            assert!(low >= -20.0 && high <= 20.0, "{position}: [{low}, {high}]");
        }
    }
}

#[test]
fn manipulated_mark_does_not_move_quotes() {
    let m = market(0.0, 0.0);
    let mut distorted = m.clone();
    distorted.mark = 9999.0;
    let normal = step_offline(&mut strategy(), &m).unwrap();
    let other = step_offline(&mut strategy(), &distorted).unwrap();
    assert_eq!(normal, other);
}
