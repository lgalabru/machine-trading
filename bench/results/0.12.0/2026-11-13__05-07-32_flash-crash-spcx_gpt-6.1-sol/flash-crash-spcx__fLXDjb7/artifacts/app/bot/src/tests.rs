use super::*;
use mtx_kit::{Intent, Position, step_offline};

fn initialized(price: f64) -> ShockBot {
    ShockBot {
        decimals: 1_000.0,
        asset_id: 41,
        equity_peak: 10_000.0,
        start_mark: price,
        initial: price,
        last: price,
        anchor: price,
        extreme: price,
        orient: 1.0,
        ages: [1_000; 3],
        move_age: 1_000,
        ..ShockBot::default()
    }
}

fn market(price: f64, position: f64, drawdown: f64) -> Market {
    Market {
        index: price,
        oracle: price,
        mark: price,
        cap: 70.0,
        max_drawdown: 1_000.0,
        drawdown,
        position: Position {
            size: position,
            entry_usd: price,
        },
        ..Market::default()
    }
}

fn target(intents: &[Intent]) -> f64 {
    intents
        .iter()
        .find_map(|i| match i {
            Intent::Target { position, .. } => Some(*position),
            _ => None,
        })
        .expect("an active turn must issue its risk-controlled target")
}

#[test]
fn loss_halt_is_permanent_and_retries_an_unfilled_exit() {
    let mut bot = initialized(143.0);
    let first = step_offline(&mut bot, &market(143.0, 70.0, 800.0)).unwrap();
    assert_eq!(target(&first), 0.0);
    assert!(bot.halted);
    // Recovering prices do not authorize new exposure. If the first exit
    // failed to fill, the next turn must try again.
    let mut recovered = market(145.0, 70.0, 0.0);
    recovered.k = 1;
    let second = step_offline(&mut bot, &recovered).unwrap();
    assert_eq!(target(&second), 0.0);
}

#[test]
fn final_turn_adds_no_execution_cost() {
    let mut bot = initialized(143.0);
    let mut last = market(160.0, -70.0, 800.0);
    last.last = true;
    assert!(step_offline(&mut bot, &last).unwrap().is_empty());
}

#[test]
fn an_adverse_jump_plus_execution_cost_stays_inside_the_loss_limit() {
    let mut traded = false;
    for price in [50.0, 100.0, 143.0, 190.0] {
        for shock in [-0.03, 0.03] {
            for drawdown in [0.0, 300.0, 600.0, 740.0] {
                let mut bot = initialized(price / (1.0 + shock));
                let orders = step_offline(&mut bot, &market(price, 0.0, drawdown)).unwrap();
                let exposure = target(&orders).abs();
                traded |= exposure > 1.0;
                assert!(exposure <= 70.0);
                let stressed_loss = drawdown + exposure * price * 0.045 + 80.0;
                assert!(stressed_loss <= 1_000.0 + 1e-7, "{stressed_loss}");
            }
        }
    }
    assert!(
        traded,
        "exercise nonzero positions, not only flat decisions"
    );
}
