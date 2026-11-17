//! A drawn hidden set is stratified, and which session falls where is
//! chance: the regime of a session never depends on its index, which a bot
//! could read off its session's name.

use mtx_grader::{
    carry::CarryFamily,
    making::MakingFamily,
    scenario::{Aftermath, ShockFamily},
    weekend::WeekendFamily,
};

const SETS: u64 = 300;
const SESSIONS: usize = 72;

/// How often `at(set)` holds for the first and the last session of
/// `SETS` sets of `SESSIONS`.
fn first_and_last(at: impl Fn(u64) -> Vec<bool>) -> (f64, f64) {
    let (mut first, mut last) = (0, 0);
    for seed in 1..=SETS {
        let flags = at(seed);
        assert_eq!(flags.len(), SESSIONS);
        first += usize::from(flags[0]);
        last += usize::from(flags[SESSIONS - 1]);
    }
    (first as f64 / SETS as f64, last as f64 / SETS as f64)
}

fn assert_same_share(family: &str, (first, last): (f64, f64)) {
    assert!(
        (first - last).abs() < 0.1,
        "{family}: the share is {first:.2} at the first session and {last:.2} at the last"
    );
}

#[test]
fn flash_crash_extends_are_as_likely_first_as_last() {
    let shares = first_and_last(|seed| {
        ShockFamily::spcx()
            .draw_set("h", SESSIONS, seed)
            .iter()
            .map(|(_, shape)| shape.aftermath == Aftermath::Extend)
            .collect()
    });
    assert_same_share("flash-crash extend", shares);
}

#[test]
fn weekend_gap_informed_days_are_as_likely_first_as_last() {
    let shares = first_and_last(|seed| {
        WeekendFamily::spcx()
            .draw_set("h", SESSIONS, seed)
            .iter()
            .map(|(_, shape)| shape.informed)
            .collect()
    });
    assert_same_share("weekend-gap informed", shares);
}

#[test]
fn market_making_jump_days_are_as_likely_first_as_last() {
    let shares = first_and_last(|seed| {
        MakingFamily::spcx()
            .sample_set_labeled("h", SESSIONS, seed)
            .iter()
            .map(|(_, informed)| informed.iter().any(|i| *i))
            .collect()
    });
    assert_same_share("market-making jumps", shares);
}

#[test]
fn a_set_is_still_stratified_and_named_by_its_index() {
    let carry = CarryFamily::spcx().draw_set("h", SESSIONS, 7);
    let names: Vec<_> = carry.iter().map(|(s, _)| s.name.clone()).collect();
    let want: Vec<_> = (1..=SESSIONS).map(|i| format!("h-{i}")).collect();
    assert_eq!(names, want);
    let extends = |seed| {
        ShockFamily::spcx()
            .draw_set("h", SESSIONS, seed)
            .iter()
            .filter(|(_, shape)| shape.aftermath == Aftermath::Extend)
            .count()
    };
    // The same count whichever way the seed falls, and the same set twice.
    assert_eq!(extends(7), extends(8));
    assert_eq!(
        ShockFamily::spcx().draw_set("h", SESSIONS, 7),
        ShockFamily::spcx().draw_set("h", SESSIONS, 7)
    );
}
