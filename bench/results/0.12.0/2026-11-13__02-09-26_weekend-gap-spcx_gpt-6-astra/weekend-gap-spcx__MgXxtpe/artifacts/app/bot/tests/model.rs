#[path = "../src/model.rs"]
mod model;
use model::Filter;

fn replay(permanent: bool) -> Filter {
    let mut f = Filter::default();
    let mut level = 0.;
    for t in (6..=480).step_by(6) {
        if !permanent {
            level *= (-std::f64::consts::LN_2 * 6. / 150.).exp();
        }
        let mut flows = [0.; 8];
        let mut variance = [0.; 8];
        if (30..=210).contains(&t) && t % 12 == 6 {
            flows[0] = 1.;
            variance[0] = 1.;
            f.trade(0, t as f64 - 2., 1.);
            level += 0.0014;
        }
        f.update(t as f64, level, 500., flows, variance);
        assert!(f.probs.iter().all(|x| x.is_finite() && *x >= 0.));
        assert!((f.probs.iter().sum::<f64>() - 1.).abs() < 1e-9);
        assert!(f.future.is_finite() && f.opening.is_finite());
    }
    f
}
#[test]
fn recognizes_permanent_flow() {
    let f = replay(true);
    assert!(f.probs[0] > 0.9, "{:?}", f.probs);
    assert!(f.opening > 0.015);
}
#[test]
fn recognizes_transient_flow() {
    let f = replay(false);
    assert!(f.probs[0] < 0.01, "{:?}", f.probs);
    assert!(f.opening.abs() < 0.001);
}
#[test]
fn mixed_side_account_is_not_informed() {
    let mut f = Filter::default();
    f.trade(0, 10., 1.);
    f.trade(0, 12., -1.);
    f.update(18., 0., 500., [0.; 8], [0.; 8]);
    assert_eq!(f.probs[0], 0.);
}
