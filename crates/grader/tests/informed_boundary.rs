//! The informed bot on a hidden set (`mtx grade --informed`): it is told
//! each session's latent state and never its future, the bot is told
//! nothing and can't reach what the informed bot was told, and running it
//! leaves the bot's sessions as they were.

use std::path::{Path, PathBuf};

use mtx_grader::{
    generators,
    grade::{self, GradeOptions, Informed},
    keys::SessionKeys,
    scenario::Scenario,
};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// A process bot that prints what it was told, and whether the informed
/// bot's logs are where it could read them, then answers every turn and
/// trades nothing. It looks only for this grader's private directories
/// (`<prefix>-<pid>-<uuid>`, the grader runs in this process): the temp
/// directory is shared with every other grader on the machine, and on a
/// workstation another grader's, running or killed, would be seen too.
fn probe(dir: &Path, logs: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join("run.sh"),
        format!(
            r#"#!/bin/bash
echo "MTX_REGIME=${{MTX_REGIME-<unset>}}"
echo "UID=$(id -u)"
env | grep -q '^MTX_REGIME=' && echo "ENV_HAS_REGIME"
[ -e "{logs}/informed" ] && echo "SAW_INFORMED_LOGS"
for dir in "{tmp}"/mtx-informed-logs-{pid}-*; do
  [ -e "$dir" ] && echo "SAW_PRIVATE_DIR"
  grep -rqs "MTX_REGIME" "$dir" && echo "READ_PRIVATE_LOGS"
done
while IFS= read -r line <&3; do
  case "$line" in
    *'"t":"hello"'*) printf '{{"t":"ready"}}\n' >&3 ;;
    *'"t":"step"'*) k=${{line#*\"k\":}}; k=${{k%%[,\}}]*}}; printf '{{"t":"done","k":%s}}\n' "$k" >&3 ;;
    *'"t":"end"'*) exit 0 ;;
  esac
done
"#,
            logs = logs.display(),
            tmp = std::env::temp_dir().display(),
            pid = std::process::id()
        ),
    )
    .unwrap();
}

async fn run(
    scenarios: &[Scenario],
    work: &Path,
    informed: Option<(&Path, &[String])>,
    keys: &SessionKeys,
) -> (grade::GradeReport, PathBuf) {
    let logs = work.join("logs");
    let bot = work.join("bot");
    probe(&bot, &logs);
    let report = grade::grade(GradeOptions {
        snapshot_dir: &root().join("snapshots/phoenix-spcx-2026-10-29"),
        bot_dir: &bot,
        scenarios,
        log_dir: &logs,
        jobs: 1,
        first_bot_user: 0,
        reference_dir: None,
        informed: informed.map(|(dir, latent)| Informed {
            dir,
            latent: Some(latent),
        }),
        anchor_jobs: 1,
        regimes: None,
        stop_on_disqualify: false,
        detail_dir: None,
        annotations: None,
        components_only: false,
        trusted: true,
        observer: None,
        keys,
        v2_sessions: true,
        host: None,
    })
    .await
    .unwrap();
    (report, logs)
}

/// What a session report says, but the wall clock.
fn session_json(report: &grade::GradeReport) -> Vec<serde_json::Value> {
    report
        .scenarios
        .iter()
        .map(|s| {
            let mut v = serde_json::to_value(&s.session).unwrap();
            strip_wall_clock(&mut v);
            v
        })
        .collect()
}

fn strip_wall_clock(v: &mut serde_json::Value) {
    match v {
        serde_json::Value::Object(map) => {
            map.remove("wall_ms");
            map.remove("turn_ms");
            map.values_mut().for_each(strip_wall_clock);
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(strip_wall_clock),
        _ => {}
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_informed_bot_is_told_the_latent_state_and_the_bot_nothing() {
    let work = std::env::temp_dir().join(format!("mtx-informed-{}", uuid::Uuid::new_v4()));
    // funding-carry: its latent state is the funding process (process
    // facts); its regime adds the session's draws, the episodes. The first
    // 80 slots are enough to tell who was told what.
    let hidden = (generators::generator("funding-carry").unwrap().hidden)(1, 41, true).unwrap();
    let mut scenarios = hidden.scenarios.clone();
    scenarios[0].duration_slots = 80;
    scenarios[0].path.retain(|w| w.slot <= 80);
    scenarios[0].tape.retain(|o| o.slot < 80);
    let regime = hidden.regimes.as_ref().unwrap()[0].clone();
    let latent = hidden.latent.clone();
    let keys = SessionKeys::from_eval_seed(b"informed-boundary");

    let informed_dir = work.join("informed");
    probe(&informed_dir, &work.join("unused"));
    let (with, logs) = run(
        &scenarios,
        &work.join("with"),
        Some((&informed_dir, &latent)),
        &keys.clone().salt_strategy_seeds(),
    )
    .await;
    let name = &scenarios[0].name;

    // The informed bot was told the latent state, and the latent state
    // holds nothing of where the session goes.
    let told = std::fs::read_to_string(logs.join("informed").join(format!("{name}.log"))).unwrap();
    assert!(
        told.contains(&format!("MTX_REGIME={}\n", latent[0])),
        "{told}"
    );
    for future in ["episodes", "premium_pct", "\"kind\"", "\"start\""] {
        assert!(regime.contains(future), "{regime}");
        assert!(!told.contains(future), "{told}");
    }
    assert!(with.scenarios[0].informed_pnl_usd.is_some());
    // The probe works: run while its own logs were in the grader's private
    // directory, the informed bot saw it. Whether it could read what it was
    // told there depends on who it ran as: the directory is the grader's
    // alone (0700), so the grader's own user reads it and the bot user a
    // grader running as root drops to (as in CI and the verifier) must not.
    assert!(told.contains("SAW_PRIVATE_DIR"), "{told}");
    let grader_uid = std::os::unix::fs::MetadataExt::uid(&std::fs::metadata(&work).unwrap());
    let same_user = told.contains(&format!("UID={grader_uid}\n"));
    assert_eq!(told.contains("READ_PRIVATE_LOGS"), same_user, "{told}");

    // The bot was told nothing, and the informed bot's logs weren't there
    // while it ran.
    let bot = std::fs::read_to_string(logs.join(format!("{name}.log"))).unwrap();
    assert!(bot.contains("MTX_REGIME=<unset>\n"), "{bot}");
    // Nor were they anywhere on disk: the grader's private directory was
    // gone before the bot's session started.
    for leak in [
        "ENV_HAS_REGIME",
        "SAW_INFORMED_LOGS",
        "SAW_PRIVATE_DIR",
        "READ_PRIVATE_LOGS",
        &latent[0],
        &regime,
    ] {
        assert!(!bot.contains(leak), "{bot}");
    }

    // Without the informed bot, the bot's session is the same.
    let (without, logs_without) = run(
        &scenarios,
        &work.join("without"),
        None,
        &keys.salt_strategy_seeds(),
    )
    .await;
    assert!(!logs_without.join("informed").exists());
    assert_eq!(session_json(&with), session_json(&without));
    let _ = std::fs::remove_dir_all(&work);
}
