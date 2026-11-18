//! The keypair probe bot (bench/checks/keypairs/run.sh) against a planted
//! secret: clean surfaces pass, and each readable one is reported as a leak.

use std::{
    path::{Path, PathBuf},
    process::Command,
};

fn script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../bench/checks/keypairs/run.sh")
}

struct Surfaces {
    root: PathBuf,
}

impl Surfaces {
    fn new(name: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("mtx-probe-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for dir in ["proc/42", "tmp", "tests", "keys"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        std::fs::write(root.join("proc/42/environ"), "PATH=/bin\0HOME=/root\0").unwrap();
        Self { root }
    }

    fn run(&self) -> (bool, String) {
        let out = Command::new("bash")
            .arg(script())
            .env("PROBE_ONCE", "1")
            .env("HOME", self.root.join("tmp/mtx-session-own"))
            .env("PROBE_PROC", self.root.join("proc"))
            .env("PROBE_TMP", self.root.join("tmp"))
            .env("PROBE_VERIFIER", self.root.join("absent-tests"))
            .env("PROBE_LOGS", self.root.join("logs/verifier"))
            .env("PROBE_KEYDIRS", self.root.join("keys"))
            .output()
            .unwrap();
        let log = String::from_utf8_lossy(&out.stdout).to_string();
        (log.contains("leaks=0"), log)
    }
}

impl Drop for Surfaces {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn a_clean_task_passes_and_every_surface_is_reported() {
    let s = Surfaces::new("clean");
    let (clear, log) = s.run();
    assert!(clear, "{log}");
    for surface in ["environ", "verifier", "graderdirs", "keyfiles"] {
        assert!(log.contains(&format!("surface={surface} ")), "{log}");
    }
    assert!(log.contains("surface=verifier status=absent"), "{log}");
}

#[test]
fn the_bots_own_keypair_and_the_snapshot_cache_are_not_leaks() {
    let s = Surfaces::new("own");
    let own = s.root.join("tmp/mtx-session-own");
    std::fs::create_dir_all(&own).unwrap();
    std::fs::create_dir_all(s.root.join("tmp/mtx-snapshots")).unwrap();
    let nums = vec!["7"; 64].join(",");
    std::fs::write(own.join("keypair.json"), format!("[{nums}]")).unwrap();
    let (clear, log) = s.run();
    assert!(clear, "{log}");
}

#[test]
fn a_readable_keypair_file_fails() {
    let s = Surfaces::new("keyfile");
    let nums = vec!["7"; 64].join(",");
    std::fs::write(s.root.join("keys/id.json"), format!("[{nums}]")).unwrap();
    let (clear, log) = s.run();
    assert!(!clear, "{log}");
    assert!(log.contains("surface=keyfiles status=LEAK"), "{log}");
    assert!(
        !log.contains("7,7,7"),
        "the probe must not print the key: {log}"
    );
}

#[test]
fn a_readable_seed_in_another_environment_fails_without_printing_it() {
    let s = Surfaces::new("seed");
    std::fs::write(
        s.root.join("proc/42/environ"),
        "MTX_EVAL_SEED=planted-not-a-real-secret\0",
    )
    .unwrap();
    let (clear, log) = s.run();
    assert!(!clear, "{log}");
    assert!(log.contains("surface=environ status=LEAK"), "{log}");
    assert!(!log.contains("planted-not-a-real-secret"), "{log}");
}

#[test]
fn readable_grader_dirs_and_verifier_files_fail() {
    let s = Surfaces::new("dirs");
    std::fs::create_dir_all(s.root.join("tmp/mtx-informed-logs-x")).unwrap();
    let (clear, log) = s.run();
    assert!(
        !clear && log.contains("surface=graderdirs status=LEAK"),
        "{log}"
    );

    let s = Surfaces::new("tests");
    let out = Command::new("bash")
        .arg(script())
        .env("PROBE_ONCE", "1")
        .env("HOME", s.root.join("tmp/mtx-session-own"))
        .env("PROBE_PROC", s.root.join("proc"))
        .env("PROBE_TMP", s.root.join("tmp"))
        .env("PROBE_VERIFIER", s.root.join("tests"))
        .env("PROBE_LOGS", s.root.join("logs/verifier"))
        .env("PROBE_KEYDIRS", s.root.join("keys"))
        .output()
        .unwrap();
    let log = String::from_utf8_lossy(&out.stdout);
    assert!(log.contains("surface=verifier status=LEAK"), "{log}");
}

#[test]
fn a_readable_reference_log_fails_but_the_bots_own_log_passes() {
    let s = Surfaces::new("logs");
    let bot = s.root.join("logs/verifier/bot");
    std::fs::create_dir_all(&bot).unwrap();
    std::fs::write(bot.join("hidden-1.log"), "slot 1: the bot's own line").unwrap();
    let (clear, log) = s.run();
    assert!(clear, "{log}");
    assert!(log.contains("surface=verifier status=clear"), "{log}");

    std::fs::create_dir_all(bot.join("reference")).unwrap();
    std::fs::write(bot.join("reference/hidden-1.log"), "slot 9: backing off").unwrap();
    let (clear, log) = s.run();
    assert!(!clear, "{log}");
    assert!(log.contains("surface=verifier status=LEAK"), "{log}");
    assert_eq!(log.matches("LEAK verifier").count(), 1, "{log}");
}

#[test]
fn the_seed_in_the_bots_own_environment_fails_without_printing_it() {
    let s = Surfaces::new("ownenv");
    let out = Command::new("bash")
        .arg(script())
        .env("PROBE_ONCE", "1")
        .env("MTX_EVAL_SEED", "planted-own-env-not-a-secret")
        .env("HOME", s.root.join("tmp/mtx-session-own"))
        .env("PROBE_PROC", s.root.join("proc"))
        .env("PROBE_TMP", s.root.join("tmp"))
        .env("PROBE_VERIFIER", s.root.join("absent-tests"))
        .env("PROBE_LOGS", s.root.join("logs/verifier"))
        .env("PROBE_KEYDIRS", s.root.join("keys"))
        .output()
        .unwrap();
    let log = String::from_utf8_lossy(&out.stdout);
    assert!(log.contains("surface=environ status=LEAK"), "{log}");
    assert!(!log.contains("planted-own-env"), "{log}");
}
