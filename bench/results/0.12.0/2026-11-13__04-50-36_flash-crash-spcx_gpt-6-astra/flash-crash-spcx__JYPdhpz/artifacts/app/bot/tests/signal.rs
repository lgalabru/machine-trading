#[path = "../src/signal.rs"]
mod signal;
mod fixtures;

#[test]
fn features_match_the_training_pipeline() {
    let mut signal=signal::Signal::default();
    for (k,&p) in fixtures::PRICES.iter().enumerate() {
        let (x,d)=signal.update(p,(fixtures::PRICES.len()-1-k) as u64);
        assert_eq!(d,fixtures::DIRECTIONS[k]);
        for j in 0..54 {
            assert!((x[j]-fixtures::FEATURES[k][j]).abs()<1e-5,"step {k}, feature {j}: {} versus {}",x[j],fixtures::FEATURES[k][j]);
        }
    }
}

#[test]
fn crashes_and_squeezes_have_symmetric_features() {
    let mut up=signal::Signal::default();
    let mut down=signal::Signal::default();
    let path=[0.,0.7,0.7,1.2,1.25,1.2,0.4,0.4,-0.3,-0.3];
    for (k,&p) in path.iter().enumerate() {
        let (x,d)=up.update(p,(path.len()-1-k) as u64);
        let (y,e)=down.update(-p,(path.len()-1-k) as u64);
        if k>0 {
            assert_eq!(d,-e);
            for j in 0..54 {assert!((x[j]-y[j]).abs()<1e-6,"step {k}, feature {j}");}
        }
    }
}

#[path = "../src/risk.rs"]
mod risk;
#[test]
fn drawdown_halt_is_permanent() {
    let mut g=risk::Guard::default();
    assert_eq!(g.observe(Some(10000.),1000.),Some(0.));
    assert_eq!(g.observe(Some(10300.),1000.),Some(0.));
    assert_eq!(g.observe(Some(10000.),1000.),Some(300.));
    assert!(!g.halted);
    assert_eq!(g.observe(Some(9600.),1000.),Some(700.));
    assert!(g.halted);
    assert_eq!(g.observe(Some(10400.),1000.),Some(0.));
    assert!(g.halted);
}
#[test]
fn missing_equity_does_not_corrupt_the_peak() {
    let mut g=risk::Guard {peak:10000.,halted:false};
    assert_eq!(g.observe(None,1000.),None);
    assert_eq!(g.observe(Some(f64::NAN),1000.),None);
    assert_eq!(g.peak,10000.);
}
#[test]
fn another_shock_fits_inside_the_risk_reserve() {
    for mark in [100.,143.11,200.] {
        for dd in (0..=620).step_by(10) {
            let size=risk::capacity(dd as f64,mark,70.,1000.).round();
            assert!((0.0..=70.0).contains(&size));
            assert!(dd as f64+size*mark*0.045<=855.);
        }
    }
    assert_eq!(risk::capacity(900.,143.,70.,1000.),0.);
    assert_eq!(risk::capacity(0.,f64::NAN,70.,1000.),0.);
}

#[path = "../src/model.rs"]
mod model;
#[test]
fn exported_forest_matches_training_predictions() {
    let mut signal=signal::Signal::default();
    for (k,&p) in fixtures::PRICES.iter().enumerate() {
        let (x,_)=signal.update(p,(fixtures::PRICES.len()-1-k) as u64);
        assert!((model::predict(&x)-fixtures::FORECASTS[k]).abs()<1e-9,"step {k}");
    }
}
