//! The JSON files the repository ships load the way the grader loads them:
//! every task's scenarios, and every example in the specs.

use std::path::{Path, PathBuf};

use mtx_core::Mandate;
use mtx_grader::scenario::Scenario;

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn json_files(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    files.sort();
    files
}

#[test]
fn every_task_scenario_loads() {
    let mut count = 0;
    for task in std::fs::read_dir(root().join("bench/tasks")).unwrap() {
        let task = task.unwrap().path();
        for dir in ["environment/practice", "tests/scenarios"] {
            let dir = task.join(dir);
            if !dir.is_dir() {
                continue;
            }
            for file in json_files(&dir) {
                Scenario::load(&file).unwrap_or_else(|e| panic!("{}: {e}", file.display()));
                count += 1;
            }
        }
    }
    assert!(count > 0, "no task scenarios found");
}

#[test]
fn every_spec_example_is_a_valid_mandate() {
    let files = json_files(&root().join("specs/examples"));
    assert!(!files.is_empty(), "no spec examples found");
    for file in files {
        Mandate::load(&file).unwrap_or_else(|e| panic!("{}: {e}", file.display()));
    }
}

/// Every mandate the tasks ship needs nothing newer than v0, and writes
/// back with the same members and values; every spec example writes back as
/// the same bytes. A v0 reader reads them as before.
#[test]
fn every_shipped_mandate_is_still_v0() {
    let mut count = 0;
    for task in std::fs::read_dir(root().join("bench/tasks")).unwrap() {
        let task = task.unwrap().path();
        for dir in ["environment/practice", "tests/scenarios"] {
            let dir = task.join(dir);
            if !dir.is_dir() {
                continue;
            }
            for file in json_files(&dir) {
                let raw: serde_json::Value =
                    serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
                let scenario = Scenario::load(&file).unwrap();
                let mandate = &scenario.mandate;
                assert_eq!(mandate.minimal_version(), "v0", "{}", file.display());
                assert_eq!(mandate.version, "v0", "{}", file.display());
                assert_eq!(
                    serde_json::to_value(mandate).unwrap(),
                    raw["mandate"],
                    "{}",
                    file.display()
                );
                count += 1;
            }
        }
    }
    assert!(count > 0, "no task scenarios found");
    for file in json_files(&root().join("specs/examples")) {
        let mandate = Mandate::load(&file).unwrap();
        if mandate.version == "v0" {
            assert_eq!(mandate.minimal_version(), "v0", "{}", file.display());
        }
        assert_eq!(
            serde_json::to_string_pretty(&mandate).unwrap() + "\n",
            std::fs::read_to_string(&file).unwrap(),
            "{}",
            file.display()
        );
    }
}
