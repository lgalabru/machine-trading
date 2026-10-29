//! The JSON files the repository ships load the way the grader loads them:
//! every task's scenarios, and every example in the specs.

use std::path::{Path, PathBuf};

use mth_botkit::Mandate;
use mth_core::scenario::Scenario;

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
