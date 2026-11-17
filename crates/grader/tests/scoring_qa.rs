//! Live sessions behind scoring's QA fixes: a bot that does nothing, graded
//! on perp sessions that start holding a position.

use std::path::{Path, PathBuf};

use mtx_grader::{
    grade::{self, GradeOptions, GradeReport},
    keys::SessionKeys,
    orders::NetScoring,
    scenario::{CloseMode, Scenario, StopOut, StopRule, Waypoint},
    scoring_v2::{Ceiling, Params},
};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// A process bot that answers every turn and trades nothing.
fn nop(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join("run.sh"),
        r#"#!/bin/bash
while IFS= read -r line <&3; do
  case "$line" in
    *'"t":"hello"'*) printf '{"t":"ready"}\n' >&3 ;;
    *'"t":"step"'*) k=${line#*\"k\":}; k=${k%%[,\}]*}; printf '{"t":"done","k":%s}\n' "$k" >&3 ;;
    *'"t":"end"'*) exit 0 ;;
  esac
done
"#,
    )
    .unwrap();
}

fn base() -> Scenario {
    Scenario::load(
        &root().join("bench/tasks/flash-crash-spcx/environment/practice/practice-1.json"),
    )
    .unwrap()
}

fn waypoint(slot: u64, level_pct: f64) -> Waypoint {
    Waypoint::at(slot, level_pct)
}

async fn grade_nop(scenarios: &[Scenario], v2_sessions: bool) -> GradeReport {
    let work = std::env::temp_dir().join(format!("mtx-qa-{}", uuid::Uuid::new_v4()));
    let bot = work.join("bot");
    nop(&bot);
    let report = grade::grade(GradeOptions {
        snapshot_dir: &root().join("snapshots/phoenix-spcx-2026-10-29"),
        bot_dir: &bot,
        scenarios,
        log_dir: &work.join("logs"),
        jobs: 1,
        first_bot_user: 0,
        reference_dir: None,
        informed: None,
        anchor_jobs: 1,
        regimes: None,
        stop_on_disqualify: false,
        detail_dir: None,
        annotations: None,
        components_only: false,
        trusted: true,
        observer: None,
        keys: &SessionKeys::from_eval_seed(b"scoring-qa-fixed-secret-0123456789"),
        v2_sessions,
    })
    .await
    .unwrap();
    let _ = std::fs::remove_dir_all(&work);
    report
}

/// SPCX's start mark is about $143: 40 of it is a $5,700 position.
fn holding(path: Vec<Waypoint>, limit: f64) -> Scenario {
    let mut scenario = base();
    scenario.duration_slots = 60;
    scenario.path = path;
    scenario.start_positions.insert("SPCX".into(), 40.0);
    scenario.mandate.limits.max_drawdown_usd = Some(limit);
    scenario
}

/// The deepest the account fell below its running peak, as the bot's steps
/// show it: what the bot itself is told, and what the risk desk reads.
fn stepped_drawdown(report: &GradeReport) -> f64 {
    let session = &report.scenarios[0].session;
    let mut peak = session.start_value_usd;
    let mut deepest = 0.0_f64;
    for step in &session.timeline.steps {
        peak = peak.max(step.value_usd);
        deepest = deepest.max(peak - step.value_usd);
    }
    deepest
}

/// A dip that is over inside one step: the account is never worth less at
/// the end of a step, so the bot never sees it and the risk desk never
/// reads it.
#[tokio::test(flavor = "multi_thread")]
async fn a_dip_inside_one_step_is_no_breach_the_bot_could_have_seen() {
    let dip = vec![
        Waypoint::at(20, 0.0),
        Waypoint::at(21, -3.0),
        Waypoint::at(22, 0.0),
    ];
    let report = grade_nop(&[holding(dip, 100.0)], true).await;
    let session = &report.scenarios[0].session;
    assert!(
        stepped_drawdown(&report) < 100.0,
        "the steps show {}",
        stepped_drawdown(&report)
    );
    assert!(
        (session.max_drawdown_usd - stepped_drawdown(&report)).abs() < 1.0,
        "the audit measured {} where the steps show {}",
        session.max_drawdown_usd,
        stepped_drawdown(&report)
    );
    assert_eq!(report.scenarios[0].disqualified, None);
}

/// A net-scored session scores a bot against holding the start, so a bot
/// that holds scores 0 whatever the price does. It is no breach, then, to
/// hold through a fall deeper than the limit.
#[tokio::test(flavor = "multi_thread")]
async fn holding_the_start_through_a_fall_is_no_drawdown_in_a_net_scored_session() {
    let fall = vec![Waypoint::at(20, 0.0), Waypoint::at(40, -3.0)];
    let mut audited = holding(fall.clone(), 100.0);
    audited.net = Some(NetScoring {
        inventory_bps: 0.0,
        venue_position: None,
    });
    let mut desk = audited.clone();
    desk.name = "desk".into();
    desk.stop_out = Some(StopOut {
        on: vec![StopRule::Drawdown],
        close: CloseMode::Market,
        slice_base: None,
    });
    let report = grade_nop(&[audited, desk], true).await;
    for result in &report.scenarios {
        let session = &result.session;
        assert_eq!(result.disqualified, None, "{}", session.scenario);
        assert!(session.forced_closes.is_empty(), "{}", session.scenario);
        assert!(
            session.pnl_usd.abs() < 5.0,
            "{}: {}",
            session.scenario,
            session.pnl_usd
        );
    }
}

/// A session that starts holding SPCX, scored net of the hold at the fair
/// value, whose book trades at a premium to it at the close: what holding
/// earns is not what the hold baseline takes off.
fn held_at_a_premium() -> Scenario {
    let mut scenario = holding(
        vec![
            Waypoint::at(20, 0.0),
            Waypoint {
                slot: 40,
                level_pct: -3.0,
                premium_pct: 0.5,
            },
        ],
        5000.0,
    );
    scenario.net = Some(NetScoring {
        inventory_bps: 0.0,
        venue_position: None,
    });
    scenario.fair_path = Some(vec![Waypoint::at(20, 0.0), Waypoint::at(40, -3.0)]);
    scenario.reference_pnl_usd = Some(100.0);
    scenario.informed_pnl_usd = Some(1000.0);
    scenario.reference_max_drawdown_usd = Some(0.0);
    scenario.informed_max_drawdown_usd = Some(0.0);
    scenario
}

/// Doing nothing is the 0 of v2's scale: a bot that does nothing scores 0
/// whatever the session started with and wherever its book trades.
#[tokio::test(flavor = "multi_thread")]
async fn a_bot_that_does_nothing_scores_zero_under_v2_on_a_session_that_starts_holding() {
    let mut report = grade_nop(&[held_at_a_premium()], true).await;
    let params = Params::new(Ceiling::Informed { eta_override: None });
    report.score_v2(&params).unwrap();
    let v2 = report.v2.as_ref().unwrap();
    let pnl = report.scenarios[0].session.pnl_usd;
    assert!(
        v2.raw.abs() < 0.005,
        "a bot that did nothing made {pnl} and scored {}",
        v2.raw
    );
}

/// An RFQ desk that starts with the asset: by default the asset's move is
/// the desk's PnL, so a desk that quotes nothing makes or loses it.
#[tokio::test(flavor = "multi_thread")]
async fn a_desk_that_quotes_nothing_scores_zero_under_v2_with_inventory_it_started_with() {
    let mut scenario =
        Scenario::load(&root().join("bench/checks/actors/rfq-dust.json")).unwrap();
    scenario.reference_pnl_usd = Some(100.0);
    scenario.informed_pnl_usd = Some(1000.0);
    scenario.reference_max_drawdown_usd = Some(0.0);
    scenario.informed_max_drawdown_usd = Some(0.0);
    let mut report = grade_nop(&[scenario], true).await;
    let params = Params::new(Ceiling::Informed { eta_override: None });
    report.score_v2(&params).unwrap();
    let v2 = report.v2.as_ref().unwrap();
    let pnl = report.scenarios[0].session.pnl_usd;
    assert!(
        v2.raw.abs() < 0.005,
        "a desk that quoted nothing made {pnl} and scored {}",
        v2.raw
    );
}
