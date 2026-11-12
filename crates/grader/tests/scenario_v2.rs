//! Scenario files in v1 and v2: every shipped file loads as v1, converts to
//! v2 and back without loss, and a chosen few match their golden v2 files
//! byte for byte (`MTX_BLESS=1 cargo test -p mtx-grader --test scenario_v2`
//! rewrites the goldens).

use std::path::{Path, PathBuf};

use mtx_grader::{
    scenario::Scenario,
    scenario_v2::{ScenarioV2, parse},
};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn scenario_files() -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(root().join("bench/tasks"))
        .unwrap()
        .flat_map(|task| {
            let task = task.unwrap().path();
            [
                task.join("environment/practice"),
                task.join("tests/scenarios"),
            ]
        })
        .collect();
    dirs.extend([
        root().join("bench/checks/spot"),
        root().join("bench/checks/faults"),
    ]);
    for dir in dirs.into_iter().filter(|d| d.is_dir()) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|e| e == "json")
                && std::fs::read_to_string(&path)
                    .unwrap()
                    .contains("\"duration_slots\"")
            {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

#[test]
fn every_shipped_file_survives_the_trip_to_v2() {
    let files = scenario_files();
    assert!(files.len() > 20, "found {} files", files.len());
    for file in files {
        let v1 = Scenario::load(&file).unwrap_or_else(|e| panic!("{}: {e}", file.display()));
        let v2 = ScenarioV2::try_from(&v1).unwrap();
        assert_eq!(v2.validate(), Ok(()), "{}", file.display());
        let text = serde_json::to_string_pretty(&v2).unwrap();
        assert_eq!(parse(&text).unwrap(), v1, "{}", file.display());
        assert_eq!(v2.into_scenario().unwrap(), v1);
    }
}

/// A v1 file of each kind and the v2 file it must write.
const GOLDEN: [(&str, &str); 6] = [
    (
        "bench/tasks/flash-crash-spcx/environment/practice/practice-1.json",
        "flash-crash.json",
    ),
    (
        "bench/tasks/market-making-spcx/environment/practice/practice-1.json",
        "market-making.json",
    ),
    (
        "bench/tasks/weekend-gap-spcx/environment/practice/practice-1.json",
        "weekend-gap.json",
    ),
    (
        "bench/tasks/funding-carry-spcx/environment/practice/practice-1.json",
        "funding-carry.json",
    ),
    ("bench/checks/spot/smoke-buy-order.json", "spot-order.json"),
    ("bench/checks/spot/smoke-net.json", "spot-net.json"),
];

#[test]
fn golden_v2_files_are_what_v1_files_write() {
    let golden = root().join("crates/grader/tests/golden/scenario_v2");
    for (source, name) in GOLDEN {
        let v1 = Scenario::load(&root().join(source)).unwrap();
        let tmp = std::env::temp_dir().join(format!("mtx-v2-{}-{name}", std::process::id()));
        v1.save_v2(&tmp).unwrap();
        let written = std::fs::read_to_string(&tmp).unwrap();
        std::fs::remove_file(&tmp).unwrap();
        if std::env::var_os("MTX_BLESS").is_some() {
            std::fs::create_dir_all(&golden).unwrap();
            std::fs::write(golden.join(name), &written).unwrap();
        }
        let expected = std::fs::read_to_string(golden.join(name))
            .unwrap_or_else(|e| panic!("{name}: {e}; bless it"));
        assert_eq!(written, expected, "{name}");
        assert_eq!(Scenario::load(&golden.join(name)).unwrap(), v1, "{name}");
    }
}

/// A perp scenario whose oracle is written by feeds: no shipped file has
/// one yet, so the v1 side is built here from a shipped file.
#[test]
fn golden_v2_oracles_file_is_what_a_v1_scenario_with_feeds_writes() {
    use mtx_grader::oracles::{
        Failure, Feed, Level, Mode, Oracles, PhoenixOracle, PythOracle, Source, Window,
    };
    let mut v1 = Scenario::load(
        &root().join("bench/tasks/flash-crash-spcx/environment/practice/practice-1.json"),
    )
    .unwrap();
    v1.path.clear();
    v1.oracles = Some(Oracles {
        seed: 7,
        feeds: vec![
            Feed {
                name: "spcx".into(),
                source: Source::Phoenix(PhoenixOracle {
                    symbol: v1.symbol.clone(),
                }),
                cadence_slots: 8,
                path: vec![Level {
                    slot: 400,
                    level_pct: -6.0,
                }],
                conf_bps: 2.0,
                failures: vec![Failure {
                    mode: Mode::Stale,
                    window: Window {
                        from_slot: 100,
                        from_slot_max: Some(200),
                        slots: Some(40),
                    },
                }],
            },
            Feed {
                name: "sol".into(),
                source: Source::Pyth(PythOracle {
                    feed_id: "ef0d8b6fda2ceba41da15d4095d1da392a0d2f8ed0c6c7bc0f4cfac8c280b56d"
                        .into(),
                    shard: 0,
                    exponent: -8,
                    account: None,
                }),
                cadence_slots: 4,
                path: Vec::new(),
                conf_bps: 2.0,
                failures: vec![Failure {
                    mode: Mode::OffPrice { bias_pct: 3.0 },
                    window: Window {
                        from_slot: 300,
                        from_slot_max: None,
                        slots: None,
                    },
                }],
            },
        ],
    });
    assert_eq!(v1.validate(), Ok(()));
    let name = "oracles.json";
    let golden = root().join("crates/grader/tests/golden/scenario_v2");
    let tmp = std::env::temp_dir().join(format!("mtx-v2-{}-{name}", std::process::id()));
    v1.save_v2(&tmp).unwrap();
    let written = std::fs::read_to_string(&tmp).unwrap();
    std::fs::remove_file(&tmp).unwrap();
    if std::env::var_os("MTX_BLESS").is_some() {
        std::fs::write(golden.join(name), &written).unwrap();
    }
    let expected = std::fs::read_to_string(golden.join(name))
        .unwrap_or_else(|e| panic!("{name}: {e}; bless it"));
    assert_eq!(written, expected);
    assert_eq!(Scenario::load(&golden.join(name)).unwrap(), v1);
    assert_eq!(
        ScenarioV2::try_from(&v1).unwrap().into_scenario().unwrap(),
        v1
    );
}

/// A perp scenario whose path is drawn from a market calibration, so it
/// carries `market_model`'s record: no shipped file has one yet, so the v1
/// side is drawn here from the committed SPCX calibration.
#[test]
fn golden_v2_market_model_file_is_what_a_drawn_v1_scenario_writes() {
    use mtx_grader::market::{Calibration, MarketModel, ModelSpec};
    let calibration = Calibration::load(
        &root().join("calibration/market/spcx.20261112.d1f48be2.toml"),
    )
    .unwrap();
    let model = MarketModel::new(calibration, ModelSpec::default()).unwrap();
    let mut template = Scenario::load(
        &root().join("bench/tasks/flash-crash-spcx/environment/practice/practice-1.json"),
    )
    .unwrap();
    template.path.clear();
    let v1 = model.scenario(&template, "drawn-1", 7).unwrap();
    assert!(v1.market_model.is_some());
    let name = "market-model.json";
    let golden = root().join("crates/grader/tests/golden/scenario_v2");
    let tmp = std::env::temp_dir().join(format!("mtx-v2-{}-{name}", std::process::id()));
    v1.save_v2(&tmp).unwrap();
    let written = std::fs::read_to_string(&tmp).unwrap();
    std::fs::remove_file(&tmp).unwrap();
    if std::env::var_os("MTX_BLESS").is_some() {
        std::fs::write(golden.join(name), &written).unwrap();
    }
    let expected = std::fs::read_to_string(golden.join(name))
        .unwrap_or_else(|e| panic!("{name}: {e}; bless it"));
    assert_eq!(written, expected);
    assert_eq!(Scenario::load(&golden.join(name)).unwrap(), v1);
    assert_eq!(
        ScenarioV2::try_from(&v1).unwrap().into_scenario().unwrap(),
        v1
    );
}
